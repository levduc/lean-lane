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
    Arc::new(LaneNode::open(Config { datadir: dir.to_path_buf(), ..Default::default() }).unwrap().0)
}

/// A signed fan-out tx plus the funding its sender needs.
async fn funded_tx(n: &LaneNode, key: u64, nonce: u32, outputs: usize) -> Vec<u8> {
    let domain = current_lane_domain();
    let outs = (0..outputs)
        .map(|i| Output { to: Address::with_last_byte((i % 200) as u8), amount: 3 })
        .collect();
    let signed = LeanSigned::new(LeanTx::sign(nonce, outs, &sk(key), &domain));
    let sender = signed.tx().recover_sender(&domain).unwrap();
    n.seed_account(
        sender,
        Acct { nonce: 0, balance: 100 * (lean_fee(outputs) + outputs as u128 * 3 * AMOUNT_UNIT) },
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
    assert!(LeanBlock::commitment_of_wire(&wire[..wire.len() - 5]).is_err(), "truncated body");
    let mut trailing = wire.clone();
    trailing.push(0);
    assert!(LeanBlock::commitment_of_wire(&trailing).is_err(), "trailing");
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
    assert_eq!(n.stats.block_pulls.load(Ordering::Relaxed), 0);
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
    assert_eq!(r, NewBlockOutcome::Valid(c), "anchor must not SYNC past its own stage");
    assert!(t.elapsed() >= Duration::from_millis(20), "it really did wait");
    assert!(matches!(staging.await.unwrap().unwrap(), StageOutcome::Staged(x) if x == c));

    assert!(n.stats.stage_race_waits.load(Ordering::Relaxed) >= 1, "the wait is counted");
    assert_eq!(n.stats.block_pulls.load(Ordering::Relaxed), 0, "no peer fetch");
    assert_eq!(n.head.lock().await.1, 1);
    assert_eq!(log_len(&n).await, 1, "appended exactly once");
    assert!(n.inflight.lock().await.is_empty());
}

/// The grace is gated on `stage_pending`: with no stage in flight a
/// by-commitment anchor for a block we simply do not have must fall through to
/// SYNCING immediately, not sit out `LEAN_STAGE_RACE_MS`. Otherwise every
/// genuine backfill would pay the grace on top of its pull.
#[tokio::test(flavor = "multi_thread")]
async fn no_stage_pending_means_no_wait() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let t = std::time::Instant::now();
    let r = n.shim_new_by_commitment(B256::repeat_byte(0xab)).await.unwrap();
    assert!(matches!(r, NewBlockOutcome::Syncing { head: 0 }));
    assert!(t.elapsed() < Duration::from_millis(30), "took {:?}", t.elapsed());
    assert_eq!(n.stats.stage_race_waits.load(Ordering::Relaxed), 0);
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
    assert!(matches!(r, NewBlockOutcome::Syncing { head: 0 }), "got {r:?}");
    assert!(t.elapsed() < Duration::from_secs(2), "took {:?}", t.elapsed());
    assert!(matches!(staging.await.unwrap().unwrap(), StageOutcome::Syncing { head: 0 }));
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

    let t = std::time::Instant::now();
    let r = n.shim_new_by_commitment(orphan.commitment).await.unwrap();
    assert!(matches!(r, NewBlockOutcome::Syncing { head: 0 }));
    assert!(t.elapsed() < Duration::from_millis(30), "took {:?}", t.elapsed());
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
    assert_eq!(n.shim_new_by_commitment(c).await.unwrap(), NewBlockOutcome::Valid(c));
    assert_eq!(log_len(&n).await, 1);
}
