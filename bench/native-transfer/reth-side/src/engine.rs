//! The engine path: the same blocks fed to reth's own engine tree
//! (`EngineApiTreeHandler` + `BasicEngineValidator`, default `TreeConfig`),
//! in-process, as `reth_newPayload` + `forkchoiceUpdated` messages. No
//! JSON-RPC, no network, no CL. This is how a running reth node imports a
//! block: payload decode, parallel sender recovery streamed into execution,
//! prewarming, the cross-block execution cache, the sparse-trie state-root
//! task running beside execution, receipts root, and the consensus checks
//! (a block is only VALID if its state root, receipts root and gas match).
//!
//! Persistence is not timed here: the tree keeps blocks in memory and
//! persists them in background batches (default threshold 50 blocks), so no
//! block of a short run is persisted. The per-block `save_blocks + commit`
//! cost is measured by the direct path (phase 4).

use alloy_primitives::B256;
use alloy_rpc_types_engine::{ExecutionData, ForkchoiceState};
use reth_chainspec::ChainSpec;
use reth_consensus::FullConsensus;
use reth_engine_primitives::{BeaconEngineMessage, NoopInvalidBlockHook, PayloadValidator};
use reth_engine_tree::{
    engine::{EngineApiKind, EngineApiRequest, FromEngine},
    persistence::PersistenceHandle,
    tree::{BasicEngineValidator, EngineApiTreeHandler, TreeConfig},
};
use reth_ethereum_consensus::EthBeaconConsensus;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_ethereum_payload_builder::EthereumExecutionPayloadValidator;
use reth_ethereum_primitives::{Block, EthPrimitives};
use reth_evm_ethereum::EthEvmConfig;
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::NewPayloadError;
use reth_primitives_traits::SealedBlock;
use reth_provider::{
    providers::BlockchainProvider, test_utils::MockNodeTypesWithDB, ProviderFactory,
};
use reth_prune::PrunerBuilder;
use reth_storage_overlay::OverlayManager;
use reth_tasks::{RayonConfig, RuntimeBuilder, RuntimeConfig};
use serde::Serialize;
use std::sync::Arc;
use std::time::Instant;

/// reth's Ethereum payload validator (`EthereumEngineValidator` in
/// reth-node-ethereum is this same delegation; that crate is not pulled in to
/// keep the build small).
#[derive(Debug, Clone)]
pub struct EthPayloadValidator(pub EthereumExecutionPayloadValidator<ChainSpec>);

impl PayloadValidator<EthEngineTypes> for EthPayloadValidator {
    type Block = Block;

