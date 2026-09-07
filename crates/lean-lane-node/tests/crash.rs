//! Crash-consistency gate: kill mid-append (torn tail, garbage tail) and
//! snapshot+tail recovery must reproduce exactly the state of the intact
//! prefix.

use alloy_primitives::Address;
use lean_lane_node::node::{Config, LaneNode};
use lean_lane_node::state::Acct;
use lean_native::envelope::{current_lane_domain, LeanSigned};
use lean_native::{lean_fee, LeanTx, Output, AMOUNT_UNIT};
use secp256k1::SecretKey;
use std::io::Write;

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

async fn make_node(dir: &std::path::Path, snapshot_every: u64) -> LaneNode {
    let cfg = Config {
        datadir: dir.to_path_buf(),
        snapshot_every,
        ..Default::default()
    };
    LaneNode::open(cfg).unwrap().0
}

/// Seed + submit `count` single-output txs (one per sender), producing one
/// block per `per_block` txs. Returns nothing; state lives in the node.
async fn drive(node: &LaneNode, count: u64, per_block: u64) {
    let domain = current_lane_domain();
    for i in 0..count {
        let tx = LeanTx::sign(
            0,
            vec![Output { to: Address::with_last_byte((i % 200) as u8), amount: 5 }],
            &sk(i),
            &domain,
        );
        let signed = LeanSigned::new(tx);
        let sender = signed.tx().recover_sender(&domain).unwrap();
        node.seed_account(sender, Acct { nonce: 0, balance: lean_fee(1) + 5 * AMOUNT_UNIT })
            .await;
        node.submit_raw(signed.raw()).await.unwrap();
        if (i + 1) % per_block == 0 {
            node.produce_block(1_000 + i).await.unwrap().unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_from_snapshot_plus_tail_and_torn_writes() {
    let dir = tempfile::tempdir().unwrap();
    let node = make_node(dir.path(), 2).await; // snapshot every 2 blocks
    drive(&node, 12, 3).await; // 4 blocks: snapshots at 2 and 4, tail = none
    let digest = node.state.lock().await.digest();
    let head = *node.head.lock().await;
    drop(node);

    // 1) clean recovery: snapshot@4 (+ zero tail)
    let node2 = make_node(dir.path(), 2).await;
    assert_eq!(node2.state.lock().await.digest(), digest);
    assert_eq!(*node2.head.lock().await, head, "head must be re-derived from the log tail");
    drop(node2);

    // 2) torn tail: append a partial record (kill mid-append)
    let log_path = dir.path().join("blocks.log");
    let len_before = std::fs::metadata(&log_path).unwrap().len();
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&log_path).unwrap();
        f.write_all(&123u32.to_le_bytes()).unwrap(); // claims 123-byte payload...
        f.write_all(&[0xAB; 40]).unwrap(); // ...delivers 40 garbage bytes
    }
    let node3 = make_node(dir.path(), 2).await;
    assert_eq!(node3.state.lock().await.digest(), digest, "torn tail must be ignored");
    assert_eq!(
        std::fs::metadata(&log_path).unwrap().len(),
        len_before,
        "torn tail must be truncated away"
    );
    drop(node3);

    // 3) garbage that parses as a length but fails the commitment check
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&log_path).unwrap();
        let garbage = vec![0u8; 96];
        f.write_all(&(96u32).to_le_bytes()).unwrap();
        f.write_all(&garbage).unwrap();
        f.write_all(&(96u32).to_le_bytes()).unwrap();
    }
    let node4 = make_node(dir.path(), 2).await;
    assert_eq!(node4.state.lock().await.digest(), digest, "bad-commitment tail must be ignored");

    // 4) and the lane keeps working after recovery: one more block applies
    drive(&node4, 3, 3).await;
    assert_ne!(node4.state.lock().await.digest(), digest);
    let head4 = *node4.head.lock().await;
    assert_eq!(head4.1, head.1 + 1, "exactly one block appended after recovery");
}

#[tokio::test(flavor = "multi_thread")]
async fn total_stf_noop_never_halts() {
    // A tx that passes pool admission but is invalidated before execution
    // (another tx from the same sender executed first via direct injection)
    // must become a NO-OP, not an error.
    let dir = tempfile::tempdir().unwrap();
    let node = make_node(dir.path(), 1024).await;
    let domain = current_lane_domain();
    let t1 = LeanTx::sign(0, vec![Output { to: Address::with_last_byte(1), amount: 5 }], &sk(500), &domain);
    let s1 = LeanSigned::new(t1);
    let sender = s1.tx().recover_sender(&domain).unwrap();
    node.seed_account(sender, Acct { nonce: 0, balance: 2 * (lean_fee(1) + 5 * AMOUNT_UNIT) })
        .await;
    node.submit_raw(s1.raw()).await.unwrap();
    node.produce_block(1_000).await.unwrap().unwrap();

    // Same sender, nonce 0 again — stale by now; force it around the pool by
    // injecting directly into a block via the log-replay path semantics:
    // simplest equivalent: submit nonce-2 (gap) is rejected by pool; so build
    // a block by hand through apply_block with a stale item.
    let stale = LeanTx::sign(0, vec![Output { to: Address::with_last_byte(2), amount: 5 }], &sk(500), &domain);
    let stale_signed = LeanSigned::new(stale);
    let mut state = node.state.lock().await;
    let before = state.digest();
    let out = lean_lane_node::exec::apply_block(
        &mut state,
        &[(sender, stale_signed.tx().clone(), *stale_signed.hash())],
        99,
        Address::with_last_byte(0xbe),
    );
    assert_eq!(out.applied, 0);
    assert_eq!(out.noops, 1);
    assert!(!out.receipts[0].applied);
    assert_eq!(state.digest(), before, "no-op leaves state untouched");
}
