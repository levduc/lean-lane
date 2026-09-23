//! The stage/anchor race (2026-09-15).
//!
//! The CL fires `arc_stageBlock` and then, ~15 ms later, `arc_newBlock
//! {commitment}` for the SAME block — while the stage is still running. On the
//! fleet (400 M budget, 2.2 MB / 767-tx blocks) that happened on ~35 % of
//! heights: the anchor found nothing in the index, nothing staged, nothing
//! queued, and pulled the block from a peer over HTTP for 200-250 ms, on the
//! consensus critical path, for a block that was landing in this process's own
//! memory milliseconds later.
//!
//! Two windows have to be closed, and both are gated here:
//!   1. the stage is RUNNING and has registered its commitment in `inflight`;
//!   2. the stage has ARRIVED at the RPC layer but has not named itself yet
//!      (JSON parse + base64 decode of ~3 MB, then the wire hash) — covered by
//!      `stage_pending` + `stage_notify`.

use alloy_primitives::{Address, B256};
use lean_lane_node::chain::LeanBlock;
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome, StageOutcome};
use lean_lane_node::state::Acct;
use lean_native::envelope::{current_lane_domain, LeanSigned};
use lean_native::{lean_fee, LeanTx, Output, AMOUNT_UNIT};
use secp256k1::SecretKey;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

fn node(dir: &std::path::Path) -> Arc<LaneNode> {
    Arc::new(
        LaneNode::open(Config {
            datadir: dir.to_path_buf(),
            ..Default::default()
        })
        .unwrap()
        .0,
    )
}

/// A signed fan-out tx plus the funding its sender needs.
async fn funded_tx(n: &LaneNode, key: u64, nonce: u32, outputs: usize) -> Vec<u8> {
    let domain = current_lane_domain();
    let outs = (0..outputs)
        .map(|i| Output {
            to: Address::with_last_byte((i % 200) as u8),
            amount: 3,
        })
        .collect();
    let signed = LeanSigned::new(LeanTx::sign(nonce, outs, &sk(key), &domain));
    let sender = signed.tx().recover_sender(&domain).unwrap();
    n.seed_account(
        sender,
        Acct {
            nonce: 0,
            balance: 100 * (lean_fee(outputs) + outputs as u128 * 3 * AMOUNT_UNIT),
        },
    )
    .await;
    signed.raw().to_vec()
}

/// Build head+1 WITHOUT going through `shim_build` — `arc_buildBlock` stages
/// its own result, which would hand the anchor a staged entry before the race
/// under test could happen.
async fn wire_head_plus_one(n: &LaneNode, key: u64, txs_per_block: usize) -> (B256, Vec<u8>) {
    let (head_c, head_n, ts) = *n.head.lock().await;
    let mut txs = Vec::new();
    for i in 0..txs_per_block {
        txs.push(funded_tx(n, key + i as u64, 0, 4).await);
    }
    let b = LeanBlock::new(head_c, head_n + 1, ts + 1000, txs);
    (b.commitment, b.to_wire_bytes())
}

/// How many blocks the log actually holds — the "appended once" gate. A second
/// append of the same block would be rejected by the parent link and leave the
/// head where it is, so head alone cannot tell a double-append from a no-op.
async fn log_len(n: &LaneNode) -> u64 {
    let mut len = 0;
    let mut log = n.log.lock().await;
    while log.read_block(len + 1).unwrap().is_some() {
        len += 1;
    }
    len
}

// ---------------------------------------------------------------------------
// the wire-hash refactor `stage_block` leans on
// ---------------------------------------------------------------------------

