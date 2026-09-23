//! THE COLD GRACE (`Config::anchor_grace_ms`, 2026-09-15).
//!
//! `stage_race.rs` gates the two WARM windows: a stage that is running and has
//! named itself in `inflight`, and a stage that has arrived at the RPC layer
//! but has not named itself yet (`stage_pending`). On the 4-validator fleet
//! (450 M budget, 2.5 MB / 863-tx blocks) neither of them fired: anchor paths
//! staged 514, **peer 515**, index 286, with `stage_race_waits=2`. When the
//! anchor landed, the stage for that block had not reached the node AT ALL —
//! nothing staged, nothing claimed, `stage_pending` zero — so the node pulled
//! 2.5 MB from a peer, 200-250 ms on the consensus critical path, for a block
//! that was about to arrive anyway.
//!
//! Why the anchor wins that race on a LAN and never on loopback: the CL spawns
//! `arc_stageBlock` fire-and-forget, and the spawned task must base64+JSON
//! encode a 2.5 MB body before it hits the wire, while `arc_newBlock
//! {commitment}` is a tiny awaited request issued ~15 ms later. Small request,
//! big request, real link — the small one lands first.
//!
//! So: when the anchor holds none of the block and no stage is even pending,
//! wait up to `anchor_grace_ms` for one to show up.

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

/// Build head+1 WITHOUT `shim_build` — that stages its own result, which would
/// hand the anchor a staged entry before the race under test could happen.
async fn wire_head_plus_one(n: &LaneNode, key: u64, txs_per_block: usize) -> (B256, Vec<u8>) {
    let (head_c, head_n, ts) = *n.head.lock().await;
    let mut txs = Vec::new();
    for i in 0..txs_per_block {
        txs.push(funded_tx(n, key + i as u64, 0, 4).await);
    }
    let b = LeanBlock::new(head_c, head_n + 1, ts + 1000, txs);
    (b.commitment, b.to_wire_bytes())
}

/// Blocks physically readable from the log — the "appended exactly once" gate.
async fn log_len(n: &LaneNode) -> u64 {
    let mut len = 0;
    let mut log = n.log.lock().await;
    while log.read_block(len + 1).unwrap().is_some() {
        len += 1;
    }
    len
}

// ---------------------------------------------------------------------------
// THE REGRESSION GATE
// ---------------------------------------------------------------------------

/// The anchor is issued 20 ms BEFORE the stage of the same bytes — the fleet's
/// order, not loopback's. Nothing is staged, nothing is claimed and nothing is
/// pending when it arrives, so both warm graces decline to wait; only the cold
/// grace keeps it here. It must then take the STAGED path and append the block
/// exactly once.
///
/// Without the cold grace this is `path=peer` — and on a node with no peers
/// (this one) that degrades to SYNCING, which is what the companion test in
/// `with_the_grace_disabled_the_anchor_takes_the_peer_path` asserts with
/// `anchor_grace_ms: 0`.
#[tokio::test(flavor = "multi_thread")]
async fn an_anchor_that_beats_its_stage_still_takes_the_staged_path() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let (c, wire) = wire_head_plus_one(&n, 1, 24).await;

    // nothing anywhere: this is the cold window, not `stage_race.rs`'s
    assert!(n.inflight.lock().await.is_empty());
    assert!(n.staged.lock().await.is_empty());
    assert_eq!(n.stage_pending.load(Ordering::Relaxed), 0);

    let anchoring = {
        let n = n.clone();
        tokio::spawn(async move { n.shim_new_by_commitment(c).await })
    };
    // ...and the stage, still encoding 2.5 MB of base64 somewhere, lands late
    tokio::time::sleep(Duration::from_millis(20)).await;
    let staged = n.stage_block(&wire).await.unwrap();
    assert!(matches!(staged, StageOutcome::Staged(x) if x == c));

    let r = tokio::time::timeout(Duration::from_secs(5), anchoring)
        .await
        .expect("anchor hung")
        .unwrap()
        .unwrap();
    assert_eq!(r, NewBlockOutcome::Valid(c), "must not SYNC past its stage");

    assert!(
        n.metrics.anchor_grace_hits.load(Ordering::Relaxed) >= 1,
        "the cold grace is what saved it"
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

/// A stage that NEVER comes: the anchor sits out the whole grace and then
/// answers exactly as it did before — SYNCING here, because this node has no
/// peers to pull from. The grace must be bounded and must be counted as a
/// miss, so the fleet can read hits-vs-misses and decide the window is worth
/// its cost.
#[tokio::test(flavor = "multi_thread")]
async fn a_commitment_nobody_ever_stages_falls_through_after_the_grace() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());

    let t = std::time::Instant::now();
    let r = tokio::time::timeout(
        Duration::from_secs(5),
        n.shim_new_by_commitment(B256::repeat_byte(0xab)),
    )
    .await
    .expect("anchor hung on a stage that never came")
    .unwrap();
    assert!(matches!(r, NewBlockOutcome::Syncing { head: 0 }), "{r:?}");
    assert!(
        t.elapsed() >= Duration::from_millis(50),
        "it really did wait: {:?}",
        t.elapsed()
    );
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    assert_eq!(n.metrics.anchor_grace_hits.load(Ordering::Relaxed), 0);
    assert_eq!(n.metrics.anchor_grace_misses.load(Ordering::Relaxed), 1);
    assert_eq!(n.head.lock().await.1, 0);
}

