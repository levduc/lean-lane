//! Asking peers for a block, and choosing whom to announce to.
//!
//! Every place that needs a block it does not hold — the anchor's peer
//! fallback (`arc_newBlock{commitment}`) and the backfill (by number) — goes
//! through `pull_block`: the peer order is SHUFFLED per request (so no peer is
//! everyone's first choice), up to `peer_fanout` peers are asked CONCURRENTLY,
//! the first answer whose bytes hash to what was asked for wins, and the rest
//! are cancelled. A peer that fails fast frees its slot for the next one in the
//! shuffled order; a peer that hangs holds its slot until its own request
//! timeout.
//!
//! The old walk asked `--peers` one at a time in fixed order with a 4 s
//! timeout each: up to 80 s at 20 peers, and every node hit peer[0] first.

use super::*;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::HttpClient;
use rand::seq::SliceRandom;
use std::time::Duration;

/// Request timeout of one peer call (the peer clients are built with it).
pub(super) const PEER_TIMEOUT: Duration = Duration::from_secs(4);

/// What to ask a peer for.
#[derive(Clone, Copy, Debug)]
pub(super) enum BlockQuery {
    Commitment(B256),
    Number(u64),
}

/// `n` peer indices in a fresh random order.
fn shuffled(n: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    order.shuffle(&mut rand::thread_rng());
    order
}

/// One `arc_getBlockBytes` call. `None` on any failure: transport error,
/// timeout, a miss (`null`), or a body that is not base64.
async fn fetch(client: HttpClient, q: BlockQuery) -> Option<Vec<u8>> {
    let mut params = jsonrpsee::core::params::ObjectParams::new();
    match q {
        BlockQuery::Commitment(c) => params.insert("commitment", format!("{c}")),
        BlockQuery::Number(n) => params.insert("number", n),
    }
    .ok()?;
    let v: serde_json::Value = client.request("arc_getBlockBytes", params).await.ok()?;
    let b64 = v.get("blockBytes")?.as_str()?;
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).ok()
}

impl LaneNode {
    /// Build one HTTP client per `--peers` url (index-aligned with it).
    pub(super) fn peer_clients(peers: &[String]) -> Vec<HttpClient> {
        peers
            .iter()
            .filter_map(|u| {
                jsonrpsee::http_client::HttpClientBuilder::default()
                    .max_request_size(64 * 1024 * 1024)
                    .request_timeout(PEER_TIMEOUT)
                    .build(u)
                    .ok()
            })
            .collect()
    }

    /// Fetch a block from peers: shuffled order, up to `cfg.peer_fanout` in
    /// flight, first answer that `accept`s wins, the rest are cancelled.
    ///
    /// `accept` sees the header `LeanBlock::commitment_of_wire` derives FROM
    /// THE BYTES (the commitment is recomputed, never taken from the peer), so
    /// a peer serving the wrong block — or a forged one — can never win.
    ///
    /// Total time is bounded by `PEER_TIMEOUT` × ⌈peers / fanout⌉ — the time
    /// the shuffled list takes if every peer hangs — which is never more than
    /// the old sequential walk's peers × `PEER_TIMEOUT`, and ONE timeout when
    /// fanout ≥ peers.
    pub(super) async fn pull_block(
        &self,
        q: BlockQuery,
        accept: impl Fn(&crate::chain::WireHeader) -> bool,
    ) -> Option<Vec<u8>> {
        let n = self.peer_clients.len();
        if n == 0 {
            return None;
        }
        let fanout = self.cfg.peer_fanout.clamp(1, n);
        let budget = PEER_TIMEOUT * n.div_ceil(fanout) as u32;
        let deadline = tokio::time::Instant::now() + budget;
        let mut order = shuffled(n).into_iter();
        // Dropping the set on return aborts every request still in flight.
        let mut inflight = tokio::task::JoinSet::new();
        let mut launch = |set: &mut tokio::task::JoinSet<_>| {
            if let Some(i) = order.next() {
                set.spawn(fetch(self.peer_clients[i].clone(), q));
            }
        };
        for _ in 0..fanout {
            launch(&mut inflight);
        }
        loop {
            let done = tokio::time::timeout_at(deadline, inflight.join_next()).await;
            match done {
                Err(_) | Ok(None) => return None, // budget spent / every peer missed
                Ok(Some(res)) => {
                    if let Ok(Some(bytes)) = res {
                        match LeanBlock::commitment_of_wire(&bytes) {
                            Ok(h) if accept(&h) => return Some(bytes),
                            _ => {} // wrong block or garbage: ignore this peer
                        }
                    }
                    launch(&mut inflight);
                }
            }
        }
    }

    /// Who hears about a block we just appended: every peer by default,
    /// or a fresh random `--announce-fanout` subset of them.
    pub(super) fn announce_targets(&self) -> Vec<HttpClient> {
        match self.cfg.announce_fanout {
            Some(k) if k < self.peer_clients.len() => self
                .peer_clients
                .choose_multiple(&mut rand::thread_rng(), k)
                .cloned()
                .collect(),
            _ => self.peer_clients.clone(),
        }
    }
}
