//! The announce gate (2026-09-15): a peer's announcement must never make this
//! node pull a block it ALREADY HOLDS.
//!
//! Fleet symptom that produced these gates: a validator stages every height for
//! the vote gap, so when a peer's `arc_announceBlock` for head+1 lands it
//! already has that block executed in memory and its own CL's anchor is
//! milliseconds behind. The old announce path armed the backfill on numbers
//! alone, which pulled 2.5 MB per announcing peer per height and ran a SECOND
//! commit of the same block under the `state` lock, ahead of the anchor —
//! ~1.5 `lean_anchor` lines per height, duplicate on 617 of 781 heights.
//!
//! What must stay true (the self-healing property): a node that is genuinely
//! behind still pulls. Those cases are the last two tests here, plus
//! `sync.rs::three_node_kill_restart_converges_via_backfill`.

use alloy_primitives::{Address, B256};
use lean_lane_node::node::{announce_grace_ms, Config, LaneNode, NewBlockOutcome};
use lean_lane_node::rpc;
use lean_lane_node::state::Acct;
use lean_native::envelope::{current_lane_domain, LeanSigned};
use lean_native::{lean_fee, LeanTx, Output, AMOUNT_UNIT};
use secp256k1::SecretKey;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

fn open(dir: &std::path::Path, peers: Vec<String>) -> Arc<LaneNode> {
    let (n, _) = LaneNode::open(Config {
        datadir: dir.to_path_buf(),
        peers,
        ..Default::default()
    })
    .unwrap();
    Arc::new(n)
}

async fn seed_and_submit(n: &LaneNode, key: u64, nonce: u32) {
    let domain = current_lane_domain();
    let outs = vec![Output {
        to: Address::with_last_byte(7),
        amount: 3,
    }];
    let tx = LeanTx::sign(nonce, outs, &sk(key), &domain);
    let signed = LeanSigned::new(tx);
    let sender = signed.tx().recover_sender(&domain).unwrap();
    if nonce == 0 {
        n.seed_account(
            sender,
            Acct {
                nonce: 0,
                balance: 10 * (lean_fee(1) + 3 * AMOUNT_UNIT),
            },
        )
        .await;
    }
    n.submit_raw(signed.raw()).await.unwrap();
}

/// Build+anchor one block on `n`. Returns (commitment, wire bytes).
async fn advance(n: &LaneNode, key: u64, nonce: u32) -> (B256, Vec<u8>) {
    seed_and_submit(n, key, nonce).await;
    let (head_c, head_n, ts) = *n.head.lock().await;
    let (c, wire) = n
        .shim_build(head_c, head_n + 1, ts + 1000, 1_000_000)
        .await
        .unwrap();
    assert_eq!(n.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(c));
    (c, wire)
}

/// A peer URL nothing will ever answer on, that COUNTS the TCP connections it
/// receives. This is how "no HTTP request is made" is asserted: a pull would
/// have to open a connection to the peer, whatever it then does with it.
async fn counting_peer() -> (String, Arc<AtomicU64>) {
    let l = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = l.local_addr().unwrap();
    let hits = Arc::new(AtomicU64::new(0));
    let h = hits.clone();
    tokio::spawn(async move {
        while l.accept().await.is_ok() {
            h.fetch_add(1, Ordering::Relaxed);
        }
    });
    (format!("http://{addr}"), hits)
}

