//! Peer pulls at scale: shuffled order, `peer_fanout` peers raced, first
//! VALID answer wins. Driven through `arc_newBlock{commitment}`'s peer
//! fallback against fake peers that hang, lie, or serve.

use alloy_primitives::B256;
use base64::Engine;
use jsonrpsee::server::{RpcModule, Server};
use jsonrpsee::types::ErrorObjectOwned;
use lean_lane_node::chain::{genesis_commitment, LeanBlock};
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
enum Behaviour {
    /// Serve the requested block (if known) after this many ms.
    Serve(u64),
    /// Never answer (the client's 4 s request timeout fires first).
    Hang,
    /// Answer every request instantly with a DIFFERENT, well-formed block.
    Wrong,
    /// Answer every request instantly with bytes that are not a block.
    Garbage,
}

struct FakePeer {
    behaviour: Behaviour,
    blocks: Arc<Mutex<HashMap<B256, Vec<u8>>>>,
    hits: AtomicUsize,
}

/// Start one fake lean peer serving `arc_getBlockBytes{commitment}`.
async fn fake_peer(
    behaviour: Behaviour,
    blocks: Arc<Mutex<HashMap<B256, Vec<u8>>>>,
) -> (String, Arc<FakePeer>, jsonrpsee::server::ServerHandle) {
    let peer = Arc::new(FakePeer {
        behaviour,
        blocks,
        hits: AtomicUsize::new(0),
    });
    let mut m = RpcModule::new(peer.clone());
    m.register_async_method("arc_getBlockBytes", |params, peer, _| async move {
        peer.hits.fetch_add(1, Ordering::SeqCst);
        let p: serde_json::Value = params.parse()?;
        let c: B256 = p["commitment"]
            .as_str()
            .unwrap_or_default()
            .parse()
            .unwrap_or_default();
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let bytes = match peer.behaviour {
            Behaviour::Hang => {
                tokio::time::sleep(Duration::from_secs(60)).await;
                None
            }
            Behaviour::Wrong => Some(LeanBlock::new(B256::ZERO, 1, 424242, vec![]).to_wire_bytes()),
            Behaviour::Garbage => Some(vec![0xde, 0xad, 0xbe, 0xef]),
            Behaviour::Serve(ms) => {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                peer.blocks.lock().unwrap().get(&c).cloned()
            }
        };
        Ok::<_, ErrorObjectOwned>(match bytes {
            Some(b) => serde_json::json!({ "blockBytes": b64(&b) }),
            None => serde_json::Value::Null,
        })
    })
    .unwrap();
    let server = Server::builder().build("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    (format!("http://{addr}"), peer, server.start(m))
}

struct Fleet {
    urls: Vec<String>,
    peers: Vec<Arc<FakePeer>>,
    blocks: Arc<Mutex<HashMap<B256, Vec<u8>>>>,
    _handles: Vec<jsonrpsee::server::ServerHandle>,
}

async fn fleet(behaviours: &[Behaviour]) -> Fleet {
    let blocks = Arc::new(Mutex::new(HashMap::new()));
    let (mut urls, mut peers, mut handles) = (vec![], vec![], vec![]);
    for b in behaviours {
        let (u, p, h) = fake_peer(*b, blocks.clone()).await;
        urls.push(u);
        peers.push(p);
        handles.push(h);
    }
    Fleet {
        urls,
        peers,
        blocks,
        _handles: handles,
    }
}

impl Fleet {
    fn publish(&self, b: &LeanBlock) {
        self.blocks
            .lock()
            .unwrap()
            .insert(b.commitment, b.to_wire_bytes());
    }
}

fn node(dir: &std::path::Path, peers: &[String], peer_fanout: usize) -> LaneNode {
    LaneNode::open(Config {
        datadir: dir.to_path_buf(),
        peers: peers.to_vec(),
        peer_fanout,
        // nothing is ever staged here: go straight to the peers
        anchor_grace_ms: 0,
        ..Default::default()
    })
    .unwrap()
    .0
}

/// Two of five peers hang. The old walk asked `--peers` in order with a 4 s
/// timeout each, so peers 0 and 1 cost 8 s before peer 2 answered. Raced
/// three at a time in a shuffled order, at least one live peer is always in
/// the first wave: the pull finishes in about one live round trip — far
/// under one timeout, whatever the shuffle picked.
#[tokio::test(flavor = "multi_thread")]
async fn hanging_peers_cost_at_most_one_timeout_not_one_each() {
    use Behaviour::*;
    let f = fleet(&[Hang, Hang, Serve(20), Serve(20), Serve(20)]).await;
    let genesis = genesis_commitment(1338);
    for i in 0..4u64 {
        let dir = tempfile::tempdir().unwrap();
        let n = node(dir.path(), &f.urls, 3);
        let b = LeanBlock::new(genesis, 1, 1_000 + i, vec![]);
        f.publish(&b);
        let t0 = Instant::now();
        let out = n.shim_new_by_commitment(b.commitment).await.unwrap();
        let took = t0.elapsed();
        assert_eq!(out, NewBlockOutcome::Valid(b.commitment));
        assert!(
            took < Duration::from_secs(2),
            "pull took {took:?} (sequential would be >= 8 s)"
        );
        assert_eq!(n.head.lock().await.0, b.commitment);
    }
}

/// A peer that answers instantly with the wrong block (or with garbage) must
/// not win the race against slower honest peers: the node recomputes the
/// commitment from the bytes and only accepts the one it asked for.
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_answer_never_wins_the_race() {
    use Behaviour::*;
    let f = fleet(&[Wrong, Garbage, Serve(100), Serve(100), Serve(100)]).await;
    let genesis = genesis_commitment(1338);
    for i in 0..8u64 {
        let dir = tempfile::tempdir().unwrap();
        let n = node(dir.path(), &f.urls, 3);
        let b = LeanBlock::new(genesis, 1, 2_000 + i, vec![]);
        f.publish(&b);
        let out = n.shim_new_by_commitment(b.commitment).await.unwrap();
        assert_eq!(out, NewBlockOutcome::Valid(b.commitment));
        assert_eq!(n.head.lock().await.0, b.commitment);
        assert_eq!(n.head.lock().await.1, 1);
    }
    // the liars were asked (they are in the shuffle) but never accepted
    let liars = f.peers[0].hits.load(Ordering::SeqCst) + f.peers[1].hits.load(Ordering::SeqCst);
    assert!(liars > 0, "the shuffle never reached a liar in 8 pulls");
}

/// Every node used to ask peer[0] first. With `peer_fanout = 1` each pull
/// asks exactly one (honest) peer, so the hit distribution over many pulls
/// shows the order is re-shuffled per request.
#[tokio::test(flavor = "multi_thread")]
async fn peer_order_differs_across_requests() {
    use Behaviour::*;
    let f = fleet(&[Serve(0), Serve(0), Serve(0), Serve(0), Serve(0)]).await;
    let dir = tempfile::tempdir().unwrap();
    let n = node(dir.path(), &f.urls, 1);
    const PULLS: u64 = 20;
    for i in 0..PULLS {
        // height 2 on an unknown parent: pulled, verified, then queued as
        // SYNCING — so the node never learns it and every pull is fresh
        let b = LeanBlock::new(B256::with_last_byte(i as u8 + 1), 2, 3_000 + i, vec![]);
        f.publish(&b);
        let out = n.shim_new_by_commitment(b.commitment).await.unwrap();
        assert_eq!(out, NewBlockOutcome::Syncing { head: 0 });
    }
    let hits: Vec<usize> = f
        .peers
        .iter()
        .map(|p| p.hits.load(Ordering::SeqCst))
        .collect();
    assert_eq!(
        hits.iter().sum::<usize>(),
        PULLS as usize,
        "fanout 1: one ask per pull"
    );
    let used = hits.iter().filter(|h| **h > 0).count();
    assert!(used >= 2, "every pull went to the same peer: {hits:?}");
}
