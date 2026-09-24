//! The shim contract (`docs/integration.md`): `arc_buildBlock` purity,
//! `arc_newBlock` idempotency and parent-link discipline, lookup and anchoring
//! by commitment, self-staging of built blocks, and the framing regression.

mod common;

use alloy_primitives::B256;
use common::*;
use lean_lane_node::chain::{genesis_commitment, LeanBlock};
use lean_lane_node::metrics::AnchorPath;
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome, StageOutcome};
use reth_transaction_pool::TransactionPool;

#[test]
fn framing_binds_commitment_v2_regression() {
    // THE ATTACK v1 allowed: same concatenated bodies, different framings.
    let g = genesis_commitment(1338);
    let a = LeanBlock::new(g, 1, 7, vec![b"AB".to_vec(), b"C".to_vec()]);
    let b = LeanBlock::new(g, 1, 7, vec![b"A".to_vec(), b"BC".to_vec()]);
    assert_ne!(
        a.commitment, b.commitment,
        "commitment must bind tx framing"
    );
    let c = LeanBlock::new(g, 1, 7, vec![b"ABC".to_vec()]);
    assert_ne!(a.commitment, c.commitment, "and the tx count");
    let rt = LeanBlock::from_wire_bytes(&a.to_wire_bytes()).unwrap();
    assert_eq!(rt.commitment, a.commitment);
}

