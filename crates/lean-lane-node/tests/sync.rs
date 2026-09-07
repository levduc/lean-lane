//! Behind-the-tip machinery gates:
//!  1) SYNCING round-trip: unknown parent → SYNCING + queued → parent arrives
//!     → queued block drains and applies.
//!  2) 3-node scenario over REAL RPC: A/B/C fully meshed via --peers-style
//!     config; kill C, advance A/B by 50 blocks, restart C — C must converge
//!     via push-trigger + backfill WITHOUT any external feeding.

use alloy_primitives::Address;
use lean_lane_node::chain::LeanBlock;
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome};
use lean_lane_node::rpc;
use lean_lane_node::state::Acct;
use lean_native::envelope::{current_lane_domain, LeanSigned};
use lean_native::{lean_fee, LeanTx, Output, AMOUNT_UNIT};
use secp256k1::SecretKey;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn syncing_roundtrip_queue_then_drain() {
    let dir = tempfile::tempdir().unwrap();
    let (n, _) = LaneNode::open(Config { datadir: dir.path().into(), ..Default::default() }).unwrap();
    let genesis = n.head.lock().await.0;
    let b1 = LeanBlock::new(genesis, 1, 100, vec![]);
    let b2 = LeanBlock::new(b1.commitment, 2, 200, vec![]);

    // feed b2 FIRST: unknown parent -> SYNCING, head unmoved, queued
    assert_eq!(
        n.shim_new(&b2.to_wire_bytes()).await.unwrap(),
        NewBlockOutcome::Syncing { head: 0 }
    );
    assert_eq!(n.head.lock().await.1, 0);
    assert_eq!(n.sync_queue.lock().await.len(), 1);

    // parent arrives -> applies AND drains b2
    assert_eq!(
        n.shim_new(&b1.to_wire_bytes()).await.unwrap(),
        NewBlockOutcome::Valid(b1.commitment)
    );
    let (hc, hn, _) = *n.head.lock().await;
    assert_eq!((hc, hn), (b2.commitment, 2), "queued child must drain on parent arrival");
    assert!(n.sync_queue.lock().await.is_empty());
}

async fn open_node(
    dir: &std::path::Path,
    peers: Vec<String>,
) -> (Arc<LaneNode>, std::net::SocketAddr, jsonrpsee::server::ServerHandle) {
    let (n, _) = LaneNode::open(Config {
        datadir: dir.to_path_buf(),
        snapshot_every: 16,
        peers,
        ..Default::default()
    })
    .unwrap();
    let n = Arc::new(n);
    n.spawn_sync();
    let (addr, handle) = rpc::serve(n.clone(), ([127, 0, 0, 1], 0).into()).await.unwrap();
    (n, addr, handle)
}