/// `commitment_of_wire` must agree with a full decode for every shape of
/// block, or `stage_block` would claim `inflight` under a key the anchor never
/// asks for — a silent, permanent regression to the peer-pull path.
#[tokio::test(flavor = "multi_thread")]
async fn commitment_of_wire_matches_a_full_decode() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    for txs_per_block in [0usize, 1, 5, 40] {
        let (c, wire) = wire_head_plus_one(&n, txs_per_block as u64 * 100, txs_per_block).await;
        let h = LeanBlock::commitment_of_wire(&wire).unwrap();
        assert_eq!(h.commitment, c, "{txs_per_block} txs");
        let full = LeanBlock::from_wire_bytes(&wire).unwrap();
        assert_eq!(full.commitment, c);
        assert_eq!(h.parent, full.parent);
        assert_eq!(h.number, full.number);
        assert_eq!(h.timestamp_ms, full.timestamp_ms);
        assert_eq!(full.to_wire_bytes(), wire);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn commitment_of_wire_rejects_malformed_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let (_, wire) = wire_head_plus_one(&n, 7, 3).await;
    assert!(LeanBlock::commitment_of_wire(&wire[..20]).is_err(), "short");
    assert!(
        LeanBlock::commitment_of_wire(&wire[..wire.len() - 5]).is_err(),
        "truncated body"
    );
    let mut trailing = wire.clone();
    trailing.push(0);
    assert!(
        LeanBlock::commitment_of_wire(&trailing).is_err(),
        "trailing"
    );
    // and a bad body must never leave a claim behind
    assert!(n.stage_block(&trailing).await.is_err());
    assert!(n.inflight.lock().await.is_empty());
}

// ---------------------------------------------------------------------------
// window 1: the stage is running and has named itself
// ---------------------------------------------------------------------------

/// Stage and anchor issued concurrently: the anchor must take the STAGED path
/// (no peer fetch) and the block must be appended exactly once.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_stage_and_anchor_take_the_staged_path() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let (c, wire) = wire_head_plus_one(&n, 1, 24).await;

    let staging = {
        let n = n.clone();
        let wire = wire.clone();
        tokio::spawn(async move { n.stage_block(&wire).await })
    };
    tokio::time::sleep(Duration::from_millis(3)).await;
    let r = n.shim_new_by_commitment(c).await.unwrap();
    assert_eq!(r, NewBlockOutcome::Valid(c));
    assert!(matches!(staging.await.unwrap().unwrap(), StageOutcome::Staged(x) if x == c));

    // no peer was asked (the node has none, so a pull would have SYNCED)
    assert_eq!(n.metrics.block_pulls.load(Ordering::Relaxed), 0);
    assert_eq!(n.head.lock().await.1, 1);
    assert_eq!(log_len(&n).await, 1, "appended exactly once");
    // and no claim survives the round
    assert!(n.inflight.lock().await.is_empty());
}

// ---------------------------------------------------------------------------
// window 2: the stage has arrived but has not named itself yet
// ---------------------------------------------------------------------------

/// THE REGRESSION GATE. The anchor arrives while the stage is still in the RPC
/// layer — parsed nothing, claimed nothing. Before the fix this fell straight
/// through to the peer pull (`wait_inflight_ms=0.00`, `path=peer`, 200-250 ms);
/// now it waits on `stage_pending` and promotes the staged entry.
///
/// The RPC layer's `StagePending` guard is simulated here with the same two
/// atomics it touches, so the test does not need a server socket.
#[tokio::test(flavor = "multi_thread")]
async fn anchor_waits_for_a_stage_that_has_not_registered_yet() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let (c, wire) = wire_head_plus_one(&n, 1, 24).await;

    // the RPC handler has the request but has not decoded it yet
    n.stage_pending.fetch_add(1, Ordering::Relaxed);

    let staging = {
        let n = n.clone();
        let wire = wire.clone();
        tokio::spawn(async move {
            // ... spent parsing ~3 MB of JSON + base64, well past the ~15 ms
            // the CL waits before firing the anchor
            tokio::time::sleep(Duration::from_millis(20)).await;
            let r = n.stage_block(&wire).await;
            n.stage_pending.fetch_sub(1, Ordering::Relaxed);
            n.stage_notify.notify_waiters();
            r
        })
    };

    let t = std::time::Instant::now();
    let r = n.shim_new_by_commitment(c).await.unwrap();
    assert_eq!(
        r,
        NewBlockOutcome::Valid(c),
        "anchor must not SYNC past its own stage"
    );
    assert!(
        t.elapsed() >= Duration::from_millis(20),
        "it really did wait"
    );
    assert!(matches!(staging.await.unwrap().unwrap(), StageOutcome::Staged(x) if x == c));

    assert!(
        n.metrics.stage_race_waits.load(Ordering::Relaxed) >= 1,
        "the wait is counted"
    );
    assert_eq!(
        n.metrics.block_pulls.load(Ordering::Relaxed),
        0,
        "no peer fetch"
    );
    assert_eq!(n.head.lock().await.1, 1);
    assert_eq!(log_len(&n).await, 1, "appended exactly once");
    assert!(n.inflight.lock().await.is_empty());
}

