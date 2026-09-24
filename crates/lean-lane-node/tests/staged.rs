//! Deferred anchor execution is consensus-critical: stage + promote must leave
//! byte-identical state, head, counters and LOG FILE to the direct
//! `arc_newBlock{blockBytes}` path. A divergence here forks the lane.

mod common;

use alloy_primitives::Address;
use common::*;
use lean_lane_node::chain::LeanBlock;
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome, StageOutcome};
use lean_native::envelope::{current_lane_domain, LeanSigned};
use lean_native::{LeanTx, Output};
use std::sync::Arc;
use std::time::Duration;

fn open_node(dir: &std::path::Path) -> Arc<LaneNode> {
    open_cfg(Config {
        snapshot_every: 7,
        ..cfg(dir)
    })
}

async fn seed_all(nodes: &[&LaneNode], keys: u64) {
    for n in nodes {
        for k in 0..keys {
            fund(n, k, 200 * cost(100, 1)).await;
        }
        snapshot_now(n).await;
    }
}

/// Everything two nodes that executed the same blocks must agree on.
async fn assert_same(
    d: &LaneNode,
    s: &LaneNode,
    dd: &std::path::Path,
    ds: &std::path::Path,
    at: &str,
) {
    assert_eq!(head(d).await, head(s).await, "head diverged {at}");
    assert_eq!(digest(d).await, digest(s).await, "state diverged {at}");
    assert_eq!(
        d.state.lock().await.accounts.len(),
        s.state.lock().await.accounts.len(),
        "account count diverged {at}"
    );
    assert_eq!(
        (get(&d.metrics.noops), get(&d.metrics.outputs)),
        (get(&s.metrics.noops), get(&s.metrics.outputs)),
        "no-op/output counts diverged {at}"
    );
    assert_eq!(
        std::fs::read(dd.join("blocks.log")).unwrap(),
        std::fs::read(ds.join("blocks.log")).unwrap(),
        "log files must be byte-identical {at}"
    );
}

/// Randomized blocks (0..6 txs, mixed fan-out incl. duplicate, self and zero
/// outputs, some from a never-funded sender so they are no-ops) through both
/// paths. A no-op's recipients must not be created by promote: the account
/// COUNT is covered by the digest and checked on its own.
#[tokio::test(flavor = "multi_thread")]
async fn differential_stage_promote_vs_direct() {
    let (dd, ds) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (d, s) = (open_node(dd.path()), open_node(ds.path()));
    let domain = current_lane_domain();
    seed_all(&[&d, &s], 40).await;
    let mut rng = Rng(0x5eed_1e13);
    let mut nonces = [0u32; 41];
    for height in 1..=30u64 {
        let mut txs = Vec::new();
        for _ in 0..rng.below(7) {
            let k = rng.below(41); // key 40 is never funded: its txs are no-ops
            let outs: Vec<Output> = (0..1 + rng.below(12))
                .map(|j| Output {
                    to: Address::with_last_byte(((j * 7 + rng.below(50)) % 251) as u8),
                    amount: rng.below(9) as u64,
                })
                .collect();
            let tx = LeanTx::sign(nonces[k], outs, &sk(k as u64), &domain);
            txs.push(LeanSigned::new(tx).raw().to_vec());
            nonces[k] += 1;
        }
        let (hc, hn, _) = head(&d).await;
        let block = LeanBlock::new(hc, hn + 1, 10_000 + height, txs);
        let wire = block.to_wire_bytes();
        assert!(matches!(
            d.shim_new(&wire).await.unwrap(),
            NewBlockOutcome::Valid(_)
        ));
        assert_eq!(
            s.stage_block(&wire).await.unwrap(),
            StageOutcome::Staged(block.commitment)
        );
        assert!(matches!(
            s.shim_new(&wire).await.unwrap(),
            NewBlockOutcome::Valid(_)
        ));
        assert_same(&d, &s, dd.path(), ds.path(), &format!("at {height}")).await;
    }
    assert_eq!(get(&s.metrics.promoted), 30, "every anchor on S promotes");
    assert!(get(&d.metrics.noops) > 0, "the no-op case was exercised");
}