#[tokio::test(flavor = "multi_thread")]
async fn three_node_kill_restart_converges_via_backfill() {
    let domain = current_lane_domain();
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let dc = tempfile::tempdir().unwrap();

    // Boot on fixed ports so peer urls survive C's restart.
    // (port 0 would re-randomize; pick a base unlikely to clash)
    let base = 39160u16;
    let url = |p: u16| format!("http://127.0.0.1:{p}");
    let boot = |dir: std::path::PathBuf, port: u16, peers: Vec<String>| async move {
        let (n, _) = LaneNode::open(Config {
            datadir: dir,
            snapshot_every: 16,
            peers,
            ..Default::default()
        })
        .unwrap();
        let n = Arc::new(n);
        n.spawn_sync();
        let (_addr, handle) = rpc::serve(n.clone(), ([127, 0, 0, 1], port).into()).await.unwrap();
        (n, handle)
    };
    let (a, _ha) = boot(da.path().into(), base, vec![url(base + 1), url(base + 2)]).await;
    let (b, _hb) = boot(db.path().into(), base + 1, vec![url(base), url(base + 2)]).await;
    let (c, hc) = boot(dc.path().into(), base + 2, vec![url(base), url(base + 1)]).await;

    // genesis: seed all senders on ALL nodes + snapshot-0 (blocks are the only
    // state source thereafter — the lockstep-gate lesson)
    let total = 60u64;
    let mut txs = Vec::new();
    for i in 0..total {
        let tx = LeanTx::sign(
            0,
            vec![Output { to: Address::with_last_byte((i % 200) as u8), amount: 5 }],
            &sk(i),
            &domain,
        );
        let signed = LeanSigned::new(tx);
        let sender = signed.tx().recover_sender(&domain).unwrap();
        let acct = Acct { nonce: 0, balance: lean_fee(1) + 5 * AMOUNT_UNIT };
        for n in [&a, &b, &c] {
            n.seed_account(sender, acct).await;
        }
        txs.push(signed.raw().to_vec());
    }
    for n in [&a, &b, &c] {
        let (cm, num, ts) = *n.head.lock().await;
        n.state.lock().await.write_snapshot(&n.cfg.datadir, cm, num, ts).unwrap();
    }

    // helper: A builds + commits one block (push-on-append fans it out)
    let advance = |a: Arc<LaneNode>, raw: Option<Vec<u8>>, ts: u64| async move {
        if let Some(r) = raw {
            a.submit_raw(&r).await.unwrap();
        }
        let (hc, hn, _) = *a.head.lock().await;
        let (_c, wire) = a.shim_build(hc, hn + 1, ts, 150_000_000).await.unwrap();
        assert!(matches!(a.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(_)));
    };

    // 10 blocks with everyone up: B and C follow via push alone
    for i in 0..10u64 {
        advance(a.clone(), Some(txs[i as usize].clone()), 1000 + i).await;
    }
    let wait_converge = |n: Arc<LaneNode>, target: Arc<LaneNode>, secs: u64| async move {
        let t0 = Instant::now();
        loop {
            let hn = *n.head.lock().await;
            let ht = *target.head.lock().await;
            if hn == ht {
                return true;
            }
            if t0.elapsed() > Duration::from_secs(secs) {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    assert!(wait_converge(b.clone(), a.clone(), 10).await, "B follows pushes");
    assert!(wait_converge(c.clone(), a.clone(), 10).await, "C follows pushes");

    // KILL C (server down, node dropped)
    hc.stop().unwrap();
    drop(c);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // advance A by 50 blocks while C is dead (pushes to C just drop)
    for i in 10..60u64 {
        advance(a.clone(), Some(txs[i as usize].clone()), 1000 + i).await;
    }
    assert!(wait_converge(b.clone(), a.clone(), 10).await, "B stays lockstep");

    // RESTART C from its datadir. It is 50 behind; the next push (from A's
    // next append) delivers a non-connecting block -> SYNCING -> backfill
    // fetches the gap from peers. NO external feeding.
    let (c2, _hc2) = boot(dc.path().into(), base + 2, vec![url(base), url(base + 1)]).await;
    assert_eq!(c2.head.lock().await.1, 10, "C restarts at its pre-kill head");
    advance(a.clone(), None, 2000).await; // the trigger block (empty heartbeat)

    assert!(
        wait_converge(c2.clone(), a.clone(), 30).await,
        "C must converge via backfill without external feeding"
    );
    let (ca, na, _) = *a.head.lock().await;
    let (cc, nc, _) = *c2.head.lock().await;
    assert_eq!((ca, na), (cc, nc));
    assert_eq!(na, 61);
    assert_eq!(
        a.state.lock().await.digest(),
        c2.state.lock().await.digest(),
        "state digests equal after self-healed catch-up"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn in_sync_peers_exchange_announcements_only() {
    // v1.2 bandwidth invariant: when peers are in lockstep (every CL anchors
    // every block into its own node before announcements land), per-append
    // peer traffic is ONE ~100-byte announcement — zero block-byte pulls.
    // Topology: A announces to B; B is fed via the CL-anchor path FIRST, so
    // every announcement from A must hit the known/ignore path.
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let base = 39170u16;
    let (b, _hb) = {
        let (n, _) = LaneNode::open(Config {
            datadir: db.path().into(),
            peers: vec![], // B pulls from nobody; a pull attempt would fail loudly anyway
            ..Default::default()
        })
        .unwrap();
        let n = Arc::new(n);
        n.spawn_sync();
        let (_a, h) = rpc::serve(n.clone(), ([127, 0, 0, 1], base + 1).into()).await.unwrap();
        (n, h)
    };
    let (a, _ha) = {
        let (n, _) = LaneNode::open(Config {
            datadir: da.path().into(),
            peers: vec![format!("http://127.0.0.1:{}", base + 1)],
            ..Default::default()
        })
        .unwrap();
        let n = Arc::new(n);
        n.spawn_sync();
        let (_a, h) = rpc::serve(n.clone(), ([127, 0, 0, 1], base).into()).await.unwrap();
        (n, h)
    };

    for height in 1..=100u64 {
        let (hc, hn, _) = *a.head.lock().await;
        let (_c, wire) = a.shim_build(hc, hn + 1, height, 150_000_000).await.unwrap();
        // CL-anchor analog on B FIRST (in-sync case), then A appends+announces
        assert!(matches!(b.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(_)));
        assert!(matches!(a.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(_)));
    }
    // let the fire-and-forget announcements land
    let t0 = Instant::now();
    while b.stats.announces_rx.load(std::sync::atomic::Ordering::Relaxed) < 100 {
        assert!(t0.elapsed() < Duration::from_secs(10), "announcements must arrive");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let rx = b.stats.announces_rx.load(std::sync::atomic::Ordering::Relaxed);
    let known = b.stats.announces_known.load(std::sync::atomic::Ordering::Relaxed);
    let pulls = b.stats.block_pulls.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(pulls, 0, "in-sync peers must exchange ZERO block bytes (pulls={pulls})");
    assert_eq!(rx, known, "every announcement must hit the known/ignore path");
    assert_eq!(*a.head.lock().await, *b.head.lock().await);
}
