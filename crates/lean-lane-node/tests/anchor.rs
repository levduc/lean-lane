//! `arc_newBlock{commitment}` and the arrival wait.
//!
//! The CL fires `arc_stageBlock` (a 2.5 MB body, spawned) and then
//! `arc_newBlock{commitment}` (tiny, awaited) for the same block ~15 ms later.
//! On a real link the anchor often lands first, or while the stage runs.
//! Whatever the anchor cannot find locally it pulls from a peer, 200-250 ms on
//! the consensus critical path, so it waits: for a stage of its block that is
//! running here (bounded), or for `--anchor-grace-ms` for one to arrive.
//! No peers are configured here, so "went to the peers" shows up as SYNCING.

mod common;

use alloy_primitives::B256;
use common::*;
use lean_lane_node::chain::LeanBlock;
use lean_lane_node::metrics::AnchorPath;
use lean_lane_node::node::{Config, NewBlockOutcome, StageOutcome};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread")]
async fn an_anchor_during_a_running_stage_takes_the_staged_path() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    let (c, wire) = wire_head_plus_one(&n, 1, 24).await;
    let staging = {
        let (n, wire) = (n.clone(), wire.clone());
        tokio::spawn(async move { n.stage_block(&wire).await })
    };
    tokio::time::sleep(Duration::from_millis(3)).await;
    assert_eq!(
        n.shim_new_by_commitment(c).await.unwrap(),
        NewBlockOutcome::Valid(c)
    );
    assert_eq!(staging.await.unwrap().unwrap(), StageOutcome::Staged(c));
    assert_eq!(get(&n.metrics.block_pulls), 0);
    assert_eq!(n.metrics.anchors(AnchorPath::Staged), 1);
    assert_eq!(records_on_disk(dir.path()), 1);
    assert!(!n.is_in_progress(c));
}

/// The fleet's order: the anchor 20 ms BEFORE its stage. Nothing is staged or
/// in progress when it lands; the grace keeps it until the stage arrives.
#[tokio::test(flavor = "multi_thread")]
async fn an_anchor_that_beats_its_stage_still_takes_the_staged_path() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    let (c, wire) = wire_head_plus_one(&n, 1, 24).await;
    let anchoring = {
        let n = n.clone();
        tokio::spawn(async move { n.shim_new_by_commitment(c).await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(n.stage_block(&wire).await.unwrap(), StageOutcome::Staged(c));
    let r = tokio::time::timeout(Duration::from_secs(5), anchoring)
        .await
        .expect("anchor hung")
        .unwrap()
        .unwrap();
    assert_eq!(r, NewBlockOutcome::Valid(c), "must not SYNC past its stage");
    assert_eq!(get(&n.metrics.anchor_grace_hits), 1);
    assert_eq!(get(&n.metrics.anchor_grace_misses), 0);
    assert_eq!(get(&n.metrics.block_pulls), 0);
    assert_eq!(n.metrics.anchors(AnchorPath::Staged), 1);
    assert_eq!(records_on_disk(dir.path()), 1);
}

/// A block nobody ever stages: the anchor sits out the grace (bounded,
/// counted as a miss) and then answers as before: peers, else SYNCING.
#[tokio::test(flavor = "multi_thread")]
async fn an_anchor_for_a_block_that_never_arrives_falls_through_after_the_grace() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    let t = Instant::now();
    let r = n
        .shim_new_by_commitment(B256::repeat_byte(0xab))
        .await
        .unwrap();
    assert_eq!(r, NewBlockOutcome::Syncing { head: 0 });
    assert!(
        t.elapsed() >= Duration::from_millis(55),
        "{:?}",
        t.elapsed()
    );
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    assert_eq!(get(&n.metrics.anchor_grace_hits), 0);
    assert_eq!(get(&n.metrics.anchor_grace_misses), 1);
    assert_eq!(n.metrics.anchors(AnchorPath::Syncing), 1);
}

/// A stage that fails (its parent is not our head) must release an anchor
/// waiting on it promptly, and leave nothing behind for the next one.
#[tokio::test(flavor = "multi_thread")]
async fn a_failing_stage_never_wedges_the_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    let tx = funded_tx(&n, 5, 0, 4).await;
    let orphan = LeanBlock::new(B256::repeat_byte(0x7e), 1, 1_000, vec![tx]);
    let (c, wire) = (orphan.commitment, orphan.to_wire_bytes());
    let staging = {
        let (n, wire) = (n.clone(), wire.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            n.stage_block(&wire).await
        })
    };
    let t = Instant::now();
    let r = tokio::time::timeout(Duration::from_secs(5), n.shim_new_by_commitment(c))
        .await
        .expect("anchor hung on a failed stage")
        .unwrap();
    assert_eq!(r, NewBlockOutcome::Syncing { head: 0 });
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(
        staging.await.unwrap().unwrap(),
        StageOutcome::Syncing { head: 0 }
    );
    assert!(!n.is_in_progress(c));
    assert!(n.staged.lock().await.is_empty());
    assert_eq!(head(&n).await.1, 0);
}

