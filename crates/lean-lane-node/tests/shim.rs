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
    LaneNode::open(Config {
        datadir: dir.to_path_buf(),
        ..Default::default()
    })
    .unwrap()
    .0
}

async fn seed_and_submit(n: &LaneNode, key: u64, outputs: usize) {
    let domain = current_lane_domain();
    let outs = (0..outputs)
        .map(|i| Output {
            to: Address::with_last_byte((i % 200) as u8),
            amount: 3,
        })
        .collect();
    let tx = LeanTx::sign(0, outs, &sk(key), &domain);
    let signed = LeanSigned::new(tx);
    let sender = signed.tx().recover_sender(&domain).unwrap();
    n.seed_account(
        sender,
        Acct {
            nonce: 0,
            balance: lean_fee(outputs) + outputs as u128 * 3 * AMOUNT_UNIT,
        },
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
    assert!(n
        .shim_build(B256::ZERO, head_n + 1, 1000, 150_000_000)
        .await
        .is_err());
    // wrong number rejected (deviation from the doc, checked loudly)
    assert!(n
        .shim_build(head_c, head_n + 2, 1000, 150_000_000)
        .await
        .is_err());

    // valid build: deterministic, never mutates the chain (it DOES leave a
    // memory-only staged entry — see by_commitment.rs)
    let (c1, wire1) = n
        .shim_build(head_c, head_n + 1, 1000, 150_000_000)
        .await
        .unwrap();
    let (c2, wire2) = n
        .shim_build(head_c, head_n + 1, 1000, 150_000_000)
        .await
        .unwrap();
    assert_eq!(c1, c2, "same inputs + same pool snapshot => same block");
    assert_eq!(wire1, wire2);
    assert_eq!(
        *n.head.lock().await,
        (head_c, head_n, 0),
        "build must not advance head"
    );
    assert_eq!(
        n.state.lock().await.digest(),
        digest0,
        "build must not touch state"
    );
    assert_eq!(n.pool.pool_size().pending, pool0, "build must not evict");
    {
        let q = n.staged.lock().await;
        assert_eq!(
            q.len(),
            1,
            "an identical rebuild replaces, never duplicates"
        );
        assert_eq!(q[0].0, c1);
    }

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
    assert_eq!(
        n.shim_new(&hb.to_wire_bytes()).await.unwrap(),
        NewBlockOutcome::Valid(hb.commitment)
    );
    // 8) sync serving returns byte-identical wire form
    let served = n.block_wire_bytes(1).await.unwrap().unwrap();
    assert_eq!(served, w1);
    assert!(n.block_wire_bytes(99).await.unwrap().is_none());
    assert_eq!(
        n.state.lock().await.digest(),
        digest,
        "errors must never mutate state"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stats_line_carries_every_growth_field() {
    // The LEAN_STATS line is the fleet's only view of what grows over a run,
    // so its field set is a contract: a missing key silently blinds the next
    // drift investigation.
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path()).await;
    seed_and_submit(&n, 42, 4).await;

    let line = n.stats_line().await;
    println!("{line}"); // visible with --nocapture: the line is read by humans
    assert!(line.starts_with("lean_stats "), "{line}");
    assert_eq!(
        line.split_whitespace().count(),
        18,
        "one key per field: {line}"
    );
    for key in [
        "anchor_grace_hits=",
        "anchor_grace_misses=",
        "head=",
        "accounts=",
        "staged=",
        "sync_queue=",
        "by_commitment=",
        "scan_misses=",
        "log_scans=",
        "announce_skipped_staged=",
        "backfill_pulls=",
        "stage_race_waits=",
        "pool=",
        "build_ms_last=",
        "stage_ms_last=",
        "append_ms_last=",
        "rss_mb=",
    ] {
        assert!(line.contains(key), "stats line is missing `{key}`: {line}");
    }
    assert!(line.contains("head=0"), "fresh node is at height 0: {line}");
    let pending = n.pool.pool_size().pending;
    assert_eq!(pending, 1, "the submitted tx must be pending");
    assert!(line.contains(&format!("pool={pending}")), "{line}");
    assert!(
        !line.ends_with("rss_mb=0"),
        "RSS must be readable from /proc: {line}"
    );

    // after a block: the head moved and the last-append timer is populated
    let (head_c, head_n, _) = *n.head.lock().await;
    let (c, wire) = n
        .shim_build(head_c, head_n + 1, 1_000, 150_000_000)
        .await
        .unwrap();
    assert_eq!(n.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(c));
    let line = n.stats_line().await;
    assert!(line.contains("head=1"), "{line}");
    assert!(line.contains("by_commitment=1"), "{line}");
    // the timers are sub-millisecond on a 1-tx block, so check the counters
    // the line formats rather than the rounded text
    use std::sync::atomic::Ordering;
    assert!(
        n.stats.build_ns_last.load(Ordering::Relaxed) > 0,
        "build must be timed"
    );
    assert!(
        n.stats.append_ns_last.load(Ordering::Relaxed) > 0,
        "append must be timed"
    );
}

#[test]
fn anchor_line_carries_every_timing_field() {
    // The decide anchor's decomposition. `pre_ms` and `locate_ms` were added
    // 2026-09-15 because `total_ms` on a `path=index` height ran to 226-443 ms
    // with index_ms≈6 and decode_ms≈6 — ~380 ms that no field named. pre_ms is
    // taken at RPC-handler entry (before any lock or head read), locate_ms
    // covers the by-commitment resolve (staged -> queue -> index -> log read
    // -> bounded scan), peer_ms the `arc_getBlockBytes` round trip.
    let line = lean_lane_node::node::AnchorTimer::default().line(std::time::Instant::now());
    for key in [
        "lean_anchor ",
        "n=",
        "path=",
        "total_ms=",
        "pre_ms=",
        "locate_ms=",
        "peer_ms=",
        "wait_inflight_ms=",
        "lock_ms=",
        "apply_ms=",
        "append_ms=",
        "fsync_ms=",
        "index_ms=",
        "announce_ms=",
        "rss_mb=",
        "decode_ms=",
        "pool_ms=",
        // `t_in=` (2026-09-15): WHEN the call arrived, ms since node start, on
        // the same monotonic clock as `lean_stage`/`lean_locate`. No in-call
        // duration can show the stage-vs-anchor ARRIVAL gap; this can.
        "t_in=",
    ] {
        assert!(
            line.contains(key),
            "lean_anchor line is missing `{key}`: {line}"
        );
    }
}

/// `--receipt-ring N` bounds the in-memory receipt ring, on BOTH append paths.
///
/// The ring is the node's one unbounded-by-default memory sink (receipts are
/// derived, never persisted, and only `eth_getTransactionReceipt` reads them),
/// and at fan-out it is not small: 96 B per receipt plus 64 B per non-zero
/// output, so ~6.4 KB each at N=100 and ~1.3 GB at the 200,000 default.
///
/// 0 keeps none — and is the case the old `len() == cap` eviction test got
/// exactly wrong: at 0 it popped nothing, pushed one, and never matched again.
#[tokio::test(flavor = "multi_thread")]
async fn the_receipt_ring_honours_its_configured_size() {
    for (cap, want) in [(0usize, 0usize), (3, 3), (200_000, 8)] {
        let dir = tempfile::tempdir().unwrap();
        let n = LaneNode::open(Config {
            datadir: dir.path().to_path_buf(),
            receipt_ring: cap,
            ..Default::default()
        })
        .unwrap()
        .0;
        // four blocks of one tx each through `commit_block`...
        for k in 0..4u64 {
            seed_and_submit(&n, 1_000 + k, 2).await;
            n.produce_block(1_000 + k).await.unwrap().unwrap();
        }
        // ...and four through `promote_staged`
        for k in 4..8u64 {
            seed_and_submit(&n, 1_000 + k, 2).await;
            let (hc, hn, ts) = *n.head.lock().await;
            let (c, _) = n
                .shim_build(hc, hn + 1, ts + 1_000, 150_000_000)
                .await
                .unwrap();
            assert_eq!(
                n.shim_new_by_commitment(c).await.unwrap(),
                NewBlockOutcome::Valid(c)
            );
        }
        assert_eq!(n.head.lock().await.1, 8);
        assert_eq!(n.receipts.lock().await.len(), want, "receipt_ring {cap}");
    }
}
