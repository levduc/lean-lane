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
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let (c1, w1) = advance(&n, 4, 0).await;
    let (c2, w2) = advance(&n, 4, 1).await;
    let (c3, w3) = advance(&n, 4, 2).await;
    n.by_commitment.lock().await.clear();
    // A cold index means the index is not COMPLETE down to these heights —
    // that is what index_floor states, and what makes the scan worth running.
    // (In production the only cold range is below the snapshot a process
    // opened on; above index_floor a miss is proof of absence and the scan is
    // skipped, which is the whole point of the floor.)
    let head_n = n.head.lock().await.1;
    n.index_floor.store(head_n + 1, Ordering::Relaxed);
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
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    advance(&n, 5, 0).await;
    advance(&n, 5, 1).await;
    let unknown = B256::repeat_byte(9);
    assert_eq!(n.block_wire_bytes_by_commitment(unknown).await.unwrap(), None);
    assert!(n.scan_misses.lock().await.contains(&unknown));
    let scans = n.stats.log_scans.load(Ordering::Relaxed);
    assert_eq!(n.block_wire_bytes_by_commitment(unknown).await.unwrap(), None);
    assert_eq!(
        n.stats.log_scans.load(Ordering::Relaxed),
        scans,
        "a cached miss must not scan the log again"
    );
    // Appending an UNRELATED block must NOT drop the cached miss: a miss for
    // commitment c can only turn into a hit when c itself is appended, so the
    // cache survives (the old blanket clear() made it useless in steady state).
    advance(&n, 5, 2).await;
    assert!(
        n.scan_misses.lock().await.contains(&unknown),
        "an unrelated append cannot turn a miss into a hit"
    );
    let scans = n.stats.log_scans.load(Ordering::Relaxed);
    assert_eq!(n.block_wire_bytes_by_commitment(unknown).await.unwrap(), None);
    assert_eq!(n.stats.log_scans.load(Ordering::Relaxed), scans, "still cached after an append");
}

#[tokio::test(flavor = "multi_thread")]
async fn appending_a_missed_commitment_drops_its_cached_miss() {
    // The one case where a miss DOES become a hit: the missed commitment is
    // itself appended. Its entry must be gone and the bytes must be served.
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    advance(&n, 7, 0).await;
    seed_and_submit(&n, 7, 1, 2).await;
    let (head_c, head_n, ts) = *n.head.lock().await;
    // built but NOT staged: unknown to the index, the staged map and the queue
    let (c, wire) = n.shim_build(head_c, head_n + 1, ts + 1000, 1_000_000).await.unwrap();
    assert_eq!(n.block_wire_bytes_by_commitment(c).await.unwrap(), None);
    assert!(n.scan_misses.lock().await.contains(&c));
    assert_eq!(n.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(c));
    assert!(
        !n.scan_misses.lock().await.contains(&c),
        "appending the missed commitment must drop its cached miss"
    );
    assert_eq!(n.block_wire_bytes_by_commitment(c).await.unwrap(), Some(wire));
}

#[tokio::test(flavor = "multi_thread")]
async fn new_block_by_commitment_promotes_a_staged_block() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    seed_and_submit(&n, 4, 0, 2).await;
    let (head_c, head_n, ts) = *n.head.lock().await;
    let (c, wire) = n.shim_build(head_c, head_n + 1, ts + 1000, 1_000_000).await.unwrap();
    assert_eq!(n.stage_block(&wire).await.unwrap(), StageOutcome::Staged(c));
    assert_eq!(n.shim_new_by_commitment(c).await.unwrap(), NewBlockOutcome::Valid(c));
    assert_eq!(n.head.lock().await.0, c);
    // idempotent: already canonical
    assert_eq!(n.shim_new_by_commitment(c).await.unwrap(), NewBlockOutcome::Valid(c));
}