/// The proposer's route (build -> self-stage -> anchor by commitment) against
/// a follower fed the same bytes.
#[tokio::test(flavor = "multi_thread")]
async fn self_staged_build_matches_the_direct_path() {
    let (dp, dd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (p, d) = (open(dp.path()), open(dd.path()));
    for nonce in 0..6u32 {
        seed_and_submit(&p, 23, nonce, 3).await;
        seed_and_submit(&d, 23, nonce, 3).await;
        let (c, wire) = build_next(&p).await;
        assert_eq!(
            p.shim_new_by_commitment(c).await.unwrap(),
            NewBlockOutcome::Valid(c)
        );
        assert_eq!(d.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(c));
        assert_same(&p, &d, dp.path(), dd.path(), &format!("at {nonce}")).await;
    }
    assert_eq!((get(&p.metrics.promoted), get(&d.metrics.promoted)), (6, 0));
}

/// `arc_newBlock{blockBytes}` arriving while the stage of the same bytes is
/// still executing (200-tx blocks, milliseconds of ecrecover): it waits for
/// the stage and promotes, and the result matches the direct path.
#[tokio::test(flavor = "multi_thread")]
async fn newblock_during_a_running_stage_promotes_and_matches_direct() {
    let (dd, ds) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (d, s) = (open_node(dd.path()), open_node(ds.path()));
    let domain = current_lane_domain();
    seed_all(&[&d, &s], 200).await;
    for height in 1..=8u64 {
        let txs = (0..200u64)
            .map(|k| {
                let outs = (0..3)
                    .map(|j| Output {
                        to: Address::with_last_byte(((j * 7 + k as usize) % 251) as u8),
                        amount: 1,
                    })
                    .collect();
                let tx = LeanTx::sign(height as u32 - 1, outs, &sk(k), &domain);
                LeanSigned::new(tx).raw().to_vec()
            })
            .collect();
        let (hc, hn, _) = head(&d).await;
        let block = LeanBlock::new(hc, hn + 1, 20_000 + height, txs);
        let wire = block.to_wire_bytes();
        assert!(matches!(
            d.shim_new(&wire).await.unwrap(),
            NewBlockOutcome::Valid(_)
        ));

        let stager = {
            let (s, w) = (s.clone(), wire.clone());
            tokio::spawn(async move { s.stage_block(&w).await })
        };
        let t0 = std::time::Instant::now();
        while !s.is_in_progress(block.commitment)
            && !s
                .staged
                .lock()
                .await
                .iter()
                .any(|(c, _)| *c == block.commitment)
        {
            assert!(t0.elapsed() < Duration::from_secs(2), "stage never started");
            tokio::time::sleep(Duration::from_micros(200)).await;
        }
        assert!(matches!(
            s.shim_new(&wire).await.unwrap(),
            NewBlockOutcome::Valid(_)
        ));
        assert!(matches!(
            stager.await.unwrap().unwrap(),
            StageOutcome::Staged(_)
        ));
        assert_same(&d, &s, dd.path(), ds.path(), &format!("at {height}")).await;
    }
    assert_eq!(get(&s.metrics.promoted), 8, "every raced feed must promote");
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_between_stage_and_promote_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (digest0, wire) = {
        let n = open_node(dir.path());
        let x = LeanBlock::new(head(&n).await.0, 1, 444, vec![]);
        assert!(matches!(
            n.stage_block(&x.to_wire_bytes()).await.unwrap(),
            StageOutcome::Staged(_)
        ));
        // crash here: staged data is memory-only, the log is untouched
        (digest(&n).await, x.to_wire_bytes())
    };
    let n = open_node(dir.path());
    assert_eq!(head(&n).await.1, 0, "staging must not persist anything");
    assert_eq!(digest(&n).await, digest0);
    assert!(matches!(
        n.shim_new(&wire).await.unwrap(),
        NewBlockOutcome::Valid(_)
    ));
    assert_eq!(head(&n).await.1, 1);
}