/// THIS grace is gated on `stage_pending`: with no stage in flight a
/// by-commitment anchor must not sit out the stage-race grace, or every
/// genuine backfill would pay it on top of its pull.
///
/// The anchor does still wait here — for `anchor_grace_ms`, the COLD
/// grace added later for the case where the stage has not even arrived (see
/// `anchor_grace.rs`). What this test gates is that `stage_race_waits` stays
/// at zero: the two graces are separate windows with separate counters, and
/// the warm one must not fire on an empty node.
#[tokio::test(flavor = "multi_thread")]
async fn no_stage_pending_means_no_stage_race_wait() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let r = n
        .shim_new_by_commitment(B256::repeat_byte(0xab))
        .await
        .unwrap();
    assert!(matches!(r, NewBlockOutcome::Syncing { head: 0 }));
    assert_eq!(n.metrics.stage_race_waits.load(Ordering::Relaxed), 0);
}

// ---------------------------------------------------------------------------
// a stage that fails must never wedge the anchor
// ---------------------------------------------------------------------------

/// The stage bails on the head check (bad parent) and so never produces a
/// staged entry. The anchor waiting on its claim must be released and answer
/// SYNCING — not hang on the 2 s guard, and not hang forever.
#[tokio::test(flavor = "multi_thread")]
async fn anchor_for_a_failing_stage_returns_without_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    // parent that is not our head: `stage_block` must answer SYNCING
    let tx = funded_tx(&n, 5, 0, 4).await;
    let orphan = LeanBlock::new(B256::repeat_byte(0x7e), 1, 1_000, vec![tx]);
    let (c, wire) = (orphan.commitment, orphan.to_wire_bytes());

    n.stage_pending.fetch_add(1, Ordering::Relaxed);
    let staging = {
        let n = n.clone();
        let wire = wire.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let r = n.stage_block(&wire).await;
            n.stage_pending.fetch_sub(1, Ordering::Relaxed);
            n.stage_notify.notify_waiters();
            r
        })
    };

    let t = std::time::Instant::now();
    let r = tokio::time::timeout(Duration::from_secs(5), n.shim_new_by_commitment(c))
        .await
        .expect("anchor hung on a failed stage")
        .unwrap();
    assert!(
        matches!(r, NewBlockOutcome::Syncing { head: 0 }),
        "got {r:?}"
    );
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "took {:?}",
        t.elapsed()
    );
    assert!(matches!(
        staging.await.unwrap().unwrap(),
        StageOutcome::Syncing { head: 0 }
    ));
    // the failed stage released its claim (a leak would park the NEXT anchor
    // for the full 2 s guard)
    assert!(n.inflight.lock().await.is_empty());
    assert_eq!(n.head.lock().await.1, 0);
}

/// Same, but the anchor arrives after the failing stage has already come and
/// gone: nothing is left behind to wait on.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_stage_leaves_no_claim() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let tx = funded_tx(&n, 6, 0, 4).await;
    let orphan = LeanBlock::new(B256::repeat_byte(0x11), 9, 1_000, vec![tx]);
    assert!(matches!(
        n.stage_block(&orphan.to_wire_bytes()).await.unwrap(),
        StageOutcome::Syncing { head: 0 }
    ));
    assert!(n.inflight.lock().await.is_empty());
    assert!(n.staged.lock().await.is_empty());

    // (the anchor still sits out the COLD `anchor_grace_ms` here — no
    // stage is coming, but it cannot know that; see `anchor_grace.rs`)
    let t = std::time::Instant::now();
    let r = n.shim_new_by_commitment(orphan.commitment).await.unwrap();
    assert!(matches!(r, NewBlockOutcome::Syncing { head: 0 }));
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "took {:?}",
        t.elapsed()
    );
}

