//! Recovery: snapshot + log-tail replay reproduces the state exactly; a torn
//! or garbage tail is cut away; a snapshot ahead of the log falls back; and
//! total-STF means an invalid tx is a no-op, never a halt.

mod common;

use common::*;
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome};
use std::io::Write;
use std::sync::Arc;

fn reopen(dir: &std::path::Path, snapshot_every: u64) -> Arc<LaneNode> {
    open_cfg(Config {
        snapshot_every,
        ..cfg(dir)
    })
}

/// Fund + submit `count` single-output txs (keys `from..`), producing a block
/// through the full path every `per_block`.
async fn drive(n: &LaneNode, from: u64, count: u64, per_block: u64) {
    for i in from..from + count {
        fund(n, i, cost(1, 5)).await;
        n.submit_raw(signed(i, 0, 1, 5).raw()).await.unwrap();
        if (i - from + 1).is_multiple_of(per_block) {
            produce(n, 1_000 + i, 150_000_000).await.unwrap();
        }
    }
}

fn append_raw(path: &std::path::Path, chunks: &[&[u8]]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for c in chunks {
        f.write_all(c).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_from_snapshot_plus_tail_and_torn_writes() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("blocks.log");
    let n = reopen(dir.path(), 2);
    drive(&n, 0, 12, 3).await; // 4 blocks, snapshots at 2 and 4
    let (d, h) = (digest(&n).await, head(&n).await);
    drop(n);

    // 1) clean reopen: identical state and head
    let n = reopen(dir.path(), 2);
    assert_eq!((digest(&n).await, head(&n).await), (d, h));
    drop(n);

    // 2) torn tail (kill mid-append): ignored and truncated away
    let len = std::fs::metadata(&log).unwrap().len();
    append_raw(&log, &[&123u32.to_le_bytes(), &[0xAB; 40]]);
    let n = reopen(dir.path(), 2);
    assert_eq!(digest(&n).await, d, "torn tail must be ignored");
    assert_eq!(std::fs::metadata(&log).unwrap().len(), len, "and truncated");
    drop(n);

    // 3) a well-framed record that fails the commitment check: ignored
    append_raw(
        &log,
        &[&96u32.to_le_bytes(), &[0u8; 96], &96u32.to_le_bytes()],
    );
    let n = reopen(dir.path(), 2);
    assert_eq!(digest(&n).await, d, "bad-commitment tail must be ignored");

    // 4) and the lane keeps working: exactly one more block appends
    drive(&n, 100, 3, 3).await;
    assert_ne!(digest(&n).await, d);
    assert_eq!(head(&n).await.1, h.1 + 1);
    assert_eq!(records_on_disk(dir.path()), 5);
}

/// A tx that passed admission but is stale at execution becomes a NO-OP.
#[tokio::test(flavor = "multi_thread")]
async fn total_stf_noop_never_halts() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    fund(&n, 500, 2 * cost(1, 5)).await;
    n.submit_raw(signed(500, 0, 1, 5).raw()).await.unwrap();
    produce(&n, 1_000, 150_000_000).await.unwrap();
    // nonce 0 again: stale now, and the pool would refuse it, so execute it
    // directly the way a byzantine proposer's block would
    let stale = signed(500, 0, 1, 5);
    let mut state = n.state.lock().await;
    let before = state.digest();
    let out = lean_core::apply_block(
        &mut state,
        &[(sender(500), stale.tx().clone(), *stale.hash())],
        &n.engine,
    );
    assert_eq!((out.applied, out.noops), (0, 1));
    assert_eq!(state.digest(), before, "a no-op leaves state untouched");
}

