//! Node-level replay/restart equivalence (SPEC.md §5): generated adversarial
//! chains are fed to a real `LaneNode` through BOTH commit paths — direct
//! `arc_newBlock(bytes)` and vote-gap `stage_block` + promote by commitment —
//! with the node killed and reopened (snapshot + block-log replay recovery)
//! at random heights. After every block the node's head and state digest must
//! equal the oracle's; a framing-rejected block must be refused and change
//! nothing. Scale with `LEAN_NODE_RESTART_CASES` / `LEAN_NODE_RESTART_BLOCKS`.

use alloy_primitives::B256;
use lean_diff::env_count;
use lean_diff::gen::Rates;
use lean_diff::rng::{mix, Rng};
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome, StageOutcome};
use lean_lane_node::state::Snapshots;
use std::path::Path;

fn cfg(dir: &Path, snapshot_every: u64) -> Config {
    Config {
        datadir: dir.to_path_buf(),
        snapshot_every,
        fsync: false,
        ..Default::default()
    }
}

async fn digest_head(n: &LaneNode) -> (B256, (B256, u64, u64)) {
    (n.state.lock().await.digest(), *n.head.lock().await)
}

#[tokio::test(flavor = "multi_thread")]
async fn node_commit_paths_and_restarts_match_the_oracle() {
    let seed = env_count("LEAN_DIFF_SEED", 1);
    let chains = env_count("LEAN_NODE_RESTART_CASES", 6);
    let blocks = env_count("LEAN_NODE_RESTART_BLOCKS", 24);
    let rates = Rates::parse("max_entries=24").unwrap();
    let (mut restarts, mut staged, mut direct, mut refused) = (0, 0, 0, 0);
    for c in 0..chains {
        let cs = mix(seed ^ 0x40de, c);
        let g = lean_diff::restart::generate(cs, blocks, &rates).unwrap();
        let mut rng = Rng::new(cs);
        let dir = tempfile::tempdir().unwrap();
        let snapshot_every = rng.range(0, 5); // 0 = never: pure log replay
        let conf = || cfg(dir.path(), snapshot_every);
        let genesis = (lean_core::genesis_commitment(1338), 0u64, 0u64);

        // Genesis exactly as the binary does it: seed, then snapshot 0.
        let mut node = LaneNode::open(conf()).unwrap().0;
        for (a, acct) in &g.genesis_engine.accounts {
            node.seed_account(*a, *acct).await;
        }
        node.state
            .lock()
            .await
            .write_snapshot(dir.path(), genesis.0, 0, 0)
            .unwrap();
        let mut expect = (g.genesis_engine.digest(), genesis);

        for (i, blk) in g.blocks.iter().enumerate() {
            if rng.chance(0.15) {
                drop(node);
                node = LaneNode::open(conf()).unwrap().0;
                restarts += 1;
                assert_eq!(
                    digest_head(&node).await,
                    expect,
                    "chain {c} (seed {cs}): state after reopen before block {i}"
                );
            }
            let via_stage = rng.chance(0.4);
            let got = if via_stage {
                match node.stage_block(&blk.wire).await {
                    Ok(StageOutcome::Staged(cm)) => node.shim_new_by_commitment(cm).await,
                    Ok(other) => panic!("chain {c} block {i}: stage returned {other:?}"),
                    Err(e) => Err(e),
                }
            } else {
                node.shim_new(&blk.wire).await
            };
            match (g.oracle[i], got) {
                (Some((commitment, number, ts, digest)), Ok(NewBlockOutcome::Valid(cm))) => {
                    assert_eq!(cm.0, commitment, "chain {c} block {i}: commitment");
                    expect = (B256::from(digest), (B256::from(commitment), number, ts));
                    if via_stage {
                        staged += 1;
                    } else {
                        direct += 1;
                    }
                }
                (None, Err(_)) => refused += 1,
                (want, got) => panic!(
                    "chain {c} (seed {cs}) block {i} (staged={via_stage}): oracle {:?} vs node {got:?}",
                    want.map(|w| hex::encode(w.0))
                ),
            }
            assert_eq!(
                digest_head(&node).await,
                expect,
                "chain {c} (seed {cs}) block {i} (staged={via_stage}): node digest/head"
            );
        }
        drop(node);
        let node = LaneNode::open(conf()).unwrap().0;
        assert_eq!(digest_head(&node).await, expect, "chain {c}: final reopen");
        restarts += 1;
    }
    eprintln!(
        "node restart: {chains} chains, {direct} direct + {staged} staged commits, {refused} \
         framing refusals, {restarts} reopen recoveries"
    );
    assert!(staged > 0 && direct > 0 && restarts > chains);
}