/// A duplicate stage of a block already staged stays the cheap no-op it was,
/// and does not steal or clobber the first stage's claim.
#[tokio::test(flavor = "multi_thread")]
async fn duplicate_stage_is_still_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let (c, wire) = wire_head_plus_one(&n, 40, 6).await;
    assert_eq!(n.stage_block(&wire).await.unwrap(), StageOutcome::Staged(c));
    assert_eq!(n.stage_block(&wire).await.unwrap(), StageOutcome::Staged(c));
    assert_eq!(n.staged.lock().await.len(), 1);
    assert!(n.inflight.lock().await.is_empty());
    assert_eq!(
        n.shim_new_by_commitment(c).await.unwrap(),
        NewBlockOutcome::Valid(c)
    );
    assert_eq!(log_len(&n).await, 1);
}

// ---------------------------------------------------------------------------
// claim OWNERSHIP: a stalled holder must not release someone else's claim
// ---------------------------------------------------------------------------

/// Records physically in `blocks.log`, counted off the length-prefix framing.
/// `log_len` above counts through `read_block`, whose index is keyed by NUMBER,
/// so it cannot see a block appended twice — which is the whole failure here.
fn records_on_disk(dir: &std::path::Path) -> usize {
    let bytes = std::fs::read(dir.join("blocks.log")).unwrap();
    let (mut at, mut n) = (0usize, 0usize);
    while at + 4 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let end = at + 4 + len + 4;
        if end > bytes.len() {
            break;
        }
        n += 1;
        at = end;
    }
    n
}

