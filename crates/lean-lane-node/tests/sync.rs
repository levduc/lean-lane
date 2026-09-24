//! Behind-the-tip machinery: the SYNCING queue and its drain, backfill over
//! real RPC after a kill/restart, and the in-sync bandwidth invariant.

mod common;

use common::*;
use lean_lane_node::chain::LeanBlock;
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome};
use lean_lane_node::rpc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread")]
async fn syncing_roundtrip_queue_then_drain() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    let b1 = LeanBlock::new(head(&n).await.0, 1, 100, vec![]);
    let b2 = LeanBlock::new(b1.commitment, 2, 200, vec![]);
    // b2 first: unknown parent -> SYNCING, head unmoved, queued
    assert_eq!(
        n.shim_new(&b2.to_wire_bytes()).await.unwrap(),
        NewBlockOutcome::Syncing { head: 0 }
    );
    assert_eq!(n.sync_queue.lock().await.len(), 1);
    // the parent arrives: it applies AND drains b2
    assert_eq!(
        n.shim_new(&b1.to_wire_bytes()).await.unwrap(),
        NewBlockOutcome::Valid(b1.commitment)
    );
    assert_eq!(head(&n).await.0, b2.commitment);
    assert!(n.sync_queue.lock().await.is_empty());
    assert_eq!(records_on_disk(dir.path()), 2);
}

async fn boot(
    dir: &std::path::Path,
    port: u16,
    peers: Vec<String>,
) -> (Arc<LaneNode>, jsonrpsee::server::ServerHandle) {
    let n = open_cfg(Config {
        snapshot_every: 16,
        peers,
        ..cfg(dir)
    });
    n.spawn_sync();
    let (_, h) = rpc::serve(n.clone(), ([127, 0, 0, 1], port).into())
        .await
        .unwrap();
    (n, h)
}

async fn converged(n: &LaneNode, target: &LaneNode, secs: u64) -> bool {
    let t0 = Instant::now();
    while head(n).await != head(target).await {
        if t0.elapsed() > Duration::from_secs(secs) {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    true
}

/// A/B/C fully meshed. Kill C, advance A by 50 blocks, restart C: C must
/// converge through announce + backfill with no external feeding.
#[tokio::test(flavor = "multi_thread")]
async fn three_node_kill_restart_converges_via_backfill() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    // fixed ports so C's peer urls survive its restart
    let base = 39160u16;
    let url = |p: u16| format!("http://127.0.0.1:{p}");
    let peers_of = |i: u16| (0..3).filter(|j| *j != i).map(|j| url(base + j)).collect();
    let (a, _ha) = boot(dirs[0].path(), base, peers_of(0)).await;
    let (b, _hb) = boot(dirs[1].path(), base + 1, peers_of(1)).await;
    let (c, hc) = boot(dirs[2].path(), base + 2, peers_of(2)).await;
    // genesis: every sender funded on every node, persisted
    for n in [&a, &b, &c] {
        for i in 0..60 {
            fund(n, i, cost(1, 3)).await;
        }
        snapshot_now(n).await;
    }
    let step = |raw: Option<u64>, ts: u64| {
        let a = a.clone();
        async move {
            if let Some(k) = raw {
                a.submit_raw(signed(k, 0, 1, 3).raw()).await.unwrap();
            }
            let (hc, hn, _) = head(&a).await;
            let (_, wire) = a.shim_build(hc, hn + 1, ts, 150_000_000).await.unwrap();
            assert!(matches!(
                a.shim_new(&wire).await.unwrap(),
                NewBlockOutcome::Valid(_)
            ));
        }
    };
    for i in 0..10 {
        step(Some(i), 1000 + i).await;
    }
    assert!(converged(&b, &a, 10).await, "B follows");
    assert!(converged(&c, &a, 10).await, "C follows");

    hc.stop().unwrap();
    drop(c);
    tokio::time::sleep(Duration::from_millis(200)).await;
    for i in 10..60 {
        step(Some(i), 1000 + i).await;
    }
    assert!(converged(&b, &a, 10).await, "B stays in lockstep");

    let (c2, _hc2) = boot(dirs[2].path(), base + 2, peers_of(2)).await;
    assert_eq!(head(&c2).await.1, 10, "C restarts at its pre-kill head");
    step(None, 2000).await; // the trigger: an empty heartbeat block
    assert!(converged(&c2, &a, 30).await, "C converges via backfill");
    assert_eq!(head(&a).await.1, 61);
    assert_eq!(digest(&a).await, digest(&c2).await);
    assert_eq!(records_on_disk(dirs[2].path()), 61);
}

/// Peers in lockstep (each CL anchors every block before the announcement
/// lands) exchange ONE small announcement per append and zero block bytes.
#[tokio::test(flavor = "multi_thread")]
async fn in_sync_peers_exchange_announcements_only() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let base = 39170u16;
    let (b, _hb) = boot(db.path(), base + 1, vec![]).await;
    let (a, _ha) = boot(
        da.path(),
        base,
        vec![format!("http://127.0.0.1:{}", base + 1)],
    )
    .await;
    for height in 1..=100u64 {
        let (hc, hn, _) = head(&a).await;
        let (_, wire) = a.shim_build(hc, hn + 1, height, 150_000_000).await.unwrap();
        // the CL anchors B first, then A appends and announces
        assert!(matches!(
            b.shim_new(&wire).await.unwrap(),
            NewBlockOutcome::Valid(_)
        ));
        assert!(matches!(
            a.shim_new(&wire).await.unwrap(),
            NewBlockOutcome::Valid(_)
        ));
    }
    let t0 = Instant::now();
    while get(&b.metrics.announces_rx) < 100 {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "announcements must arrive"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(get(&b.metrics.block_pulls), 0, "zero block bytes in sync");
    assert_eq!(
        get(&b.metrics.announces_rx),
        get(&b.metrics.announces_known)
    );
    assert_eq!(head(&a).await, head(&b).await);
}