/// `commitment_of_wire` (the header-only hash `arc_stageBlock` marks its work
/// under) must agree with a full decode for every block shape, or the anchor
/// would wait on a key nobody works on.
#[tokio::test(flavor = "multi_thread")]
async fn commitment_of_wire_matches_a_full_decode_and_rejects_bad_framing() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    for txs in [0usize, 1, 5, 40] {
        let (c, wire) = wire_head_plus_one(&n, txs as u64 * 100, txs).await;
        let h = LeanBlock::commitment_of_wire(&wire).unwrap();
        let full = LeanBlock::from_wire_bytes(&wire).unwrap();
        assert_eq!((h.commitment, full.commitment), (c, c), "{txs} txs");
        assert_eq!(
            (h.parent, h.number, h.timestamp_ms),
            (full.parent, full.number, full.timestamp_ms)
        );
        assert_eq!(full.to_wire_bytes(), wire);
    }
    let (c, wire) = wire_head_plus_one(&n, 7, 3).await;
    assert!(LeanBlock::commitment_of_wire(&wire[..20]).is_err(), "short");
    assert!(
        LeanBlock::commitment_of_wire(&wire[..wire.len() - 5]).is_err(),
        "truncated"
    );
    let mut trailing = wire.clone();
    trailing.push(0);
    assert!(
        LeanBlock::commitment_of_wire(&trailing).is_err(),
        "trailing"
    );
    // a bad body is refused and leaves nothing behind
    assert!(n.stage_block(&trailing).await.is_err());
    assert!(!n.is_in_progress(c));
    assert!(n.staged.lock().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn build_is_pure_and_head_anchored() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    seed_and_submit(&n, 1, 0, 3).await;
    let (head_c, head_n, _) = head(&n).await;
    let digest0 = digest(&n).await;
    let pool0 = n.pool.pool_size().pending;

    assert!(
        n.shim_build(B256::ZERO, head_n + 1, 1000, 150_000_000)
            .await
            .is_err(),
        "stale parent"
    );
    assert!(
        n.shim_build(head_c, head_n + 2, 1000, 150_000_000)
            .await
            .is_err(),
        "wrong number"
    );

    let (c1, wire1) = n
        .shim_build(head_c, head_n + 1, 1000, 150_000_000)
        .await
        .unwrap();
    let (c2, wire2) = n
        .shim_build(head_c, head_n + 1, 1000, 150_000_000)
        .await
        .unwrap();
    assert_eq!(
        (c1, &wire1),
        (c2, &wire2),
        "same inputs + same pool => same block"
    );
    assert_eq!(
        head(&n).await,
        (head_c, head_n, 0),
        "build must not advance head"
    );
    assert_eq!(digest(&n).await, digest0, "build must not touch state");
    assert_eq!(n.pool.pool_size().pending, pool0, "build must not evict");
    {
        // it DOES self-stage, and an identical rebuild replaces the entry
        let q = n.staged.lock().await;
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].0, c1);
    }
    let decoded = LeanBlock::from_wire_bytes(&wire1).unwrap();
    assert_eq!(decoded.commitment, c1);
    assert_eq!(decoded.txs.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn new_block_full_discipline() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    let genesis = head(&n).await.0;
    seed_and_submit(&n, 11, 0, 2).await;
    let (c1, w1) = n.shim_build(genesis, 1, 1000, 150_000_000).await.unwrap();
    assert_eq!(n.shim_new(&w1).await.unwrap(), NewBlockOutcome::Valid(c1));
    seed_and_submit(&n, 12, 0, 1).await;
    let (c2, w2) = n.shim_build(c1, 2, 2000, 150_000_000).await.unwrap();
    assert_eq!(n.shim_new(&w2).await.unwrap(), NewBlockOutcome::Valid(c2));
    let digest2 = digest(&n).await;

    // 1) re-feed head / 2) an ancestor: idempotent VALID, state untouched
    assert_eq!(n.shim_new(&w2).await.unwrap(), NewBlockOutcome::Valid(c2));
    assert_eq!(n.shim_new(&w1).await.unwrap(), NewBlockOutcome::Valid(c1));
    assert_eq!(digest(&n).await, digest2);
    // 3) a different block at a known height: loud error, nothing appended
    let fork = LeanBlock::new(genesis, 1, 999, vec![]);
    let e = n.shim_new(&fork.to_wire_bytes()).await.unwrap_err();
    assert!(e.contains("conflicting"), "{e}");
    assert_eq!(records_on_disk(dir.path()), 2);
    // 4) a gap (head+2) and 5) a parent mismatch at head+1: SYNCING, queued
    let gap = LeanBlock::new(c2, 4, 3000, vec![]);
    assert_eq!(
        n.shim_new(&gap.to_wire_bytes()).await.unwrap(),
        NewBlockOutcome::Syncing { head: 2 }
    );
    let bad = LeanBlock::new(B256::repeat_byte(9), 3, 3000, vec![]);
    assert_eq!(
        n.shim_new(&bad.to_wire_bytes()).await.unwrap(),
        NewBlockOutcome::Syncing { head: 2 }
    );
    assert_eq!(head(&n).await.1, 2, "SYNCING must not move head");
    // 6) trailing bytes: clean error
    let mut w = LeanBlock::new(c2, 3, 3000, vec![]).to_wire_bytes();
    w.push(0);
    assert!(n.shim_new(&w).await.is_err());
    assert_eq!(digest(&n).await, digest2, "errors must never mutate state");
    // 7) an empty heartbeat block is fine
    let hb = LeanBlock::new(c2, 3, 3000, vec![]);
    assert_eq!(
        n.shim_new(&hb.to_wire_bytes()).await.unwrap(),
        NewBlockOutcome::Valid(hb.commitment)
    );
    // 8) serving by number returns the exact wire bytes
    assert_eq!(n.block_wire_bytes(1).await.unwrap().unwrap(), w1);
    assert!(n.block_wire_bytes(99).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn blocks_are_served_and_anchored_by_commitment() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    // canonical
    let (c1, w1) = advance(&n, 1, 0).await;
    let (c2, w2) = advance(&n, 1, 1).await;
    assert_eq!(
        n.block_wire_bytes_by_commitment(c1).await.unwrap(),
        Some(w1)
    );
    assert_eq!(
        n.block_wire_bytes_by_commitment(c2).await.unwrap(),
        Some(w2)
    );
    assert_eq!(
        n.block_wire_bytes_by_commitment(B256::repeat_byte(9))
            .await
            .unwrap(),
        None
    );
    // staged
    seed_and_submit(&n, 2, 0, 2).await;
    let (c, wire) = build_next(&n).await;
    assert_eq!(n.stage_block(&wire).await.unwrap(), StageOutcome::Staged(c));
    assert_eq!(
        n.block_wire_bytes_by_commitment(c).await.unwrap(),
        Some(wire)
    );
    // anchoring the staged block promotes it; anchoring it again is idempotent
    assert_eq!(
        n.shim_new_by_commitment(c).await.unwrap(),
        NewBlockOutcome::Valid(c)
    );
    assert_eq!(head(&n).await.0, c);
    assert_eq!(
        n.shim_new_by_commitment(c).await.unwrap(),
        NewBlockOutcome::Valid(c)
    );
    // (the two `advance` feeds promoted their self-staged builds too)
    assert_eq!(get(&n.metrics.promoted), 3);
    // unknown, with no peers: SYNCING at our head
    assert_eq!(
        n.shim_new_by_commitment(B256::repeat_byte(7))
            .await
            .unwrap(),
        NewBlockOutcome::Syncing { head: 3 }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_losing_candidate_stays_servable_and_never_promotes() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    seed_and_submit(&n, 5, 0, 2).await;
    let (head_c, head_n, ts) = head(&n).await;
    let (ca, wa) = n
        .shim_build(head_c, head_n + 1, ts + 1000, 1_000_000)
        .await
        .unwrap();
    assert_eq!(n.stage_block(&wa).await.unwrap(), StageOutcome::Staged(ca));
    let (cb, wb) = n
        .shim_build(head_c, head_n + 1, ts + 2000, 1_000_000)
        .await
        .unwrap();
    assert_ne!(ca, cb);
    assert_eq!(n.shim_new(&wb).await.unwrap(), NewBlockOutcome::Valid(cb));
    // a peer may still ask for the loser's bytes
    assert_eq!(
        n.block_wire_bytes_by_commitment(ca).await.unwrap(),
        Some(wa.clone())
    );
    // but it never promotes: a different block now holds its height
    assert!(matches!(
        n.shim_new_by_commitment(ca).await,
        Err(_) | Ok(NewBlockOutcome::Syncing { .. })
    ));
    assert!(n.shim_new(&wa).await.unwrap_err().contains("conflicting"));
    assert_eq!(head(&n).await.0, cb);
    // staging on a parent that is not the head: SYNCING, nothing queued
    let z = LeanBlock::new(head_c, head_n + 1, 333, vec![]);
    assert_eq!(
        n.stage_block(&z.to_wire_bytes()).await.unwrap(),
        StageOutcome::Syncing { head: 1 }
    );
}

/// The proposer path: `arc_buildBlock` stages its own result, so its own
/// `arc_newBlock{commitment}` at decide promotes it instead of pulling its
/// own block back from a peer. A second `arc_stageBlock` of the same bytes is
/// a no-op.
#[tokio::test(flavor = "multi_thread")]
async fn a_built_block_anchors_from_its_own_staged_copy() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    for nonce in 0..3u32 {
        seed_and_submit(&n, 21, nonce, 2).await;
        let (c, wire) = build_next(&n).await;
        assert!(
            n.staged.lock().await.iter().any(|(k, _)| *k == c),
            "build must stage"
        );
        assert_eq!(n.stage_block(&wire).await.unwrap(), StageOutcome::Staged(c));
        assert_eq!(
            n.staged
                .lock()
                .await
                .iter()
                .filter(|(k, _)| *k == c)
                .count(),
            1
        );
        assert_eq!(
            n.shim_new_by_commitment(c).await.unwrap(),
            NewBlockOutcome::Valid(c)
        );
        assert_eq!(head(&n).await.0, c);
    }
    assert_eq!(get(&n.metrics.promoted), 3);
    assert_eq!(n.metrics.anchors(AnchorPath::Staged), 3);
    assert_eq!(get(&n.metrics.block_pulls), 0, "never fetch our own block");
    assert!(
        n.staged.lock().await.len() <= 1,
        "the retention sweep drains the map"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn self_staging_respects_the_32_entry_cap() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    seed_and_submit(&n, 24, 0, 2).await;
    let (head_c, head_n, _) = head(&n).await;
    for i in 0..40u64 {
        n.shim_build(head_c, head_n + 1, 1_000 + i, 1_000_000)
            .await
            .unwrap();
    }
    assert_eq!(n.staged.lock().await.len(), 32);
    let (c, _) = n
        .shim_build(head_c, head_n + 1, 1_039, 1_000_000)
        .await
        .unwrap();
    let q = n.staged.lock().await;
    assert_eq!(
        q.len(),
        32,
        "an identical rebuild replaces rather than duplicates"
    );
    assert_eq!(q.iter().filter(|(k, _)| *k == c).count(), 1);
}

/// The commitment index is rebuilt from the WHOLE log at open, below the
/// snapshot too: every block ever appended is still served by commitment and
/// still anchors as an idempotent VALID.
#[tokio::test(flavor = "multi_thread")]
async fn every_commitment_resolves_after_reopen_from_a_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let reopen = || {
        LaneNode::open(Config {
            snapshot_every: 3,
            ..cfg(dir.path())
        })
        .unwrap()
    };
    let mut blocks = Vec::new();
    {
        let (n, _) = reopen();
        for nonce in 0..7u32 {
            blocks.push(advance(&n, 31, nonce).await);
        }
    }
    let (n, rec) = reopen();
    assert_eq!((rec.snapshot_number, rec.replayed_blocks), (6, 1));
    for (c, wire) in &blocks {
        assert_eq!(
            n.block_wire_bytes_by_commitment(*c).await.unwrap().as_ref(),
            Some(wire)
        );
        assert_eq!(
            n.shim_new_by_commitment(*c).await.unwrap(),
            NewBlockOutcome::Valid(*c)
        );
    }
    assert_eq!(
        n.block_wire_bytes_by_commitment(B256::repeat_byte(9))
            .await
            .unwrap(),
        None
    );
}