#[tokio::test(flavor = "multi_thread")]
async fn new_block_by_unknown_commitment_is_syncing() {
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    let head_n = n.head.lock().await.1;
    assert_eq!(
        n.shim_new_by_commitment(B256::repeat_byte(7)).await.unwrap(),
        NewBlockOutcome::Syncing { head: head_n }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn staged_entry_survives_an_unrelated_head_move() {
    // Stage block N+1 candidate A, then apply a different N+1 (B). A's
    // bytes must still be served by commitment (it is a losing candidate,
    // a peer may still ask for it), and promoting A must now answer
    // SYNCING, not panic.
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    seed_and_submit(&n, 5, 0, 2).await;
    let (head_c, head_n, ts) = *n.head.lock().await;
    let (ca, wa) = n.shim_build(head_c, head_n + 1, ts + 1000, 1_000_000).await.unwrap();
    assert_eq!(n.stage_block(&wa).await.unwrap(), StageOutcome::Staged(ca));
    let (cb, wb) = n.shim_build(head_c, head_n + 1, ts + 2000, 1_000_000).await.unwrap();
    assert_ne!(ca, cb);
    assert_eq!(n.shim_new(&wb).await.unwrap(), NewBlockOutcome::Valid(cb));
    assert_eq!(n.block_wire_bytes_by_commitment(ca).await.unwrap(), Some(wa));
    assert!(matches!(n.shim_new_by_commitment(ca).await, Err(_) | Ok(NewBlockOutcome::Syncing { .. })));
}

#[tokio::test(flavor = "multi_thread")]
async fn announce_never_scans_the_log() {
    // A peer announcing a commitment we have never seen must cost us one head
    // read, not a backward log scan: the announce path decides on NUMBERS
    // alone. (Fleet symptom this guards: lean-node CPU per block growing ~3x
    // on peered validators while a lone-fast one stayed flat.)
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    for nonce in 0..3 {
        advance(&n, 8, nonce).await;
    }
    let head_n = n.head.lock().await.1;
    // unknown commitment, ahead of us (the SYNCING-arming case) ...
    n.on_announce(B256::repeat_byte(0xa1), head_n + 1).await;
    // ... and one at a height we already have (the in-sync case)
    n.on_announce(B256::repeat_byte(0xa2), head_n).await;
    assert_eq!(
        n.stats.log_scans.load(Ordering::Relaxed),
        0,
        "the announce path must never scan the log"
    );
    assert_eq!(n.stats.announces_rx.load(Ordering::Relaxed), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn serving_an_unknown_commitment_does_not_scan_above_the_index_floor() {
    // arc_getBlockBytes{commitment} for something we do not have: the index is
    // complete from index_floor up, so there is nothing to scan. This is the
    // per-ask cost that used to read+hash up to 1024 blocks.
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    for nonce in 0..3 {
        advance(&n, 9, nonce).await;
    }
    assert_eq!(
        n.block_wire_bytes_by_commitment_for(B256::repeat_byte(0xb7), "get_bytes").await.unwrap(),
        None
    );
    assert_eq!(n.stats.log_scans.load(Ordering::Relaxed), 0, "nothing to scan above the floor");
}

#[tokio::test(flavor = "multi_thread")]
async fn anchoring_a_staged_block_never_scans_the_log() {
    // The decide anchor is the hot path: it must resolve from the staged copy
    // and never fall into the bounded backward log scan (which reads and
    // hashes up to 1024 blocks and was measured to grow ~1.1 ms per block of
    // history when it ran on every decide).
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    for nonce in 0..3 {
        seed_and_submit(&n, 6, nonce, 2).await;
        let (head_c, head_n, ts) = *n.head.lock().await;
        let (c, wire) = n.shim_build(head_c, head_n + 1, ts + 1000, 1_000_000).await.unwrap();
        assert_eq!(n.stage_block(&wire).await.unwrap(), StageOutcome::Staged(c));
        assert_eq!(n.shim_new_by_commitment(c).await.unwrap(), NewBlockOutcome::Valid(c));
    }
    assert_eq!(n.stats.log_scans.load(Ordering::Relaxed), 0, "anchor path must not scan the log");
    // and the idempotent re-anchor of a canonical block is index-only too
    let head_c = n.head.lock().await.0;
    assert_eq!(n.shim_new_by_commitment(head_c).await.unwrap(), NewBlockOutcome::Valid(head_c));
    assert_eq!(n.stats.log_scans.load(Ordering::Relaxed), 0);
}