/// A laggard gets anchors for blocks no stage of its own will produce; with
/// the backfill armed it skips the grace.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_is_behind_does_not_pay_the_grace() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    n.sync_target.store(40, Ordering::Relaxed);
    let t = Instant::now();
    let r = n
        .shim_new_by_commitment(B256::repeat_byte(0xcd))
        .await
        .unwrap();
    assert_eq!(r, NewBlockOutcome::Syncing { head: 0 });
    assert!(t.elapsed() < Duration::from_millis(40), "{:?}", t.elapsed());
    assert_eq!(get(&n.metrics.anchor_grace_misses), 0);
}

/// `--anchor-grace-ms 0`: an anchor ahead of its stage goes straight to the
/// peers; the stage that follows still stages, and a retried anchor promotes.
#[tokio::test(flavor = "multi_thread")]
async fn with_the_grace_disabled_the_anchor_takes_the_peer_path() {
    let dir = tempfile::tempdir().unwrap();
    let n = open_cfg(Config {
        anchor_grace_ms: 0,
        ..cfg(dir.path())
    });
    let (c, wire) = wire_head_plus_one(&n, 900, 8).await;
    let t = Instant::now();
    assert_eq!(
        n.shim_new_by_commitment(c).await.unwrap(),
        NewBlockOutcome::Syncing { head: 0 }
    );
    assert!(t.elapsed() < Duration::from_millis(40), "{:?}", t.elapsed());
    assert_eq!(get(&n.metrics.anchor_grace_hits), 0);
    assert_eq!(get(&n.metrics.anchor_grace_misses), 0);
    assert_eq!(n.stage_block(&wire).await.unwrap(), StageOutcome::Staged(c));
    assert_eq!(
        n.shim_new_by_commitment(c).await.unwrap(),
        NewBlockOutcome::Valid(c)
    );
    assert_eq!(head(&n).await.1, 1);
}

/// An anchor landing while a by-bytes commit of the same block (a backfill
/// pull) runs: it answers VALID from that commit, with no peer pull and no
/// second append.
#[tokio::test(flavor = "multi_thread")]
async fn an_anchor_during_a_by_bytes_commit_answers_from_it() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    let (c, wire) = wire_head_plus_one(&n, 1_000, 300).await;
    let commit = {
        let (n, wire) = (n.clone(), wire.clone());
        tokio::spawn(async move { n.shim_new(&wire).await })
    };
    let t0 = Instant::now();
    while !n.is_in_progress(c) && head(&n).await.1 == 0 {
        assert!(t0.elapsed() < Duration::from_secs(5));
        tokio::task::yield_now().await;
    }
    assert_eq!(
        n.shim_new_by_commitment(c).await.unwrap(),
        NewBlockOutcome::Valid(c)
    );
    assert_eq!(commit.await.unwrap().unwrap(), NewBlockOutcome::Valid(c));
    assert_eq!(get(&n.metrics.blocks), 1);
    assert_eq!(get(&n.metrics.block_pulls), 0);
    assert_eq!(records_on_disk(dir.path()), 1);
}
