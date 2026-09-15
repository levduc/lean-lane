//! `LEAN_ANCHOR_GRACE_MS=0` — the cold grace rolls back to the pre-2026-09-15
//! behaviour, in which an anchor that beats its own stage falls straight
//! through to the peer pull (and, on a node with no peers, to SYNCING).
//!
//! Its own test BINARY, deliberately: `anchor_grace_ms()` reads the
//! environment once into a `OnceLock`, so a process can only ever hold one
//! value for it. This file contains exactly one test for the same reason —
//! `cargo test` runs a binary's tests on parallel threads, and a second test
//! here could observe the variable before it is set.

use alloy_primitives::Address;
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

async fn funded_tx(n: &LaneNode, key: u64, outputs: usize) -> Vec<u8> {
    let domain = current_lane_domain();
    let outs = (0..outputs)
        .map(|i| Output {
            to: Address::with_last_byte((i % 200) as u8),
            amount: 3,
        })
        .collect();
    let signed = LeanSigned::new(LeanTx::sign(0, outs, &sk(key), &domain));
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

/// The exact scenario `anchor_grace.rs` rescues — anchor 20 ms ahead of its
/// stage — with the grace switched off. The anchor must NOT wait: it takes the
/// peer path, which on a peerless node is SYNCING, and the stage that follows
/// still stages cleanly.
#[tokio::test(flavor = "multi_thread")]
async fn with_the_grace_disabled_the_anchor_takes_the_peer_path() {
    std::env::set_var("LEAN_ANCHOR_GRACE_MS", "0");

    let dir = tempfile::tempdir().unwrap();
    let n = Arc::new(
        LaneNode::open(Config {
            datadir: dir.path().to_path_buf(),
            ..Default::default()
        })
        .unwrap()
        .0,
    );

    let (head_c, head_n, ts) = *n.head.lock().await;
    let mut txs = Vec::new();
    for i in 0..8u64 {
        txs.push(funded_tx(&n, 900 + i, 4).await);
    }
    let b = LeanBlock::new(head_c, head_n + 1, ts + 1000, txs);
    let (c, wire) = (b.commitment, b.to_wire_bytes());

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
    assert_eq!(n.stats.anchor_grace_hits.load(Ordering::Relaxed), 0);
    assert_eq!(n.stats.anchor_grace_misses.load(Ordering::Relaxed), 0);

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
