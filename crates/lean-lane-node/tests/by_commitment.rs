use alloy_primitives::{Address, B256};
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome, StageOutcome};
use lean_lane_node::state::Acct;
use lean_native::envelope::{current_lane_domain, LeanSigned};
use lean_native::{lean_fee, LeanTx, Output, AMOUNT_UNIT};
use secp256k1::SecretKey;

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

fn node(dir: &std::path::Path) -> LaneNode {
    LaneNode::open(Config { datadir: dir.to_path_buf(), ..Default::default() }).unwrap().0
}

async fn seed_and_submit(n: &LaneNode, key: u64, nonce: u32, outputs: usize) {
    let domain = current_lane_domain();
    let outs = (0..outputs)
        .map(|i| Output { to: Address::with_last_byte((i % 200) as u8), amount: 3 })
        .collect();
    let tx = LeanTx::sign(nonce, outs, &sk(key), &domain);
    let signed = LeanSigned::new(tx);
    let sender = signed.tx().recover_sender(&domain).unwrap();
    if nonce == 0 {
        n.seed_account(
            sender,
            Acct { nonce: 0, balance: 10 * (lean_fee(outputs) + outputs as u128 * 3 * AMOUNT_UNIT) },
        )
        .await;
    }
    n.submit_raw(signed.raw()).await.unwrap();
}

/// Build+apply one block on `n`; returns (commitment, wire bytes).
async fn advance(n: &LaneNode, key: u64, nonce: u32) -> (B256, Vec<u8>) {
    seed_and_submit(n, key, nonce, 2).await;
    let (head_c, head_n, ts) = *n.head.lock().await;
    let (c, wire) = n.shim_build(head_c, head_n + 1, ts + 1000, 1_000_000).await.unwrap();
    assert_eq!(n.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(c));
    (c, wire)
}

#[tokio::test(flavor = "multi_thread")]
async fn canonical_blocks_are_found_by_commitment() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let (c1, w1) = advance(&n, 1, 0).await;
    let (c2, w2) = advance(&n, 1, 1).await;
    assert_eq!(n.block_wire_bytes_by_commitment(c1).await.unwrap(), Some(w1));
    assert_eq!(n.block_wire_bytes_by_commitment(c2).await.unwrap(), Some(w2));
    assert_eq!(n.block_wire_bytes_by_commitment(B256::repeat_byte(9)).await.unwrap(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn staged_blocks_are_found_by_commitment() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    seed_and_submit(&n, 2, 0, 2).await;
    let (head_c, head_n, ts) = *n.head.lock().await;
    let (c, wire) = n.shim_build(head_c, head_n + 1, ts + 1000, 1_000_000).await.unwrap();
    assert_eq!(n.stage_block(&wire).await.unwrap(), StageOutcome::Staged(c));
    assert_eq!(n.block_wire_bytes_by_commitment(c).await.unwrap(), Some(wire));
}

#[tokio::test(flavor = "multi_thread")]
async fn lookup_by_commitment_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let (c1, w1) = {
        let n = node(dir.path());
        advance(&n, 3, 0).await
    };
    let n = node(dir.path());
    assert_eq!(n.block_wire_bytes_by_commitment(c1).await.unwrap(), Some(w1));
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_finds_canonical_blocks_when_the_index_is_cold() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let (c1, w1) = advance(&n, 4, 0).await;
    let (c2, w2) = advance(&n, 4, 1).await;
    let (c3, w3) = advance(&n, 4, 2).await;
    n.by_commitment.lock().await.clear();
    assert_eq!(n.block_wire_bytes_by_commitment(c1).await.unwrap(), Some(w1));
    assert_eq!(n.block_wire_bytes_by_commitment(c2).await.unwrap(), Some(w2));
    assert_eq!(n.block_wire_bytes_by_commitment(c3).await.unwrap(), Some(w3));
    let idx = n.by_commitment.lock().await;
    assert!(idx.contains_key(&c1));
    assert!(idx.contains_key(&c2));
    assert!(idx.contains_key(&c3));
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_commitment_is_cached_as_a_miss() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    advance(&n, 5, 0).await;
    advance(&n, 5, 1).await;
    let unknown = B256::repeat_byte(9);
    assert_eq!(n.block_wire_bytes_by_commitment(unknown).await.unwrap(), None);
    assert!(n.scan_misses.lock().await.contains(&unknown));
    assert_eq!(n.block_wire_bytes_by_commitment(unknown).await.unwrap(), None);
    advance(&n, 5, 2).await;
    assert!(n.scan_misses.lock().await.is_empty());
}