async fn wait_head(n: &LaneNode, want: u64, secs: u64) -> bool {
    let t0 = Instant::now();
    while n.head.lock().await.1 != want {
        if t0.elapsed() > Duration::from_secs(secs) {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    true
}

#[test]
fn the_grace_window_defaults_to_250ms() {
    if std::env::var("LEAN_ANNOUNCE_GRACE_MS").is_ok() {
        return; // an operator override is in force; nothing to assert
    }
    assert_eq!(announce_grace_ms(), 250);
}

#[tokio::test(flavor = "multi_thread")]
async fn announcing_a_block_we_have_staged_pulls_nothing() {
    // THE regression. We staged head+1 for the vote gap; a peer announces it.
    // Our CL's anchor is milliseconds away, so this must cost zero bytes.
    let dir = tempfile::tempdir().unwrap();
    let (peer_url, hits) = counting_peer().await;
    let n = open(dir.path(), vec![peer_url]);
    n.spawn_sync();
    seed_and_submit(&n, 31, 0).await;
    let (head_c, head_n, ts) = *n.head.lock().await;
    // `arc_buildBlock` self-stages; `arc_stageBlock` on someone else's bytes
    // leaves exactly the same entry (tested in by_commitment.rs).
    let (c, wire) = n
        .shim_build(head_c, head_n + 1, ts + 1000, 1_000_000)
        .await
        .unwrap();
    assert!(
        n.staged.lock().await.iter().any(|(k, _)| *k == c),
        "precondition: staged"
    );

    let t0 = Instant::now();
    n.on_announce(c, head_n + 1).await;
    assert!(
        t0.elapsed() < Duration::from_millis(200),
        "a held block must not wait out the grace"
    );

    assert_eq!(n.stats.announce_skipped_staged.load(Ordering::Relaxed), 1);
    assert_eq!(n.stats.backfill_pulls.load(Ordering::Relaxed), 0);
    assert_eq!(n.stats.block_pulls.load(Ordering::Relaxed), 0);
    assert_eq!(
        n.head.lock().await.1,
        head_n,
        "head must not move on an announcement"
    );
    // give any backfill the announcement could have armed time to act
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        hits.load(Ordering::Relaxed),
        0,
        "no HTTP request may be made"
    );
    assert_eq!(n.stats.backfill_pulls.load(Ordering::Relaxed), 0);

    // and the anchor still lands, from the staged copy
    assert_eq!(
        n.shim_new_by_commitment(c).await.unwrap(),
        NewBlockOutcome::Valid(c)
    );
    assert_eq!(n.head.lock().await.1, head_n + 1);
    assert_eq!(n.stats.promoted.load(Ordering::Relaxed), 1);
    let _ = wire;
}

#[tokio::test(flavor = "multi_thread")]
async fn announcing_a_different_candidate_at_a_staged_height_pulls_nothing() {
    // The proposer we staged for is not necessarily the one a peer announces
    // (a re-proposed height). We are still one height behind with a staged
    // entry AT that height, so the anchor — not a pull — settles which wins.
    let dir = tempfile::tempdir().unwrap();
    let (peer_url, hits) = counting_peer().await;
    let n = open(dir.path(), vec![peer_url]);
    n.spawn_sync();
    seed_and_submit(&n, 32, 0).await;
    let (head_c, head_n, ts) = *n.head.lock().await;
    let (c, _) = n
        .shim_build(head_c, head_n + 1, ts + 1000, 1_000_000)
        .await
        .unwrap();

    let other = B256::repeat_byte(0xc7);
    assert_ne!(other, c);
    n.on_announce(other, head_n + 1).await;

    assert_eq!(n.stats.announce_skipped_staged.load(Ordering::Relaxed), 1);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        hits.load(Ordering::Relaxed),
        0,
        "no HTTP request may be made"
    );
    assert_eq!(n.stats.backfill_pulls.load(Ordering::Relaxed), 0);
    assert_eq!(
        n.stats.log_scans.load(Ordering::Relaxed),
        0,
        "and still no log scan"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn announcing_head_plus_two_with_nothing_staged_still_pulls() {
    // Genuinely behind: the self-healing path must be untouched, and must not
    // wait out the grace either (a gap is proof our CL is not about to close
    // it).
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let a = open(da.path(), vec![]);
    let (a_addr, _ha) = rpc::serve(a.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    let (_c1, _) = advance(&a, 41, 0).await;
    let (c2, _) = advance(&a, 41, 1).await;

    let b = open(db.path(), vec![format!("http://{a_addr}")]);
    b.spawn_sync();
    assert_eq!(b.head.lock().await.1, 0);
    assert!(b.staged.lock().await.is_empty());

    b.on_announce(c2, 2).await;
    assert!(wait_head(&b, 2, 15).await, "a laggard must still backfill");
    assert_eq!(b.head.lock().await.0, c2);
    assert!(
        b.stats.backfill_pulls.load(Ordering::Relaxed) >= 2,
        "both gap blocks are pulled: {}",
        b.stats.backfill_pulls.load(Ordering::Relaxed)
    );
    assert_eq!(b.stats.announce_skipped_staged.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn announcing_head_plus_one_with_a_parked_cl_pulls_after_the_grace() {
    // The reason the grace is a window and not a veto: this node's CL is gone,
    // so nothing will ever stage or anchor head+1. After the grace the
    // backfill arms exactly as before and the node heals itself.
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let a = open(da.path(), vec![]);
    let (a_addr, _ha) = rpc::serve(a.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    let (c1, _) = advance(&a, 42, 0).await;

    let b = open(db.path(), vec![format!("http://{a_addr}")]);
    b.spawn_sync();
    b.on_announce(c1, 1).await;

    assert!(
        wait_head(&b, 1, 15).await,
        "a parked-CL node must still heal via the backfill"
    );
    assert_eq!(b.head.lock().await.0, c1);
    assert!(b.stats.backfill_pulls.load(Ordering::Relaxed) >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_feeds_of_the_same_block_append_it_once() {
    // The other half of the duplicate: a backfill pull and the CL's anchor
    // feeding the SAME bytes at the same moment. The commit is an exclusive
    // claim in `inflight`, so the loser waits for the winner's result and
    // answers VALID — it never appends the block a second time.
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path(), vec![]);
    seed_and_submit(&n, 43, 0).await;
    let (head_c, head_n, ts) = *n.head.lock().await;
    let (c, wire) = n
        .shim_build(head_c, head_n + 1, ts + 1000, 1_000_000)
        .await
        .unwrap();
    // drop the self-staged entry so BOTH feeds take the by-bytes commit path
    n.staged.lock().await.clear();

    let (n1, n2) = (n.clone(), n.clone());
    let (w1, w2) = (wire.clone(), wire.clone());
    let (r1, r2) = tokio::join!(
        tokio::spawn(async move { n1.shim_new(&w1).await }),
        tokio::spawn(async move { n2.shim_new(&w2).await }),
    );
    assert_eq!(r1.unwrap().unwrap(), NewBlockOutcome::Valid(c));
    assert_eq!(r2.unwrap().unwrap(), NewBlockOutcome::Valid(c));
    assert_eq!(*n.head.lock().await, (c, 1, ts + 1000));
    assert_eq!(
        n.stats.blocks.load(Ordering::Relaxed),
        1,
        "exactly one commit"
    );
    assert!(
        n.block_wire_bytes(2).await.unwrap().is_none(),
        "nothing appended above the head"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_anchor_waits_out_an_in_flight_commit_of_the_same_block() {
    // `arc_newBlock{commitment}` arriving while a backfill's by-bytes commit of
    // that commitment is still running: it must WAIT for that result and
    // answer VALID, not queue its own `state`/`log` locks behind it (or worse,
    // fetch the block from a peer because the commitment is not indexed yet).
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path(), vec![]);
    seed_and_submit(&n, 44, 0).await;
    let (head_c, head_n, ts) = *n.head.lock().await;
    let (c, wire) = n
        .shim_build(head_c, head_n + 1, ts + 1000, 1_000_000)
        .await
        .unwrap();
    n.staged.lock().await.clear();

    // Park the commit INSIDE `commit_block`, after it has claimed the
    // commitment, by holding the `state` lock it needs. That makes the race
    // deterministic instead of hoping to observe a sub-millisecond window.
    let state_guard = n.state.lock().await;
    let feeder = n.clone();
    let bytes = wire.clone();
    let commit = tokio::spawn(async move { feeder.shim_new(&bytes).await });
    let t0 = Instant::now();
    while !n.inflight.lock().await.contains_key(&c) {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "the commit must claim the commitment"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let anchor = n.clone();
    let by_c = tokio::spawn(async move { anchor.shim_new_by_commitment(c).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !by_c.is_finished(),
        "the anchor must WAIT on the in-flight commit, not answer early"
    );
    drop(state_guard);

    assert_eq!(commit.await.unwrap().unwrap(), NewBlockOutcome::Valid(c));
    assert_eq!(
        by_c.await.unwrap().unwrap(),
        NewBlockOutcome::Valid(c),
        "idempotent VALID"
    );
    assert_eq!(n.head.lock().await.0, c);
    assert_eq!(
        n.stats.blocks.load(Ordering::Relaxed),
        1,
        "exactly one commit"
    );
    assert_eq!(
        n.stats.block_pulls.load(Ordering::Relaxed),
        0,
        "and no peer fetch"
    );
    assert_eq!(
        n.stats.log_scans.load(Ordering::Relaxed),
        0,
        "and no log scan"
    );
}
