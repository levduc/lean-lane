//! Minimal lane RPC: eth_sendRawTransaction (0x50 only), the binary batch
//! endpoint arc_sendRawTxBatch, txpool_status, arc_getHead, and a
//! recent-window eth_getTransactionReceipt served from the derivation ring.
//!
//! arc_sendRawTxBatch: ONE base64 string of length-framed canonical txs
//! (`[u32 LE len][tx bytes]`*). One JSON string per thousands of txs — no
//! per-tx hex, no per-tx JSON object; this is the admission-side answer to
//! the JSON-hex tax `arc_rawPayload` fixed on the payload-fetch side.

use crate::node::LaneNode;
use alloy_primitives::{hex, B256};
use base64::Engine;
use jsonrpsee::server::{RpcModule, Server, ServerConfigBuilder, ServerHandle};
use jsonrpsee::types::ErrorObjectOwned;
use reth_transaction_pool::TransactionPool;
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;

fn err(msg: impl Into<String>) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(-32000, msg.into(), None::<()>)
}

/// Raises `node.stage_pending` for the lifetime of one `arc_stageBlock` call.
///
/// It exists to cover the window between "a stage request arrived" and "the
/// stage registered its commitment in `inflight`" — JSON parse plus the base64
/// decode of a ~3 MB body, before the node can possibly know which commitment
/// it is about to work on. An `arc_newBlock` landing in that window used to
/// see nothing anywhere and pay a 200-250 ms peer pull; it now waits (see
/// `LaneNode::await_stage_race`).
///
/// A guard rather than a pair of statements so that the handler's `?` exits
/// and a panic both lower the count — a leak here would make every later
/// anchor burn the grace window for nothing.
struct StagePending(Arc<LaneNode>);

impl StagePending {
    fn new(node: &Arc<LaneNode>) -> Self {
        node.stage_pending.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(node.clone())
    }
}

impl Drop for StagePending {
    fn drop(&mut self) {
        self.0.stage_pending.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        // a waiter polling the grace window should re-check `inflight` now
        self.0.stage_notify.notify_waiters();
    }
}

