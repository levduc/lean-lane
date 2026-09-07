//! Shim v1.3 gates — deferred anchor execution.
//!
//! DIFFERENTIAL (consensus-critical): for randomized blocks, stage+promote
//! must produce byte-identical post-state, head, receipts, and LOG-FILE BYTES
//! vs the direct arc_newBlock path. A divergence here forks the lane.

use alloy_primitives::Address;
use lean_lane_node::chain::LeanBlock;
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome, StageOutcome};
use lean_lane_node::state::Acct;
use lean_native::envelope::{current_lane_domain, LeanSigned};
use lean_native::{lean_fee, LeanTx, Output, AMOUNT_UNIT};
use secp256k1::SecretKey;
use std::sync::atomic::Ordering;
use std::time::Duration;

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

async fn open_node(dir: &std::path::Path) -> LaneNode {
    LaneNode::open(Config { datadir: dir.to_path_buf(), snapshot_every: 7, ..Default::default() })
        .unwrap()
        .0
}

async fn seed_all(nodes: &[&LaneNode], keys: u64) {
    let domain = current_lane_domain();
    for k in 0..keys {
        let probe = LeanTx::sign(0, vec![Output { to: Address::ZERO, amount: 1 }], &sk(k), &domain);
        let sender = LeanSigned::new(probe).tx().recover_sender(&domain).unwrap();
        for n in nodes {
            n.seed_account(sender, Acct { nonce: 0, balance: 200 * (lean_fee(100) + 100 * AMOUNT_UNIT) })
                .await;
        }
    }
    for n in nodes {
        let (c, num, ts) = *n.head.lock().await;
        n.state.lock().await.write_snapshot(&n.cfg.datadir, c, num, ts).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn differential_stage_promote_vs_direct() {
    let dd = tempfile::tempdir().unwrap(); // direct
    let ds = tempfile::tempdir().unwrap(); // staged
    let d = open_node(dd.path()).await;
    let s = open_node(ds.path()).await;
    let domain = current_lane_domain();
    seed_all(&[&d, &s], 40).await;
    let mut rng = Rng(0x5eed_1e13);
    let mut nonces = [0u32; 40];

    for height in 1..=30u64 {
        // randomized block: 0..6 txs, mixed fan-out incl. duplicates/self/zero
        let ntx = rng.below(7);
        let mut txs = Vec::new();
        for _ in 0..ntx {
            let k = rng.below(40) as u64;
            let n_out = 1 + rng.below(12);
            let outs: Vec<Output> = (0..n_out)
                .map(|j| Output {
                    to: Address::with_last_byte(((j * 7 + rng.below(50)) % 251) as u8),
                    amount: rng.below(9) as u64, // zero-value outputs included
                })
                .collect();
            let t = LeanTx::sign(nonces[k as usize], outs, &sk(k), &domain);
            nonces[k as usize] += 1;
            txs.push(LeanSigned::new(t).raw().to_vec());
        }
        let (hc, hn, _) = *d.head.lock().await;
        let block = LeanBlock::new(hc, hn + 1, 10_000 + height, txs);
        let wire = block.to_wire_bytes();

        // direct path
        assert!(matches!(d.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(_)));
        // staged path: stage in the "vote gap", then anchor
        assert_eq!(
            s.stage_block(&wire).await.unwrap(),
            StageOutcome::Staged(block.commitment),
            "stage must succeed at height {height}"
        );
        assert!(matches!(s.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(_)));

        assert_eq!(*d.head.lock().await, *s.head.lock().await, "head diverged at {height}");
        assert_eq!(
            d.state.lock().await.digest(),
            s.state.lock().await.digest(),
            "state diverged at {height}"
        );
        let rd: Vec<_> = d.receipts.lock().await.iter().cloned().collect();
        let rs: Vec<_> = s.receipts.lock().await.iter().cloned().collect();
        assert_eq!(rd, rs, "receipts diverged at {height}");
    }
    // every anchored block on S must have taken the fast path
    assert_eq!(s.stats.promoted.load(Ordering::Relaxed), 30, "all 30 must promote");
    // THE byte gate: identical log files
    let ld = std::fs::read(dd.path().join("blocks.log")).unwrap();
    let ls = std::fs::read(ds.path().join("blocks.log")).unwrap();
    assert_eq!(ld, ls, "log files must be byte-identical");
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_staged_entry_never_promotes() {
    let dir = tempfile::tempdir().unwrap();
    let n = open_node(dir.path()).await;
    let genesis = n.head.lock().await.0;

    // stage X against genesis
    let x = LeanBlock::new(genesis, 1, 111, vec![]);
    assert_eq!(
        n.stage_block(&x.to_wire_bytes()).await.unwrap(),
        StageOutcome::Staged(x.commitment)
    );
    // sibling Y lands first via the FULL path -> head advances -> staged map cleared
    let y = LeanBlock::new(genesis, 1, 222, vec![]);
    assert!(matches!(n.shim_new(&y.to_wire_bytes()).await.unwrap(), NewBlockOutcome::Valid(_)));
    assert!(n.staged.lock().await.is_empty(), "head advance must invalidate staged entries");
    assert_eq!(n.stats.promoted.load(Ordering::Relaxed), 0);
    // feeding X now: height 1 is KNOWN with a different block -> the loud
    // conflicting error (v1.1 contract), never a stale promote
    let e = n.shim_new(&x.to_wire_bytes()).await.unwrap_err();
    assert!(e.contains("conflicting"), "{e}");
    assert_eq!(n.head.lock().await.0, y.commitment);

    // staging when parent != head -> SYNCING, nothing queued (speculative path)
    let z = LeanBlock::new(genesis, 1, 333, vec![]);
    assert_eq!(
        n.stage_block(&z.to_wire_bytes()).await.unwrap(),
        StageOutcome::Syncing { head: 1 }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_between_stage_and_promote_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let digest0;
    let wire;
    {
        let n = open_node(dir.path()).await;
        let genesis = n.head.lock().await.0;
        let x = LeanBlock::new(genesis, 1, 444, vec![]);
        wire = x.to_wire_bytes();
        assert!(matches!(n.stage_block(&wire).await.unwrap(), StageOutcome::Staged(_)));
        digest0 = n.state.lock().await.digest();
        // CRASH here: staged data is memory-only, log untouched
    }
    let n2 = open_node(dir.path()).await;
    assert_eq!(n2.head.lock().await.1, 0, "staging must not persist anything");
    assert_eq!(n2.state.lock().await.digest(), digest0);
    // the block still applies cleanly through the full path after restart
    assert!(matches!(n2.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(_)));
    assert_eq!(n2.head.lock().await.1, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn newblock_during_inflight_staging_promotes_and_matches_direct() {
    // v1.3.1 race case: arc_newBlock arrives while staging is registered and
    // (for these 200-tx blocks, ~8ms of ecrecover) still mid-execution. It
    // must await the staging, PROMOTE, and match the direct path byte-for-byte.
    let dd = tempfile::tempdir().unwrap();
    let ds = tempfile::tempdir().unwrap();
    let d = open_node(dd.path()).await;
    let s = std::sync::Arc::new(open_node(ds.path()).await);
    let domain = current_lane_domain();
    seed_all(&[&d, &s], 200).await;
    let mut nonces = [0u32; 200];

    for height in 1..=8u64 {
        let mut txs = Vec::new();
        for k in 0..200u64 {
            let outs: Vec<Output> = (0..3)
                .map(|j| Output { to: Address::with_last_byte(((j * 7 + k as usize) % 251) as u8), amount: 1 })
                .collect();
            let t = LeanTx::sign(nonces[k as usize], outs, &sk(k), &domain);
            nonces[k as usize] += 1;
            txs.push(LeanSigned::new(t).raw().to_vec());
        }
        let (hc, hn, _) = *d.head.lock().await;
        let block = LeanBlock::new(hc, hn + 1, 20_000 + height, txs);
        let wire = block.to_wire_bytes();

        assert!(matches!(d.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(_)));

        // fire staging; wait only until it is REGISTERED (or already done),
        // then race the anchor into it
        let s2 = s.clone();
        let w2 = wire.clone();
        let stager = tokio::spawn(async move { s2.stage_block(&w2).await });
        let t0 = std::time::Instant::now();
        loop {
            let registered = s.inflight.lock().await.contains_key(&block.commitment)
                || s.staged.lock().await.iter().any(|(c, _)| *c == block.commitment);
            if registered {
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(2), "staging never registered");
            tokio::time::sleep(Duration::from_micros(200)).await;
        }
        assert!(matches!(s.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(_)));
        assert!(matches!(stager.await.unwrap().unwrap(), StageOutcome::Staged(_)));

        assert_eq!(*d.head.lock().await, *s.head.lock().await, "head diverged at {height}");
        assert_eq!(d.state.lock().await.digest(), s.state.lock().await.digest(), "state diverged at {height}");
    }
    assert_eq!(
        s.stats.promoted.load(Ordering::Relaxed),
        8,
        "every raced anchor must promote (await-in-flight or staged-map hit)"
    );
    assert_eq!(
        std::fs::read(dd.path().join("blocks.log")).unwrap(),
        std::fs::read(ds.path().join("blocks.log")).unwrap(),
        "log files must be byte-identical under the race"
    );
    let rd: Vec<_> = d.receipts.lock().await.iter().cloned().collect();
    let rs: Vec<_> = s.receipts.lock().await.iter().cloned().collect();
    assert_eq!(rd, rs, "receipts diverged under the race");
}

#[tokio::test(flavor = "multi_thread")]
async fn dead_inflight_staging_never_wedges_newblock() {
    // A staging that dies (sender dropped without signaling) must not wedge
    // arc_newBlock: the guard falls through to the full path.
    let dir = tempfile::tempdir().unwrap();
    let n = open_node(dir.path()).await;
    let genesis = n.head.lock().await.0;
    let x = LeanBlock::new(genesis, 1, 555, vec![]);

    // forge a dead in-flight entry for this commitment on this head
    let (tx, rx) = tokio::sync::watch::channel(false);
    drop(tx); // stager "panicked"
    n.inflight.lock().await.insert(x.commitment, (genesis, rx));

    let t0 = std::time::Instant::now();
    assert!(matches!(n.shim_new(&x.to_wire_bytes()).await.unwrap(), NewBlockOutcome::Valid(_)));
    assert!(t0.elapsed() < Duration::from_secs(2), "dead staging must not consume the full guard");
    assert_eq!(n.head.lock().await.1, 1);
    assert_eq!(n.stats.promoted.load(Ordering::Relaxed), 0, "must have used the full path");
}