    fn convert_payload_to_block(
        &self,
        payload: ExecutionData,
    ) -> Result<SealedBlock<Block>, NewPayloadError> {
        self.0
            .ensure_well_formed_payload(payload)
            .map_err(Into::into)
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct EngineBlockMs {
    pub block: u64,
    /// server-side latency reported by the tree (`NewPayloadTimings::latency`)
    pub new_payload: f64,
    /// send -> response, as seen by the caller
    pub new_payload_wall: f64,
    pub forkchoice_updated: f64,
}

pub type Factory = ProviderFactory<MockNodeTypesWithDB>;

/// Feeds `payloads` (with block hashes `hashes`) through the engine tree on
/// top of `factory` (genesis already written). Returns per-block timings and
/// the provider (canonical in-memory state over the database) for checks.
pub fn run(
    factory: Factory,
    chain_spec: Arc<ChainSpec>,
    payloads: Vec<ExecutionData>,
    hashes: &[B256],
    threads: usize,
    warmup: usize,
) -> eyre::Result<(Vec<EngineBlockMs>, BlockchainProvider<MockNodeTypesWithDB>)> {
    let rayon = RayonConfig {
        cpu_threads: Some(threads),
        ..Default::default()
    };
    let runtime = RuntimeBuilder::new(RuntimeConfig::default().with_rayon(rayon)).build()?;
    let provider = BlockchainProvider::new(factory.clone())?;
    let consensus: Arc<dyn FullConsensus<EthPrimitives>> =
        Arc::new(EthBeaconConsensus::new(chain_spec.clone()));
    let evm_config = EthEvmConfig::new(chain_spec.clone());
    let overlay_manager = OverlayManager::new(runtime.state_trie_overlay_worker_pool());
    let tree_config = TreeConfig::default();
    let validator = BasicEngineValidator::new(
        provider.clone(),
        consensus.clone(),
        evm_config.clone(),
        EthPayloadValidator(EthereumExecutionPayloadValidator::new(chain_spec)),
        tree_config.clone(),
        Box::new(NoopInvalidBlockHook::default()),
        overlay_manager.clone(),
        runtime.clone(),
    );
    let pruner = PrunerBuilder::default().build_with_provider_factory(factory.clone());
    let (metrics_tx, metrics_rx) = tokio::sync::mpsc::unbounded_channel();
    let persistence =
        PersistenceHandle::<EthPrimitives>::spawn_service(factory, pruner, metrics_tx);
    let (payload_tx, payload_rx) = tokio::sync::mpsc::unbounded_channel();
    let payload_builder = PayloadBuilderHandle::<EthEngineTypes>::new(payload_tx);
    let canonical = provider.canonical_in_memory_state();
    let (to_tree, mut from_tree) = EngineApiTreeHandler::spawn_new(
        provider.clone(),
        consensus,
        validator,
        persistence,
        payload_builder,
        canonical,
        overlay_manager,
        tree_config,
        EngineApiKind::Ethereum,
        evm_config,
        runtime,
    );
    // drain the tree's outgoing events
    std::thread::spawn(move || while from_tree.blocking_recv().is_some() {});

    let mut out = Vec::with_capacity(payloads.len());
    for (i, payload) in payloads.into_iter().enumerate() {
        let number = i as u64 + 1;
        let mut m = EngineBlockMs {
            block: number,
            ..Default::default()
        };

        let (tx, rx) = tokio::sync::oneshot::channel();
        let t = Instant::now();
        to_tree
            .send(FromEngine::Request(EngineApiRequest::Beacon(
                BeaconEngineMessage::RethNewPayload {
                    payload,
                    // reth-bench's unbiased mode: start once the previous block's
                    // background work (cache save, trie preservation) is done, as
                    // it is on a live chain between slots
                    wait_for_persistence: true,
                    wait_for_caches: true,
                    tx,
                    enqueued_at: Instant::now(),
                },
            )))
            .map_err(|_| eyre::eyre!("engine tree is gone"))?;
        let (status, timings) = rx
            .blocking_recv()?
            .map_err(|e| eyre::eyre!("newPayload error: {e}"))?;
        m.new_payload_wall = t.elapsed().as_secs_f64() * 1e3;
        m.new_payload = timings.latency.as_secs_f64() * 1e3;
        eyre::ensure!(
            status.is_valid(),
            "engine rejected block {number}: {:?} (the direct path's header disagrees with the engine)",
            status
        );

        let (tx, rx) = tokio::sync::oneshot::channel();
        let head = hashes[i];
        let t = Instant::now();
        to_tree
            .send(FromEngine::Request(EngineApiRequest::Beacon(
                BeaconEngineMessage::ForkchoiceUpdated {
                    state: ForkchoiceState {
                        head_block_hash: head,
                        safe_block_hash: head,
                        finalized_block_hash: B256::ZERO,
                    },
                    payload_attrs: None,
                    tx,
                },
            )))
            .map_err(|_| eyre::eyre!("engine tree is gone"))?;
        let fcu = rx
            .blocking_recv()?
            .map_err(|e| eyre::eyre!("forkchoiceUpdated error: {e}"))?;
        m.forkchoice_updated = t.elapsed().as_secs_f64() * 1e3;
        eyre::ensure!(
            fcu.forkchoice_status().is_valid(),
            "forkchoiceUpdated not valid"
        );

        eprintln!(
            "  reth engine block {:>2}{}: newPayload {:>8.2} ms (wall {:.2}) + forkchoiceUpdated {:.2}",
            number,
            if i < warmup { " (warm-up)" } else { "" },
            m.new_payload,
            m.new_payload_wall,
            m.forkchoice_updated
        );
        out.push(m);
    }
    drop((to_tree, metrics_rx, payload_rx));
    Ok((out, provider))
}