pub fn module(node: Arc<LaneNode>) -> RpcModule<Arc<LaneNode>> {
    let mut m = RpcModule::new(node);

    m.register_async_method("eth_sendRawTransaction", |params, node, _| async move {
        let raw: String = params.one()?;
        let bytes = hex::decode(raw.trim_start_matches("0x")).map_err(|e| err(e.to_string()))?;
        let hash = node.submit_raw(&bytes).await.map_err(err)?;
        Ok::<_, ErrorObjectOwned>(format!("{hash}"))
    })
    .unwrap();

    m.register_async_method("arc_sendRawTxBatch", |params, node, _| async move {
        let b64: String = params.one()?;
        let blob =
            base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| err(e.to_string()))?;
        let mut at = 0usize;
        let mut accepted = 0u64;
        let mut rejected = 0u64;
        while blob.len() >= at + 4 {
            let len = u32::from_le_bytes(blob[at..at + 4].try_into().unwrap()) as usize;
            at += 4;
            if blob.len() < at + len {
                return Err(err("torn frame in batch"));
            }
            match node.submit_raw(&blob[at..at + len]).await {
                Ok(_) => accepted += 1,
                Err(_) => rejected += 1,
            }
            at += len;
        }
        if at != blob.len() {
            return Err(err("trailing bytes in batch"));
        }
        Ok::<_, ErrorObjectOwned>(json!({ "accepted": accepted, "rejected": rejected }))
    })
    .unwrap();

    // ---- CL shim (contract: docs/lean-lane-integration.md in the arc repo) ----

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct BuildParams {
        parent_commitment: B256,
        number: u64,
        timestamp_ms: u64,
        budget_gas: u64,
    }
    m.register_async_method("arc_buildBlock", |params, node, _| async move {
        let p: BuildParams = params.parse()?;
        let (commitment, wire) = node
            .shim_build(p.parent_commitment, p.number, p.timestamp_ms, p.budget_gas)
            .await
            .map_err(err)?;
        Ok::<_, ErrorObjectOwned>(json!({
            "commitment": format!("{commitment}"),
            "blockBytes": base64::engine::general_purpose::STANDARD.encode(wire),
        }))
    })
    .unwrap();

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct NewBlockParams {
        block_bytes: Option<String>,
        commitment: Option<B256>,
    }
    m.register_async_method("arc_newBlock", |params, node, _| async move {
        // FIRST statement in the handler. `pre_ms` in the `lean_anchor` line
        // is everything from here to path dispatch: param parse, the base64
        // decode of a ~3.4 MB body, and whatever the tokio executor made this
        // task wait for before it ran.
        //
        // NOT `rpc_queue_ms`: jsonrpsee hands the handler no request-receive
        // timestamp (the third closure argument is `Extensions`, and the
        // server inserts nothing time-related), so socket-read -> handler-entry
        // is not observable from inside a method. This instant is the earliest
        // point the node can name.
        let t_entry = std::time::Instant::now();
        let p: NewBlockParams = params.parse()?;
        let outcome = match (p.block_bytes, p.commitment) {
            (Some(b64), _) => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&b64)
                    .map_err(|e| err(e.to_string()))?;
                node.shim_new_at(&bytes, t_entry).await.map_err(err)?
            }
            (None, Some(c)) => {
                node.shim_new_by_commitment_at(c, t_entry).await.map_err(err)?
            }
            (None, None) => return Err(err("arc_newBlock: need blockBytes or commitment")),
        };
        match outcome {
            crate::node::NewBlockOutcome::Valid(commitment) => Ok::<_, ErrorObjectOwned>(
                json!({ "status": "VALID", "commitment": format!("{commitment}") }),
            ),
            crate::node::NewBlockOutcome::Syncing { head } => {
                Ok(json!({ "status": "SYNCING", "number": head }))
            }
        }
    })
    .unwrap();

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct GetBlockParams {
        number: Option<u64>,
        commitment: Option<B256>,
    }
    m.register_async_method("arc_stageBlock", |params, node, _| async move {
        // FIRST statement in the handler — see `StagePending`. Everything
        // below it (param parse, base64 decode of ~3 MB) is the window in
        // which the node knows a stage is coming but not yet which block.
        let _pending = StagePending::new(&node);
        let p: NewBlockParams = params.parse()?;
        let block_bytes =
            p.block_bytes.ok_or_else(|| err("arc_stageBlock: need blockBytes"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&block_bytes)
            .map_err(|e| err(e.to_string()))?;
        match node.stage_block(&bytes).await.map_err(err)? {
            crate::node::StageOutcome::Staged(commitment) => Ok::<_, ErrorObjectOwned>(
                json!({ "status": "STAGED", "commitment": format!("{commitment}") }),
            ),
            crate::node::StageOutcome::Syncing { head } => {
                Ok(json!({ "status": "SYNCING", "number": head }))
            }
        }
    })
    .unwrap();

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct AnnounceParams {
        commitment: B256,
        number: u64,
    }
    m.register_async_method("arc_announceBlock", |params, node, _| async move {
        let p: AnnounceParams = params.parse()?;
        node.on_announce(p.commitment, p.number).await;
        Ok::<_, ErrorObjectOwned>(json!({ "status": "ACK" }))
    })
    .unwrap();

    m.register_async_method("arc_getBlockBytes", |params, node, _| async move {
        let p: GetBlockParams = params.parse()?;
        let wire = match (p.commitment, p.number) {
            (Some(c), _) => {
                node.block_wire_bytes_by_commitment_for(c, "get_bytes").await.map_err(err)?
            }
            (None, Some(n)) => node.block_wire_bytes(n).await.map_err(err)?,
            (None, None) => return Err(err("arc_getBlockBytes: need number or commitment")),
        };
        Ok::<_, ErrorObjectOwned>(json!({
            "blockBytes": wire.map(|w| base64::engine::general_purpose::STANDARD.encode(w)),
        }))
    })
    .unwrap();

    m.register_async_method("txpool_status", |_params, node, _| async move {
        let s = node.pool.pool_size();
        Ok::<_, ErrorObjectOwned>(json!({ "pending": s.pending, "queued": s.queued }))
    })
    .unwrap();

    // Committed-state nonce (block tag ignored — the lane has instant finality).
    // Spammers/wallets use this for nonce resync; the pool tracks pending on top.
    m.register_async_method("eth_getTransactionCount", |params, node, _| async move {
        let mut seq = params.sequence();
        let addr: alloy_primitives::Address =
            seq.next().map_err(|e| err(e.to_string()))?;
        let _tag: Option<String> = seq.optional_next().unwrap_or(None);
        let nonce = node.state.lock().await.get(&addr).nonce;
        Ok::<_, ErrorObjectOwned>(format!("0x{nonce:x}"))
    })
    .unwrap();

    m.register_async_method("arc_getHead", |_params, node, _| async move {
        let (commitment, number, timestamp_ms) = *node.head.lock().await;
        Ok::<_, ErrorObjectOwned>(
            json!({ "number": number, "commitment": format!("{commitment}"), "timestampMs": timestamp_ms }),
        )
    })
    .unwrap();

    m.register_async_method("eth_getTransactionReceipt", |params, node, _| async move {
        let hash: String = params.one()?;
        let hash: B256 =
            hash.trim_start_matches("0x").parse().map_err(|_| err("bad hash"))?;
        let r = node.receipt(hash).await;
        Ok::<_, ErrorObjectOwned>(r.map(|r| {
            json!({
                "transactionHash": format!("{}", r.tx_hash),
                "blockNumber": r.block_number,
                "transactionIndex": r.index,
                "from": format!("{}", r.sender),
                "status": if r.applied { "0x1" } else { "0x0" },
                "logs": r.logs.iter().map(|l| json!({
                    "from": format!("{}", l.from),
                    "to": format!("{}", l.to),
                    "amountWei": l.amount.to_string(),
                })).collect::<Vec<_>>(),
            })
        }))
    })
    .unwrap();

    m
}