/// With `fsync` off the snapshot is durable but the log append is not, so a
/// crash can leave a snapshot above the last intact record. `open` must fall
/// back to the newest snapshot the log can continue from and replay to that
/// record, instead of leaving an unservable hole below the head.
#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_ahead_of_the_log_falls_back_instead_of_leaving_a_hole() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("blocks.log");
    let n = reopen(dir.path(), 2);
    // every sender funded before block 1: seeds are not logged, so one made
    // after a snapshot could not be replayed and the digests would not compare
    for i in 900..912 {
        fund(&n, i, cost(1, 5)).await;
    }
    for i in 900..909 {
        n.submit_raw(signed(i, 0, 1, 5).raw()).await.unwrap();
        if (i - 900 + 1).is_multiple_of(3) {
            produce(&n, 1_000 + i, 150_000_000).await.unwrap();
        }
    }
    let (len3, d3, h3) = (
        std::fs::metadata(&log).unwrap().len(),
        digest(&n).await,
        head(&n).await,
    );
    assert_eq!(h3.1, 3);
    for i in 909..912 {
        n.submit_raw(signed(i, 0, 1, 5).raw()).await.unwrap();
    }
    produce(&n, 2_000, 150_000_000).await.unwrap(); // block 4 -> snapshot-4.bin
    drop(n);
    assert!(dir.path().join("snapshot-4.bin").exists());
    // the crash: the snapshot survives, the log tail does not
    std::fs::OpenOptions::new()
        .write(true)
        .open(&log)
        .unwrap()
        .set_len(len3)
        .unwrap();

    let (n, report) = LaneNode::open(Config {
        snapshot_every: 2,
        ..cfg(dir.path())
    })
    .unwrap();
    assert_eq!(
        report.snapshot_number, 2,
        "recovered from the snapshot BELOW the tail"
    );
    assert_eq!(head(&n).await, h3, "head is the last intact record");
    assert_eq!(digest(&n).await, d3);
    // no hole: height 4 is head+1 again, ordinary lag
    for i in 909..912 {
        n.submit_raw(signed(i, 0, 1, 5).raw()).await.unwrap();
    }
    produce(&n, 3_000, 150_000_000).await.unwrap();
    assert_eq!(head(&n).await.1, 4);
    assert!(n.block_wire_bytes(4).await.unwrap().is_some());
}

/// `--snapshot-every 0` means never (it used to panic on `% 0` in both append
/// paths). Recovery is then a full replay of the log.
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_every_zero_never_snapshots_and_never_panics() {
    let dir = tempfile::tempdir().unwrap();
    let h = {
        let n = reopen(dir.path(), 0);
        drive(&n, 0, 12, 3).await; // 4 blocks through `commit_block`
        seed_and_submit(&n, 999, 0, 1).await; // and one through `promote_staged`
        let (c, _) = build_next(&n).await;
        assert_eq!(
            n.shim_new_by_commitment(c).await.unwrap(),
            NewBlockOutcome::Valid(c)
        );
        assert_eq!(get(&n.metrics.promoted), 1);
        head(&n).await
    };
    assert_eq!(h.1, 5);
    let snaps = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("snapshot-")
        })
        .count();
    assert_eq!(snaps, 0);
    // (the seeds are not in the log, so replayed txs are no-ops and the state
    // legitimately differs; the chain is what must match)
    let (n, report) = LaneNode::open(Config {
        snapshot_every: 0,
        ..cfg(dir.path())
    })
    .unwrap();
    assert_eq!((report.snapshot_number, report.replayed_blocks), (0, 5));
    assert_eq!(head(&n).await, h);
}

/// Restart/replay equivalence across both append paths: a chain written by
/// promotes and direct commits, reopened with no snapshot, replays to the
/// same head and digest as the live node had.
#[tokio::test(flavor = "multi_thread")]
async fn replay_reproduces_a_mixed_promote_and_commit_chain() {
    let dir = tempfile::tempdir().unwrap();
    let (h, d) = {
        let n = reopen(dir.path(), 0);
        for k in 0..6 {
            fund(&n, 700 + k, 10 * cost(2, 3)).await;
        }
        snapshot_now(&n).await; // genesis funding survives the reopen
        for k in 0..6u64 {
            n.submit_raw(signed(700 + k, 0, 2, 3).raw()).await.unwrap();
            let (c, wire) = build_next(&n).await;
            let r = if k % 2 == 0 {
                n.shim_new_by_commitment(c).await.unwrap() // promote
            } else {
                n.staged.lock().await.clear();
                n.shim_new(&wire).await.unwrap() // full execution
            };
            assert_eq!(r, NewBlockOutcome::Valid(c));
        }
        assert_eq!(get(&n.metrics.promoted), 3);
        (head(&n).await, digest(&n).await)
    };
    let (n, report) = LaneNode::open(Config {
        snapshot_every: 0,
        ..cfg(dir.path())
    })
    .unwrap();
    assert_eq!(report.replayed_blocks, 6);
    assert_eq!((head(&n).await, digest(&n).await), (h, d));
}
