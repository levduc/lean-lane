//! Every block is appended exactly once, whatever feeds it and in whatever
//! order: `arc_stageBlock`, `arc_newBlock{blockBytes}` (the CL's by-bytes
//! feed, the backfill), `arc_newBlock{commitment}`, a sync-queue drain, and
//! announcements. The commit gate is what guarantees it; these tests drive
//! only the public verbs and check the result off the log's own framing.

mod common;

use alloy_primitives::B256;
use common::*;
use lean_lane_node::chain::{genesis_commitment, LeanBlock};
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome, StageOutcome};
use std::sync::Arc;
use std::time::Duration;

/// Rounds of: a child y fed first (queued behind x), then x and y through
/// every route at once (stage, bytes twice, commitment for both, y by bytes).
/// x's commit drains y while y's own feeds race it.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_feeds_append_each_block_once() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    for round in 1..=5u64 {
        let (hc, hn, ts) = head(&n).await;
        let (cx, wx) = wire_child(&n, hc, hn + 1, ts + 1000, round * 1000, 20).await;
        let (cy, wy) = wire_child(&n, cx, hn + 2, ts + 2000, round * 1000 + 500, 20).await;
        assert!(matches!(
            n.shim_new(&wy).await.unwrap(),
            NewBlockOutcome::Syncing { .. }
        ));
        let spawn_new = |w: Vec<u8>| {
            let n = n.clone();
            tokio::spawn(async move { n.shim_new(&w).await })
        };
        let spawn_by_c = |c: B256| {
            let n = n.clone();
            tokio::spawn(async move { n.shim_new_by_commitment(c).await })
        };
        let stage = {
            let (n, w) = (n.clone(), wx.clone());
            tokio::spawn(async move { n.stage_block(&w).await })
        };
        let feeds = vec![
            spawn_new(wx.clone()),
            spawn_by_c(cx),
            spawn_new(wy.clone()),
            spawn_by_c(cy),
            spawn_new(wx.clone()),
        ];
        assert!(matches!(
            stage.await.unwrap().unwrap(),
            StageOutcome::Staged(_) | StageOutcome::Syncing { .. }
        ));
        for f in feeds {
            // every route answers VALID or SYNCING, never an error
            f.await.unwrap().unwrap();
        }
        // the CL's retry of y, in case its feeds all lost to x's commit
        assert_eq!(n.shim_new(&wy).await.unwrap(), NewBlockOutcome::Valid(cy));
        assert_eq!(head(&n).await.0, cy);
        assert_eq!(records_on_disk(dir.path()), 2 * round as usize);
        assert_eq!(get(&n.metrics.blocks), 2 * round);
    }
}

const BLOCKS: usize = 10;
const TXS: usize = 16;
const TASKS: u64 = 6;
const OPS: usize = 24;

fn key(block: usize, tx: usize) -> u64 {
    (block * TXS + tx) as u64 + 10_000
}

/// A short chain of blocks built off-node, one fresh funded sender per tx.
fn prebuilt_chain() -> Vec<(B256, Vec<u8>)> {
    let mut parent = genesis_commitment(Config::default().chain_id);
    (0..BLOCKS)
        .map(|b| {
            let txs = (0..TXS)
                .map(|t| signed(key(b, t), 0, 1 + t % 5, 3).raw().to_vec())
                .collect();
            let blk = LeanBlock::new(parent, b as u64 + 1, 1_000 * (b as u64 + 1), txs);
            parent = blk.commitment;
            (blk.commitment, blk.to_wire_bytes())
        })
        .collect()
}

async fn fund_chain(n: &LaneNode) {
    for b in 0..BLOCKS {
        for t in 0..TXS {
            fund(n, key(b, t), 10 * cost(5, 3)).await;
        }
    }
}

/// THE RANDOMIZED INVARIANT. `TASKS` tasks issue random interleavings of
/// stage / newBlock{bytes} / newBlock{commitment} / announce over the same
/// pre-built chain (mostly its frontier), with random short pauses, across
/// several seeds. Then the
/// CL's in-order retry feeds every block once more. Whatever happened in
/// between: the head is the last block, the log holds each block exactly once
/// in order (byte-identical to a sequential replay's log), and the state
/// digest equals the sequential replay's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn random_interleavings_append_each_block_once() {
    let chain = Arc::new(prebuilt_chain());
    let rdir = tempfile::tempdir().unwrap();
    let reference = open(rdir.path());
    fund_chain(&reference).await;
    for (c, w) in chain.iter() {
        assert_eq!(
            reference.shim_new(w).await.unwrap(),
            NewBlockOutcome::Valid(*c)
        );
    }
    let ref_digest = digest(&reference).await;
    let ref_log = std::fs::read(rdir.path().join("blocks.log")).unwrap();

    for seed in 1..=4u64 {
        let dir = tempfile::tempdir().unwrap();
        let n = open_cfg(Config {
            anchor_grace_ms: 5,
            announce_grace_ms: 5,
            ..cfg(dir.path())
        });
        fund_chain(&n).await;
        let tasks: Vec<_> = (0..TASKS)
            .map(|t| {
                let (n, chain) = (n.clone(), chain.clone());
                tokio::spawn(async move {
                    let mut rng = Rng(seed * 7_919 + t * 104_729 + 1);
                    for _ in 0..OPS {
                        // mostly the frontier (head+1, head+2), so feeds
                        // collide on the same block; sometimes anywhere
                        let h = head(&n).await.1 as usize;
                        let i = match rng.below(4) {
                            0 => rng.below(BLOCKS),
                            _ => (h + rng.below(2)).min(BLOCKS - 1),
                        };
                        let (c, wire) = &chain[i];
                        match rng.below(4) {
                            0 => {
                                n.stage_block(wire).await.unwrap();
                            }
                            1 => {
                                n.shim_new(wire).await.unwrap();
                            }
                            2 => {
                                n.shim_new_by_commitment(*c).await.unwrap();
                            }
                            _ => n.on_announce(*c, i as u64 + 1).await,
                        }
                        match rng.below(3) {
                            0 => {
                                tokio::time::sleep(Duration::from_millis(rng.below(3) as u64)).await
                            }
                            _ => tokio::task::yield_now().await,
                        }
                    }
                })
            })
            .collect();
        for t in tasks {
            t.await.expect("a feed panicked");
        }
        for (c, w) in chain.iter() {
            assert_eq!(
                n.shim_new(w).await.unwrap(),
                NewBlockOutcome::Valid(*c),
                "seed {seed}"
            );
        }
        let (last_c, last_w) = chain.last().unwrap();
        let last = LeanBlock::from_wire_bytes(last_w).unwrap();
        assert_eq!(
            head(&n).await,
            (*last_c, BLOCKS as u64, last.timestamp_ms),
            "seed {seed}"
        );
        assert_eq!(
            records_on_disk(dir.path()),
            BLOCKS,
            "seed {seed}: each block once"
        );
        assert_eq!(get(&n.metrics.blocks), BLOCKS as u64, "seed {seed}");
        assert_eq!(
            std::fs::read(dir.path().join("blocks.log")).unwrap(),
            ref_log,
            "seed {seed}: log differs from the sequential replay"
        );
        assert_eq!(digest(&n).await, ref_digest, "seed {seed}: state digest");
    }
}