pub async fn serve(node: Arc<LaneNode>, addr: SocketAddr) -> eyre::Result<(SocketAddr, ServerHandle)> {
    let config = ServerConfigBuilder::new().max_request_body_size(64 * 1024 * 1024).build();
    // A restart must not die on a transient EADDRINUSE (a predecessor's
    // lingering socket, a slow port release): retry for a while rather than
    // throw away a completed recovery on one syscall. Bounded, so a genuinely
    // occupied port (another node on it — the 2026-09-07 test-script bug
    // that first surfaced this path) still fails loudly instead of hanging.
    const BIND_ATTEMPTS: u32 = 240; // 240 x 500 ms = 120 s
    let mut last = None;
    for attempt in 1..=BIND_ATTEMPTS {
        match Server::builder().set_config(config.clone()).build(addr).await {
            Ok(server) => {
                let bound = server.local_addr()?;
                if attempt > 1 {
                    eprintln!("rpc: bound {bound} after {attempt} attempts");
                }
                let handle = server.start(module(node));
                return Ok((bound, handle));
            }
            Err(e) => {
                if attempt % 20 == 1 {
                    eprintln!("rpc: bind {addr} failed ({e}); retrying (attempt {attempt}/{BIND_ATTEMPTS})");
                }
                last = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
    Err(eyre::eyre!("rpc: could not bind {addr} after {BIND_ATTEMPTS} attempts: {:#}", last.expect("no bind without an error")))
}

/// Client-side helper: frame + base64 a batch of canonical txs.
pub fn encode_batch(txs: &[Vec<u8>]) -> String {
    let mut blob = Vec::with_capacity(txs.iter().map(|t| 4 + t.len()).sum());
    for t in txs {
        blob.extend_from_slice(&(t.len() as u32).to_le_bytes());
        blob.extend_from_slice(t);
    }
    base64::engine::general_purpose::STANDARD.encode(blob)
}
