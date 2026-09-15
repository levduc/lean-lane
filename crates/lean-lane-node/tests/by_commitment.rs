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
    let (c, wire) = n.shim_build(head_c, head_n + 1, ts + 1000, 1_000_000).await.unwrap();
    // `arc_buildBlock` self-stages its result; drop the staged copy, because
    // this test needs a commitment that really is unknown to the index, the
    // staged map AND the queue — that is what makes the lookup a cached miss.
    n.staged.lock().await.clear();
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

#[tokio::test(flavor = "multi_thread")]
async fn a_built_block_anchors_from_its_own_staged_copy() {
    // THE proposer path (2026-09-15). `arc_buildBlock` used to execute on a
    // throwaway copy, so at decide the proposer's own
    // `arc_newBlock{commitment}` found nothing staged and pulled its own block
    // back from a peer over HTTP (`path=peer`, 150-300 ms, on the consensus
    // critical path). Build must now leave a staged entry, and the anchor must
    // take the promote path with zero peer traffic and zero log scans.
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    for nonce in 0..3u32 {
        seed_and_submit(&n, 21, nonce, 2).await;
        let (head_c, head_n, ts) = *n.head.lock().await;
        let (c, _wire) = n.shim_build(head_c, head_n + 1, ts + 1000, 1_000_000).await.unwrap();
        assert!(
            n.staged.lock().await.iter().any(|(k, _)| *k == c),
            "arc_buildBlock must stage its own result"
        );
        // the CL only ever hands back the commitment
        assert_eq!(n.shim_new_by_commitment(c).await.unwrap(), NewBlockOutcome::Valid(c));
        assert_eq!(n.head.lock().await.0, c);
    }
    assert_eq!(
        n.stats.promoted.load(Ordering::Relaxed),
        3,
        "every self-built block must anchor through promote_staged (path=staged)"
    );
    assert_eq!(
        n.stats.block_pulls.load(Ordering::Relaxed),
        0,
        "the proposer must never fetch its own block from a peer"
    );
    assert_eq!(n.stats.log_scans.load(Ordering::Relaxed), 0, "and never scan the log");
    // staged map drained by the retention sweep, not leaking an entry a height
    assert!(n.staged.lock().await.len() <= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn staging_a_self_built_block_again_is_a_no_op() {
    // A proposer's CL may also `arc_stageBlock` the bytes it just built. That
    // must not re-execute: the entry already there is a post-state of the same
    // parent over the same bytes.
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    seed_and_submit(&n, 22, 0, 2).await;
    let (head_c, head_n, ts) = *n.head.lock().await;
    let (c, wire) = n.shim_build(head_c, head_n + 1, ts + 1000, 1_000_000).await.unwrap();
    let staged_before: Vec<B256> =
        n.staged.lock().await.iter().map(|(k, _)| *k).collect();
    assert_eq!(staged_before, vec![c]);

    assert_eq!(n.stage_block(&wire).await.unwrap(), StageOutcome::Staged(c));
    // the no-op leaves exactly one entry for this commitment, still on this head
    let q = n.staged.lock().await;
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].0, c);
    drop(q);

    // build -> stage -> newBlock still works, and still promotes
    use std::sync::atomic::Ordering;
    assert_eq!(n.shim_new_by_commitment(c).await.unwrap(), NewBlockOutcome::Valid(c));
    assert_eq!(n.head.lock().await.0, c);
    assert_eq!(n.stats.promoted.load(Ordering::Relaxed), 1);
    assert_eq!(n.stats.block_pulls.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn self_staged_build_matches_the_direct_path_byte_for_byte() {
    // Differential: a chain anchored through the build-self-stage promote path
    // must produce the same head, state digest, receipts and LOG BYTES as the
    // same blocks fed through the full `arc_newBlock{bytes}` path.
    use std::sync::atomic::Ordering;
    let dp = tempfile::tempdir().unwrap(); // proposer (build -> by-commitment)
    let dd = tempfile::tempdir().unwrap(); // follower (bytes)
    let p = node(dp.path());
    let d = node(dd.path());
    for nonce in 0..6u32 {
        seed_and_submit(&p, 23, nonce, 3).await;
        seed_and_submit(&d, 23, nonce, 3).await;
        let (head_c, head_n, ts) = *p.head.lock().await;
        let (c, wire) = p.shim_build(head_c, head_n + 1, ts + 1000, 1_000_000).await.unwrap();
        assert_eq!(p.shim_new_by_commitment(c).await.unwrap(), NewBlockOutcome::Valid(c));
        assert_eq!(d.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(c));
        assert_eq!(*p.head.lock().await, *d.head.lock().await, "head diverged at {nonce}");
        assert_eq!(
            p.state.lock().await.digest(),
            d.state.lock().await.digest(),
            "state diverged at {nonce}"
        );
    }
    assert_eq!(p.stats.promoted.load(Ordering::Relaxed), 6, "all six via promote_staged");
    assert_eq!(d.stats.promoted.load(Ordering::Relaxed), 0, "the follower used the full path");
    let rp: Vec<_> = p.receipts.lock().await.iter().cloned().collect();
    let rd: Vec<_> = d.receipts.lock().await.iter().cloned().collect();
    assert_eq!(rp, rd, "receipts diverged");
    assert_eq!(
        std::fs::read(dp.path().join("blocks.log")).unwrap(),
        std::fs::read(dd.path().join("blocks.log")).unwrap(),
        "log files must be byte-identical"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn self_staging_respects_the_32_entry_cap() {
    // Retention/cap unchanged: repeated builds at the same height (each a
    // distinct candidate) must never grow the staged map past 32.
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path());
    seed_and_submit(&n, 24, 0, 2).await;
    let (head_c, head_n, _) = *n.head.lock().await;
    for i in 0..40u64 {
        // distinct timestamps => distinct commitments => distinct candidates
        n.shim_build(head_c, head_n + 1, 1_000 + i, 1_000_000).await.unwrap();
    }
    assert_eq!(n.staged.lock().await.len(), 32, "staged map stays bounded at 32");
    // and an identical rebuild replaces rather than duplicates
    let before = n.staged.lock().await.len();
    let (c, _) = n.shim_build(head_c, head_n + 1, 1_039, 1_000_000).await.unwrap();
    let q = n.staged.lock().await;
    assert_eq!(q.len(), before);
    assert_eq!(q.iter().filter(|(k, _)| *k == c).count(), 1);
}