/// THE LAGGARD. A node that is behind gets anchors for commitments no stage of
/// its own will ever produce. It must not pay the grace on every one of them,
/// so an armed backfill (`sync_target` above our head) skips it outright — the
/// same shape as the stage-race grace being skipped when no stage is pending.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_that_is_behind_does_not_pay_the_grace() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    n.sync_target.store(40, Ordering::Relaxed); // the backfill is armed

    let t = std::time::Instant::now();
    let r = n
        .shim_new_by_commitment(B256::repeat_byte(0xcd))
        .await
        .unwrap();
    assert!(matches!(r, NewBlockOutcome::Syncing { head: 0 }), "{r:?}");
    assert!(
        t.elapsed() < Duration::from_millis(40),
        "took {:?}",
        t.elapsed()
    );
    assert_eq!(n.metrics.anchor_grace_hits.load(Ordering::Relaxed), 0);
    assert_eq!(n.metrics.anchor_grace_misses.load(Ordering::Relaxed), 0);
}

/// The exact scenario above — anchor 20 ms ahead of its stage — with the
/// grace switched off. The anchor must NOT wait: it takes the peer path, which
/// on a peerless node is SYNCING, and the stage that follows still stages
/// cleanly.
#[tokio::test(flavor = "multi_thread")]
async fn with_the_grace_disabled_the_anchor_takes_the_peer_path() {
    let dir = tempfile::tempdir().unwrap();
    let n = Arc::new(
        LaneNode::open(Config {
            datadir: dir.path().to_path_buf(),
            anchor_grace_ms: 0,
            ..Default::default()
        })
        .unwrap()
        .0,
    );
    let (c, wire) = wire_head_plus_one(&n, 900, 8).await;

    let t = std::time::Instant::now();
    let r = n.shim_new_by_commitment(c).await.unwrap();
    assert!(
        matches!(r, NewBlockOutcome::Syncing { head: 0 }),
        "got {r:?}"
    );
    assert!(
        t.elapsed() < Duration::from_millis(40),
        "the grace was supposed to be off: {:?}",
        t.elapsed()
    );
    assert_eq!(n.metrics.anchor_grace_hits.load(Ordering::Relaxed), 0);
    assert_eq!(n.metrics.anchor_grace_misses.load(Ordering::Relaxed), 0);

    // the stage that lost the race is unaffected
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(matches!(n.stage_block(&wire).await.unwrap(), StageOutcome::Staged(x) if x == c));
    // ...and a retried anchor still promotes it, as it always did
    assert_eq!(
        n.shim_new_by_commitment(c).await.unwrap(),
        NewBlockOutcome::Valid(c)
    );
    assert_eq!(n.head.lock().await.1, 1);
    assert!(n.inflight.lock().await.is_empty());
}
