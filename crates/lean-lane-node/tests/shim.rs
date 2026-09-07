//! Shim contract tests: buildBlock purity, newBlock idempotency + parent-link
//! discipline, and the v2 framing-binding regression (the equivocation hole).

use alloy_primitives::{Address, B256};
use lean_lane_node::chain::{genesis_commitment, LeanBlock};
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome};
use lean_lane_node::state::Acct;
use lean_native::envelope::{current_lane_domain, LeanSigned};
use lean_native::{lean_fee, LeanTx, Output, AMOUNT_UNIT};
use reth_transaction_pool::TransactionPool;
use secp256k1::SecretKey;

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

async fn node(dir: &std::path::Path) -> LaneNode {
    LaneNode::open(Config { datadir: dir.to_path_buf(), ..Default::default() }).unwrap().0
}

async fn seed_and_submit(n: &LaneNode, key: u64, outputs: usize) {
    let domain = current_lane_domain();
    let outs = (0..outputs)
        .map(|i| Output { to: Address::with_last_byte((i % 200) as u8), amount: 3 })
        .collect();
    let tx = LeanTx::sign(0, outs, &sk(key), &domain);
    let signed = LeanSigned::new(tx);
    let sender = signed.tx().recover_sender(&domain).unwrap();
    n.seed_account(
        sender,
        Acct { nonce: 0, balance: lean_fee(outputs) + outputs as u128 * 3 * AMOUNT_UNIT },
    )
    .await;
    n.submit_raw(signed.raw()).await.unwrap();
}

#[test]
fn framing_binds_commitment_v2_regression() {
    // THE ATTACK v1 allowed: same concatenated bodies, different framings.
    let g = genesis_commitment(1338);
    let a = LeanBlock::new(g, 1, 7, vec![b"AB".to_vec(), b"C".to_vec()]);
    let b = LeanBlock::new(g, 1, 7, vec![b"A".to_vec(), b"BC".to_vec()]);
    assert_ne!(
        a.commitment, b.commitment,
        "v2 commitment must bind tx framing, not just concatenated bodies"
    );
    // and count alone must also bind
    let c = LeanBlock::new(g, 1, 7, vec![b"ABC".to_vec()]);
    assert_ne!(a.commitment, c.commitment);
    // wire round-trip preserves commitment exactly
    let rt = LeanBlock::from_wire_bytes(&a.to_wire_bytes()).unwrap();
    assert_eq!(rt.commitment, a.commitment);
}

#[tokio::test(flavor = "multi_thread")]
async fn build_is_pure_and_head_anchored() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path()).await;
    seed_and_submit(&n, 1, 3).await;
    let (head_c, head_n, _) = *n.head.lock().await;
    let digest0 = n.state.lock().await.digest();
    let pool0 = n.pool.pool_size().pending;

    // stale parent rejected
    assert!(n.shim_build(B256::ZERO, head_n + 1, 1000, 150_000_000).await.is_err());
    // wrong number rejected (deviation from the doc, checked loudly)
    assert!(n.shim_build(head_c, head_n + 2, 1000, 150_000_000).await.is_err());

    // valid build: deterministic, never mutates
    let (c1, wire1) = n.shim_build(head_c, head_n + 1, 1000, 150_000_000).await.unwrap();
    let (c2, wire2) = n.shim_build(head_c, head_n + 1, 1000, 150_000_000).await.unwrap();
    assert_eq!(c1, c2, "same inputs + same pool snapshot => same block");
    assert_eq!(wire1, wire2);
    assert_eq!(*n.head.lock().await, (head_c, head_n, 0), "build must not advance head");
    assert_eq!(n.state.lock().await.digest(), digest0, "build must not touch state");
    assert_eq!(n.pool.pool_size().pending, pool0, "build must not evict");

    // commitment matches an independent recompute from the wire bytes
    let decoded = LeanBlock::from_wire_bytes(&wire1).unwrap();
    assert_eq!(decoded.commitment, c1);
    assert_eq!(decoded.txs.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn new_block_full_discipline() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path()).await;
    let genesis = n.head.lock().await.0;

    seed_and_submit(&n, 11, 2).await;
    let (c1, w1) = n.shim_build(genesis, 1, 1000, 150_000_000).await.unwrap();
    assert_eq!(n.shim_new(&w1).await.unwrap(), NewBlockOutcome::Valid(c1));

    seed_and_submit(&n, 12, 1).await;
    let (c2, w2) = n.shim_build(c1, 2, 2000, 150_000_000).await.unwrap();
    assert_eq!(n.shim_new(&w2).await.unwrap(), NewBlockOutcome::Valid(c2));
    let digest = n.state.lock().await.digest();

    // 1) re-feed head: idempotent, same commitment, state untouched
    assert_eq!(n.shim_new(&w2).await.unwrap(), NewBlockOutcome::Valid(c2));
    // 2) feed ancestor: idempotent
    assert_eq!(n.shim_new(&w1).await.unwrap(), NewBlockOutcome::Valid(c1));
    assert_eq!(n.state.lock().await.digest(), digest);
    // 3) conflicting block at a known height: loud error
    let fork = LeanBlock::new(genesis, 1, 999, vec![]);
    let e = n.shim_new(&fork.to_wire_bytes()).await.unwrap_err();
    assert!(e.contains("conflicting"), "{e}");
    // 4) gap (head+2): SYNCING, queued, head untouched (behind-the-tip machinery)
    let gap = LeanBlock::new(c2, 4, 3000, vec![]);
    assert_eq!(
        n.shim_new(&gap.to_wire_bytes()).await.unwrap(),
        NewBlockOutcome::Syncing { head: 2 }
    );
    // 5) parent mismatch at head+1: SYNCING too (queued until its parent shows up)
    let bad = LeanBlock::new(B256::repeat_byte(9), 3, 3000, vec![]);
    assert_eq!(
        n.shim_new(&bad.to_wire_bytes()).await.unwrap(),
        NewBlockOutcome::Syncing { head: 2 }
    );
    assert_eq!(n.head.lock().await.1, 2, "SYNCING must not move head");
    // 6) trailing bytes: clean error
    let mut w = LeanBlock::new(c2, 3, 3000, vec![]).to_wire_bytes();
    w.push(0);
    assert!(n.shim_new(&w).await.is_err());
    // 7) empty heartbeat block: fine
    let hb = LeanBlock::new(c2, 3, 3000, vec![]);
    assert_eq!(n.shim_new(&hb.to_wire_bytes()).await.unwrap(), NewBlockOutcome::Valid(hb.commitment));
    // 8) sync serving returns byte-identical wire form
    let served = n.block_wire_bytes(1).await.unwrap().unwrap();
    assert_eq!(served, w1);
    assert!(n.block_wire_bytes(99).await.unwrap().is_none());
    assert_eq!(n.state.lock().await.digest(), digest, "errors must never mutate state");
}