/// THE I3 GATE. Claims used to be keyed on the commitment alone, with an
/// unconditional release. A stage that stalled past the 2 s guard and had its
/// claim taken over by a committer would, on finishing, delete THE COMMITTER'S
/// claim — and a third feed of the same bytes arriving in that window sees no
/// claim, still passes the parent-link check (the head has not moved, the
/// committer is still inside `commit_block`) and appends the same block a
/// second time. The state converges (total-STF), but the log diverges
/// byte-for-byte from every other validator's, which is exactly the invariant
/// the by-commitment serving path rests on.
///
/// Here the committer is parked on the `state` lock — a slow commit — with its
/// claim live, and the stalled stage's release is replayed against it.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_release_cannot_hand_a_second_committer_the_same_block() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let (c, wire) = wire_head_plus_one(&n, 300, 8).await;

    // hold the state lock: whoever reaches `commit_block` parks there
    let state_guard = n.state.lock().await;
    let committer = {
        let n = n.clone();
        let wire = wire.clone();
        tokio::spawn(async move { n.shim_new(&wire).await })
    };
    let mut held = false;
    for _ in 0..400 {
        if n.inflight.lock().await.contains_key(&c) {
            held = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(held, "the committer must hold the claim before we race it");

    // the stalled stage it took the claim from finishes NOW and releases
    assert!(
        !n.release_claim(c, u64::MAX).await,
        "a claim we do not own must not be released"
    );
    assert!(
        n.inflight.lock().await.contains_key(&c),
        "the committer still holds its claim"
    );

    // a third feed of the same bytes, while the commit is still in flight
    let third = {
        let n = n.clone();
        let wire = wire.clone();
        tokio::spawn(async move { n.shim_new(&wire).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(state_guard);

    assert_eq!(committer.await.unwrap().unwrap(), NewBlockOutcome::Valid(c));
    // the third feed either acknowledges the block or SYNCs — never appends it
    let r3 = third.await.unwrap().unwrap();
    let ok = matches!(r3, NewBlockOutcome::Valid(x) if x == c)
        || matches!(r3, NewBlockOutcome::Syncing { .. });
    assert!(ok, "got {r3:?}");
    assert_eq!(n.head.lock().await.1, 1);
    assert_eq!(
        records_on_disk(dir.path()),
        1,
        "the block is on disk exactly once"
    );
    assert!(n.inflight.lock().await.is_empty());
}

// ---------------------------------------------------------------------------
// F3: the append is linkage-safe at the LAST MOMENT, not at the caller's
// ---------------------------------------------------------------------------

/// Build a block on an ARBITRARY parent. `wire_head_plus_one` reads the head,
/// which cannot produce head+2 before head+1 exists — and the queue/drain
/// race needs exactly that.
async fn wire_child(
    n: &LaneNode,
    parent: B256,
    number: u64,
    timestamp_ms: u64,
    key: u64,
    txs_per_block: usize,
) -> (B256, Vec<u8>) {
    let mut txs = Vec::new();
    for i in 0..txs_per_block {
        txs.push(funded_tx(n, key + i as u64, 0, 4).await);
    }
    let b = LeanBlock::new(parent, number, timestamp_ms, txs);
    (b.commitment, b.to_wire_bytes())
}

/// THE F3 GATE. The sync-queue drain commits with NO claim at all, so claim
/// discipline — the only mutual exclusion the commit path had — does not cover
/// it. It re-reads the head before committing a queued child, but that read
/// and the head write inside `commit_block` are a whole commit apart, and a
/// by-bytes feed of the same block arriving in that window takes the free
/// claim and commits it concurrently: two identical records in `blocks.log`.
/// The state converges either way (total-STF), which is what makes this
/// invisible to every other check the node runs.
///
/// Staged deterministically: the committer of b1 is parked on `sync_queue` —
/// the first lock its drain takes, after b1 is appended and the head written —
/// so b1 is canonical and b2 is feedable by bytes while its drain has not
/// started; then the `state` lock is taken, so the drain parks inside
/// `commit_block(b2)` with the link check already passed, which is precisely
/// the window.
#[tokio::test(flavor = "multi_thread")]
async fn a_drain_racing_a_by_bytes_feed_appends_the_block_once() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let (c1, w1) = wire_head_plus_one(&n, 500, 4).await;
    let ts = n.head.lock().await.2;
    let (c2, w2) = wire_child(&n, c1, 2, ts + 2_000, 600, 4).await;

    // b2 first: it does not connect yet, so it is QUEUED behind b1 and the
    // commit of b1 will drain it.
    let q = n.shim_new(&w2).await.unwrap();
    assert!(matches!(q, NewBlockOutcome::Syncing { .. }), "got {q:?}");
    assert_eq!(n.sync_queue.lock().await.len(), 1);

    // park the b1 feed at its drain, after the head write
    let queue_guard = n.sync_queue.lock().await;
    let a = {
        let (n, w1) = (n.clone(), w1.clone());
        tokio::spawn(async move { n.shim_new(&w1).await })
    };
    let mut at_head = false;
    for _ in 0..400 {
        if n.head.lock().await.1 == 1 {
            at_head = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // b1 canonical is what makes the drain race possible at all
    assert!(at_head, "b1 never became the head");

    // take `state` BEFORE releasing `sync_queue`, so the drain cannot get past
    // `commit_block`'s apply
    let state_guard = n.state.lock().await;
    drop(queue_guard);

    // the drain removes b2 from the queue as its first act: that is the signal
    // that it is committed to committing b2
    let mut draining = false;
    for _ in 0..400 {
        if n.sync_queue.lock().await.is_empty() {
            draining = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(draining, "the drain must have taken b2 out of the queue");

    // ...and now the same block by bytes, claim free, head still saying 1
    let b = {
        let (n, w2) = (n.clone(), w2.clone());
        tokio::spawn(async move { n.shim_new(&w2).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(state_guard);

    assert_eq!(a.await.unwrap().unwrap(), NewBlockOutcome::Valid(c1));
    let rb = b.await.unwrap().unwrap();
    let ok = matches!(rb, NewBlockOutcome::Valid(x) if x == c2)
        || matches!(rb, NewBlockOutcome::Syncing { .. });
    assert!(ok, "got {rb:?}");
    assert_eq!(n.head.lock().await.1, 2);
    assert_eq!(records_on_disk(dir.path()), 2, "b1 and b2, once each");
    assert_eq!(log_len(&n).await, 2);
    assert!(n.inflight.lock().await.is_empty());
}

/// The idempotent no-op above must stay narrow: same NUMBER is not enough,
/// the commitment has to match. A different block at a height we already hold
/// is still the loud fork error it always was.
#[tokio::test(flavor = "multi_thread")]
async fn a_conflicting_block_at_a_known_height_is_still_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let (genesis_c, _, genesis_ts) = *n.head.lock().await;
    let (c1, w1) = wire_head_plus_one(&n, 700, 3).await;
    assert_eq!(n.shim_new(&w1).await.unwrap(), NewBlockOutcome::Valid(c1));
    // re-feeding the very same bytes is the idempotent VALID
    assert_eq!(n.shim_new(&w1).await.unwrap(), NewBlockOutcome::Valid(c1));
    assert_eq!(n.head.lock().await.0, c1);

    // a FORK of height 1: same parent, same number, different body
    let (c1b, w1b) = wire_child(&n, genesis_c, 1, genesis_ts + 1_000, 800, 3).await;
    assert_ne!(c1b, c1);
    let e = n.shim_new(&w1b).await.unwrap_err();
    assert!(e.contains("conflicting"), "{e}");
    assert_eq!(records_on_disk(dir.path()), 1, "the fork was not appended");
    assert_eq!(n.head.lock().await.0, c1);
}

// ---------------------------------------------------------------------------
// (head, state) atomicity: a staged entry's parent and its post-state
// ---------------------------------------------------------------------------

/// THE ATOMICITY GATE. `stage_claimed` read the head in one critical section
/// and cloned the state in another. A commit landing in between produced an
/// entry whose `staged_parent` was the OLD head and whose post-state was built
/// on the NEW one — a block executed against a parent it does not descend
/// from, kept in the staged map and servable by commitment. The pairing is
/// exactly what `promote_staged` trusts instead of re-executing.
///
/// Driven deterministically. The stage is parked on the `staged` lock, which
/// its already-staged probe takes and which sits precisely between the head
/// read and the state clone. The commit that advances the head past it is a
/// sync-queue DRAIN, because the drain reaches `commit_block` without taking
/// `staged` first (it takes it only in the retention sweep, after the head
/// write), whereas `shim_new` would park alongside the stage.
///
/// Set-up: head P at height 1, C (child of P) queued behind it, and the feed
/// of P parked at its drain on `sync_queue`. The stage is of B, a DIFFERENT
/// child of P; releasing the queue commits C under the parked stage.
#[tokio::test(flavor = "multi_thread")]
async fn a_commit_between_the_head_read_and_the_state_clone_stages_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let (cp, wp) = wire_head_plus_one(&n, 1_000, 4).await;
    let ts = n.head.lock().await.2;
    let (cc, wc) = wire_child(&n, cp, 2, ts + 2_000, 2_000, 4).await;
    let (cb, wb) = wire_child(&n, cp, 2, ts + 3_000, 3_000, 4).await;
    assert_ne!(cb, cc);

    // C does not connect yet: queued behind P
    let q = n.shim_new(&wc).await.unwrap();
    assert!(matches!(q, NewBlockOutcome::Syncing { .. }), "got {q:?}");

    // feed P, parked at its drain once P is canonical and its claim released
    let queue_guard = n.sync_queue.lock().await;
    let feeder = {
        let (n, wp) = (n.clone(), wp.clone());
        tokio::spawn(async move { n.shim_new(&wp).await })
    };
    let mut parked = false;
    for _ in 0..400 {
        if n.head.lock().await.1 == 1 && !n.inflight.lock().await.contains_key(&cp) {
            parked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(parked, "P never became the head");

    // park whoever asks for `staged` next, then stage B on head P
    let staged_guard = n.staged.lock().await;
    let staging = {
        let (n, wb) = (n.clone(), wb.clone());
        tokio::spawn(async move { n.stage_block(&wb).await })
    };
    let mut claimed = false;
    for _ in 0..400 {
        if n.inflight.lock().await.contains_key(&cb) {
            claimed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(claimed, "the stage never registered its claim");
    // ...and is now past its head read, parked on `staged`
    tokio::time::sleep(Duration::from_millis(25)).await;

    // release the drain: it commits C and moves the head under the stage
    drop(queue_guard);
    let mut moved = false;
    for _ in 0..400 {
        if n.head.lock().await.1 == 2 {
            moved = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(moved, "the drain never advanced the head");
    drop(staged_guard);

    assert_eq!(feeder.await.unwrap().unwrap(), NewBlockOutcome::Valid(cp));
    // the head moved under the stage: it must refuse, not stage a post-state
    // of a parent it never re-checked
    assert_eq!(
        staging.await.unwrap().unwrap(),
        StageOutcome::Syncing { head: 2 }
    );
    assert!(
        !n.staged.lock().await.iter().any(|(c, _)| *c == cb),
        "staged an entry whose post-state is not its parent's",
    );
    assert_eq!(n.head.lock().await.0, cc);
    assert!(n.inflight.lock().await.is_empty());
}
