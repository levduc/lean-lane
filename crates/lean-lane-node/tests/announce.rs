//! A peer's announcement must never make this node pull a block it already
//! holds (staged, or in progress): its own CL anchors it milliseconds later.
//! Pulling it anyway cost 2.5 MB per announcing peer per height and a second
//! commit of the same block. A node that is genuinely behind still pulls
//! (the last two tests, and `sync.rs`).

mod common;

use alloy_primitives::B256;
use common::*;
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome};
use lean_lane_node::rpc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn with_peers(dir: &std::path::Path, peers: Vec<String>) -> Arc<LaneNode> {
    let n = open_cfg(Config { peers, ..cfg(dir) });
    n.spawn_sync();
    n
}

/// A peer URL that answers nothing but COUNTS the TCP connections it gets:
/// any pull would have to open one.
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

#[tokio::test(flavor = "multi_thread")]
async fn announcing_a_block_we_have_staged_pulls_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (peer, hits) = counting_peer().await;
    let n = with_peers(dir.path(), vec![peer]);
    seed_and_submit(&n, 31, 0, 1).await;
    let (c, _) = build_next(&n).await; // self-staged
    let t0 = Instant::now();
    n.on_announce(c, 1).await;
    assert!(
        t0.elapsed() < Duration::from_millis(200),
        "no grace for a held block"
    );
    assert_eq!(get(&n.metrics.announce_skipped_staged), 1);
    assert_eq!(head(&n).await.1, 0, "an announcement never moves the head");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        hits.load(Ordering::Relaxed),
        0,
        "no HTTP request may be made"
    );
    assert_eq!(get(&n.metrics.backfill_pulls), 0);
    // and the anchor still lands from the staged copy
    assert_eq!(
        n.shim_new_by_commitment(c).await.unwrap(),
        NewBlockOutcome::Valid(c)
    );
    assert_eq!(get(&n.metrics.promoted), 1);
}

/// The announced candidate need not be the one we staged (a re-proposed
/// height): holding ANY staged candidate at head+1 means the anchor, not a
/// pull, settles it.
#[tokio::test(flavor = "multi_thread")]
async fn announcing_a_different_candidate_at_a_staged_height_pulls_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (peer, hits) = counting_peer().await;
    let n = with_peers(dir.path(), vec![peer]);
    seed_and_submit(&n, 32, 0, 1).await;
    let (c, _) = build_next(&n).await;
    let other = B256::repeat_byte(0xc7);
    assert_ne!(other, c);
    n.on_announce(other, 1).await;
    assert_eq!(get(&n.metrics.announce_skipped_staged), 1);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        hits.load(Ordering::Relaxed),
        0,
        "no HTTP request may be made"
    );
    assert_eq!(get(&n.metrics.backfill_pulls), 0);
}

/// Genuinely behind (a gap): pull at once, no grace.
#[tokio::test(flavor = "multi_thread")]
async fn announcing_head_plus_two_with_nothing_staged_still_pulls() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = open(da.path());
    let (a_addr, _ha) = rpc::serve(a.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    advance(&a, 41, 0).await;
    let (c2, _) = advance(&a, 41, 1).await;
    let b = with_peers(db.path(), vec![format!("http://{a_addr}")]);
    b.on_announce(c2, 2).await;
    assert!(wait_head(&b, 2, 15).await, "a laggard must still backfill");
    assert_eq!(head(&b).await.0, c2);
    assert!(get(&b.metrics.backfill_pulls) >= 2);
    assert_eq!(get(&b.metrics.announce_skipped_staged), 0);
}

/// This node's CL is gone, so nothing will ever stage or anchor head+1: after
/// the grace the backfill arms and the node heals itself.
#[tokio::test(flavor = "multi_thread")]
async fn announcing_head_plus_one_with_a_parked_cl_pulls_after_the_grace() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = open(da.path());
    let (a_addr, _ha) = rpc::serve(a.clone(), ([127, 0, 0, 1], 0).into())
        .await
        .unwrap();
    let (c1, _) = advance(&a, 42, 0).await;
    let b = with_peers(db.path(), vec![format!("http://{a_addr}")]);
    b.on_announce(c1, 1).await;
    assert!(
        wait_head(&b, 1, 15).await,
        "a parked-CL node must still heal"
    );
    assert_eq!(head(&b).await.0, c1);
    assert!(get(&b.metrics.backfill_pulls) >= 1);
}
