//! The lane node: reth's pool as a LIBRARY (validator + ordering + eviction,
//! unmodified) + flat-state total STF + append-only log, driven by the
//! consensus layer through the shim verbs (`arc_buildBlock`, `arc_stageBlock`,
//! `arc_newBlock`, `arc_getHead`, `arc_getBlockBytes`; contract in
//! `docs/integration.md`) plus node-to-node gossip and backfill.

use crate::chain::{genesis_commitment, BlockLog, LeanBlock};
use crate::exec::{apply_block, execute, Delta};
use crate::metrics::{rss_mb, AnchorPath, Metrics, PathCell};
use crate::provider::LeanProvider;
use crate::state::{Acct, FlatState};
use alloy_consensus::Transaction as _;
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, B256, U256};
use lean_native::envelope::{set_lane_chain_id, ArcTxEnvelope, LEAN_TX_TYPE};
use lean_native::pool::ArcPooledTx;
use lean_native::LeanTx;
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::SignerRecoverable;
use reth_transaction_pool::blobstore::InMemoryBlobStore;
use reth_transaction_pool::validate::{EthTransactionValidator, EthTransactionValidatorBuilder};
use reth_transaction_pool::{
    CoinbaseTipOrdering, Pool, PoolConfig, PoolTransaction, SubPoolLimit, TransactionOrigin,
    TransactionPool, TransactionPoolExt,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use std::time::Instant;
use tokio::sync::Mutex;

mod build;
mod commit;
mod shim;
mod stage;
mod sync;

pub type LanePool = Pool<
    EthTransactionValidator<LeanProvider, ArcPooledTx, EthEvmConfig>,
    CoinbaseTipOrdering<ArcPooledTx>,
    InMemoryBlobStore,
>;

pub struct Config {
    pub datadir: PathBuf,
    pub chain_id: u64,
    /// Blocks between full state snapshots. **0 means NEVER snapshot** — the
    /// node then recovers by replaying the whole log from genesis, which is
    /// what a short-lived test or a throwaway datadir wants. Any other value
    /// is a modulus (see `snapshot_due`).
    pub snapshot_every: u64,
    pub beneficiary: Address,
    /// Other lean nodes' RPC urls (backfill source + push-on-append targets).
    pub peers: Vec<String>,
    /// fsync the block log after every append (`--no-fsync` turns it off).
    /// Off, a crash can lose the log tail: a liveness cost only — peers
    /// backfill it and the CL certificate binds the bytes (see
    /// `chain::BlockLog::append` and the snapshot-ahead repair in `open`).
    pub fsync: bool,
    /// Log a `lean_stats` line (counters and sizes) at info every 10 s.
    pub stats: bool,
    /// How long an announcement for head+1 that we do NOT hold waits for our
    /// own CL to anchor it before arming the backfill. It only matters on a
    /// node whose CL is parked or dead; on a healthy validator the anchor
    /// wins and the grace saves a full-block pull per announcing peer. 0
    /// pulls immediately.
    pub announce_grace_ms: u64,
    /// How long an `arc_newBlock{commitment}` that holds none of the block
    /// waits for an `arc_stageBlock` that has not ARRIVED yet (see
    /// `await_anchor_grace`). 0 disables it.
    pub anchor_grace_ms: u64,
}

impl Config {
    /// Is block `number` a snapshot point? `snapshot_every == 0` disables
    /// snapshots entirely rather than panicking on `% 0`, which is what both
    /// append paths used to do the first time they committed a block.
    pub fn snapshot_due(&self, number: u64) -> bool {
        self.snapshot_every != 0 && number.is_multiple_of(self.snapshot_every)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            datadir: PathBuf::from("lean-lane-data"),
            chain_id: 1338,
            snapshot_every: 1024,
            beneficiary: Address::with_last_byte(0xbe),
            peers: Vec::new(),
            fsync: true,
            stats: false,
            announce_grace_ms: 250,
            anchor_grace_ms: 60,
        }
    }
}

/// LOCK ORDER (the whole of it — audited 2026-09-15).
///
/// Almost every lock in this node is taken ALONE: acquired, used, dropped
/// inside one statement or one short block, never while holding another. Two
/// deliberate exceptions, and they agree:
///
///   1. `commit_gate` -> anything. `commit_block` and `promote_staged` hold
///      the gate across the whole append (the linkage decision, the state
///      write, the log append, the head write and the bookkeeping sweeps) and
///      take `head`, `state`, `log`, `by_commitment`, `staged` and
///      `inflight` under it — one at a time.
///   2. `head` -> `state`, nested in the two staging paths (`stage_claimed`
///      and `shim_build`) so that the head a staged entry names as its parent
///      and the state it executes on are read ATOMICALLY. That is the same
///      order (1) uses sequentially — head first, via `link_to_head`, then
///      state — so the two cannot deadlock against each other.
///
/// Nothing anywhere takes `state` before `head`. The serving path
/// (`locate_by_commitment`, `get_block_bytes`, `stats_line`) and admission
/// take single locks only, and no lock is held across an await other than a
/// lock acquisition — the `head` guard in (2) spans exactly the `state`
/// acquire.
pub struct LaneNode {
    pub cfg: Config,
    pub pool: LanePool,
    pub provider: LeanProvider,
    pub state: Mutex<FlatState>,
    pub log: Mutex<BlockLog>,
    /// (head commitment, head number, head timestamp_ms)
    pub head: Mutex<(B256, u64, u64)>,
    /// THE APPEND GATE. Held for the whole of a commit — from the linkage
    /// decision, through the state apply and the log append, to the head
    /// write — by `commit_block` and `promote_staged`, the only two functions
    /// that append. It is what makes the head read that decides "does this
    /// block connect?" and the head write that answers it for the next
    /// committer ONE critical section.
    ///
    /// Claim discipline (`inflight`) narrows the double-append window but has
    /// never closed it, because not every appender holds a claim: the
    /// sync-queue drain commits with none, the staged promote hands its claim
    /// back before promoting, and a claim waited out for 2 s can be stolen
    /// from a holder that is merely slow rather than dead. Each of those ends
    /// in two committers that both passed a parent-link check nobody re-took
    /// at the moment of the append. The gate makes that check last-moment and
    /// universal (`link_to_head`); the claim stays, as the thing that spares
    /// the loser ~230 ms of execution it does not need.
    ///
    /// Costs nothing in steady state: commits are serial anyway (each builds
    /// on the head the previous one wrote), so the gate is uncontended except
    /// in exactly the racing case it exists to arbitrate. Staging, admission
    /// and the serving path never take it.
    commit_gate: Mutex<()>,
    pub metrics: Metrics,
    /// Blocks that arrived ahead of our head, keyed by PARENT commitment
    /// (bounded; drained as soon as their parent lands).
    pub sync_queue: Mutex<std::collections::HashMap<B256, crate::chain::LeanBlock>>,
    /// Wakes the backfill task when a SYNCING entry is queued.
    pub sync_notify: tokio::sync::Notify,
    /// Highest announced block number (backfill target when nothing is queued).
    pub sync_target: AtomicU64,
    /// Vote-gap staged blocks keyed by commitment (bounded to 32). Entries
    /// behind the head are dropped whenever the head advances; entries at or
    /// ahead of the new head are kept — a losing candidate at the settled
    /// height stays servable by commitment to a peer that still asks for it.
    pub staged: Mutex<std::collections::VecDeque<(B256, StagedBlock)>>,
    /// commitment -> block number for EVERY canonical block: filled from the
    /// whole log at open, extended on every append. Complete from height 1,
    /// so a miss is proof the block is not canonical here.
    pub by_commitment: Mutex<std::collections::HashMap<B256, u64>>,
    /// IN-FLIGHT work on a commitment: commitment -> `Claim`.
    /// arc_newBlock awaits a matching in-flight staging (bounded guard)
    /// instead of redoing the full execution — a <=35ms wait beats a ~230ms
    /// redo (v1.3 lost ~half the anchor races at 2.6MB blocks) — and the
    /// COMMIT of a block takes the same claim, which is what stops two feeds
    /// of the same bytes from appending it twice.
    ///
    /// The `id` is what makes that exclusion sound. Keyed on the commitment
    /// alone, a release is "whoever finishes first deletes the entry": a
    /// stage that stalled past the 2 s guard and was taken over would, on
    /// finishing, delete the CLAIM OF THE COMMITTER THAT TOOK IT OVER, and a
    /// third feed arriving in that window would pass the parent-link check
    /// (the head has not moved yet) and append the same block a second time.
    /// Both the release and the takeover therefore compare ids: you may only
    /// drop your own claim, and you may only steal a claim you have already
    /// waited out (see `release_claim` and the claim rounds in
    /// `shim_new_inner`).
    pub inflight: Mutex<std::collections::HashMap<B256, Claim>>,
    /// `arc_stageBlock` calls that have ARRIVED but not yet returned. Raised
    /// by the RPC handler as its first statement — before the JSON parse and
    /// the base64 decode of a ~3 MB body — so it covers the window in which
    /// the node knows a stage is coming but cannot yet know WHICH commitment
    /// (the commitment is a function of the bytes it is still decoding).
    ///
    /// Without it an anchor that arrives during that window sees an empty
    /// `inflight`, concludes the block is nowhere, and pays a 200-250 ms peer
    /// pull on the consensus critical path for a block that lands in memory
    /// milliseconds later. With it the anchor waits (bounded) for the stage to
    /// name itself. Zero when no stage is in flight, so the backfill's genuine
    /// peer pulls are not slowed at all.
    pub stage_pending: AtomicU64,
    /// Fires when a stage registers in `inflight` or finishes; the anchor's
    /// grace loop waits on it rather than spinning.
    pub stage_notify: tokio::sync::Notify,
    /// Peer RPC clients (index-aligned with cfg.peers).
    peer_clients: Vec<jsonrpsee::http_client::HttpClient>,
}

/// Outcome of `arc_newBlock`: applied/known (VALID, with the commitment) or
/// queued behind the tip (SYNCING, with our current head number).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewBlockOutcome {
    Valid(B256),
    Syncing { head: u64 },
}

/// Outcome of `arc_stageBlock` (speculative — never queues, never appends).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageOutcome {
    Staged(B256),
    Syncing { head: u64 },
}

/// A fully validated + executed block awaiting its decide anchor. Memory-only:
/// the log append at promotion is the durability point, so a crash between
/// stage and promote loses nothing.
pub struct StagedBlock {
    /// head commitment this was staged AGAINST — promotion requires it to
    /// still be the head (head equality ⟺ state equality: state is a pure
    /// function of the chain).
    staged_parent: B256,
    block: crate::chain::LeanBlock,
    wire: Vec<u8>,
    /// what executing `block` on `staged_parent`'s state writes — appended
    /// verbatim on promote, by the same `append` as a direct commit
    delta: Delta,
}

/// What a log replay reports: (head, blocks replayed, txs replayed, the
/// commitment -> number index of EVERY record in the log).
type Replayed = (
    (B256, u64, u64),
    u64,
    u64,
    std::collections::HashMap<B256, u64>,
);

/// Replay `log` onto `state`, starting from the snapshot `snap` = (commitment,
/// number, timestamp_ms) that `state` was loaded from. Each replayed tx pays
/// one decode + one ecrecover — the honest recovery cost. Returns the new head
/// plus what it took to get there: (head, blocks, txs, commitment index).
///
/// The commitment index covers every intact record, below the snapshot too,
/// so it is complete from height 1: a miss is proof of absence.
fn replay_onto(
    state: &mut FlatState,
    log: &mut BlockLog,
    snap: (B256, u64, u64),
    beneficiary: Address,
) -> std::io::Result<Replayed> {
    let (mut head, mut blocks, mut txs) = (snap, 0u64, 0u64);
    let mut by_commitment = std::collections::HashMap::new();
    log.replay(|block| {
        by_commitment.insert(block.commitment, block.number);
        if block.number <= snap.1 {
            return;
        }
        let items = decode_block_txs(&block.txs);
        txs += items.len() as u64;
        apply_block(state, &items, beneficiary);
        head = (block.commitment, block.number, block.timestamp_ms);
        blocks += 1;
    })?;
    Ok((head, blocks, txs, by_commitment))
}

/// One holder's exclusive claim on a commitment: (staged_parent, claim id,
/// completion signal). See `LaneNode::inflight`.
pub type Claim = (B256, u64, tokio::sync::watch::Receiver<bool>);

/// Process-unique claim id. Only equality matters (ownership), so a plain
/// relaxed counter is enough — it never wraps in any realistic lifetime.
fn next_claim_id() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

impl LaneNode {
    /// Open (or create) the lane at `cfg.datadir`: newest snapshot + log-tail
    /// replay (each replayed tx pays one decode + one ecrecover — the honest
    /// recovery cost). Returns the node with the pool's validator seeded from
    /// the recovered state.
    pub fn open(cfg: Config) -> eyre::Result<(Self, RecoveryReport)> {
        set_lane_chain_id(cfg.chain_id);
        std::fs::create_dir_all(&cfg.datadir)?;
        let t0 = Instant::now();
        let (mut state, snap_commitment, mut snap_number, snap_ts) =
            match FlatState::load_newest_snapshot(&cfg.datadir)? {
                Some((s, c, n, t)) => (s, c, n, t),
                None => (FlatState::default(), genesis_commitment(cfg.chain_id), 0, 0),
            };
        let mut log = BlockLog::open(&cfg.datadir.join("blocks.log"), cfg.fsync)?;
        let beneficiary = cfg.beneficiary;
        let snap = (snap_commitment, snap_number, snap_ts);
        let (mut head, mut replayed_blocks, mut replayed_txs, mut by_commitment) =
            replay_onto(&mut state, &mut log, snap, beneficiary)?;

        // SNAPSHOT AHEAD OF THE LOG (only reachable with `fsync` off, which
        // fsyncs the snapshot but not the append — see `BlockLog::append`).
        // Resuming from a snapshot the log cannot reach leaves a permanent hole
        // below the head: the backfill only ever pulls `head + 1` upward, so
        // those heights stay unservable and a re-feed of one of them is a hard
        // error. Restart from the newest snapshot the log CAN continue from
        // (they are never pruned, so normally the previous one) and replay from
        // there; state and log then end at the same block and the backfill
        // repairs the difference as ordinary lag.
        let log_head = log.last_number().unwrap_or(0);
        if snap_number > log_head {
            let fallback = FlatState::load_newest_snapshot_upto(&cfg.datadir, log_head)?;
            let (from, at) = match fallback {
                Some((s, c, n, t)) => (s, (c, n, t)),
                None => (
                    FlatState::default(),
                    (genesis_commitment(cfg.chain_id), 0, 0),
                ),
            };
            tracing::warn!(
                "lean_recovery WARNING snapshot {snap_number} is AHEAD of the block log \
                 (last intact record {log_head}): heights {}..={snap_number} were lost with \
                 the unsynced log tail. Restarting from snapshot {} and replaying; the \
                 backfill re-pulls the rest from peers.",
                log_head + 1,
                at.1,
            );
            state = from;
            snap_number = at.1;
            (head, replayed_blocks, replayed_txs, by_commitment) =
                replay_onto(&mut state, &mut log, at, beneficiary)?;
        }
        let recovery = RecoveryReport {
            snapshot_number: snap_number,
            replayed_blocks,
            replayed_txs,
            elapsed: t0.elapsed(),
        };

        let provider = LeanProvider::new();
        provider.load_from(&state);
        provider.set_head(head.1);
        let validator =
            EthTransactionValidatorBuilder::new(provider.clone(), EthEvmConfig::mainnet())
                .with_custom_tx_type(LEAN_TX_TYPE)
                .build(InMemoryBlobStore::default());
        // Lane pool caps: the fleet launchers' deep-pool trio (counts 400k /
        // 512MB / big account slots) — default reth caps (~10k) reject bulk
        // ingress (measured: first bench run bounced 1,772 of 30k).
        let big = SubPoolLimit {
            max_txs: 400_000,
            max_size: 512 * 1024 * 1024,
        };
        let pool_cfg = PoolConfig {
            pending_limit: big,
            basefee_limit: big,
            queued_limit: big,
            // 256 was too small for payment-lane senders: with per-node pools
            // governed to ~30k txs over ~200 senders, drain is BURSTY (a node's
            // pool empties only on its own proposer turns), so per-sender depth
            // variance crossed 256 -> reth flags the sender as a spammer
            // (ExceededSenderTransactionsCapacity) and DROPS the tx; the nonce
            // gap then poisons the sender's whole remaining stream. Measured
            // 2026-08-26: 62% of a 2.4M-tx feed bounced, blocks 35-42% full.
            max_account_slots: 4096,
            ..Default::default()
        };
        let pool = Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            pool_cfg,
        );
        let peer_clients = cfg
            .peers
            .iter()
            .filter_map(|u| {
                jsonrpsee::http_client::HttpClientBuilder::default()
                    .max_request_size(64 * 1024 * 1024)
                    .request_timeout(std::time::Duration::from_secs(4))
                    .build(u)
                    .ok()
            })
            .collect();
        Ok((
            Self {
                cfg,
                pool,
                provider,
                state: Mutex::new(state),
                log: Mutex::new(log),
                head: Mutex::new(head),
                commit_gate: Mutex::new(()),
                metrics: Metrics::default(),
                sync_queue: Mutex::new(Default::default()),
                sync_notify: tokio::sync::Notify::new(),
                sync_target: AtomicU64::new(0),
                staged: Mutex::new(Default::default()),
                by_commitment: Mutex::new(by_commitment),
                inflight: Mutex::new(Default::default()),
                stage_pending: AtomicU64::new(0),
                stage_notify: tokio::sync::Notify::new(),
                peer_clients,
            },
            recovery,
        ))
    }

    /// Bench/genesis hook: seed an account into state + the validator's view.
    pub async fn seed_account(&self, a: Address, acct: Acct) {
        self.state.lock().await.accounts.insert(a, acct);
        self.provider.upsert(a, acct);
    }

    /// Admit one raw canonical tx (2718 bytes). 0x50-only by design: type-2
    /// is rejected here (documented choice — supporting it would need
    /// fee-market semantics the lane deliberately dropped).
    pub async fn submit_raw(&self, bytes: &[u8]) -> Result<B256, String> {
        if bytes.first() != Some(&LEAN_TX_TYPE) {
            Metrics::inc(&self.metrics.rejected);
            return Err("only lean (0x50) transactions are accepted on the lane".into());
        }
        let env =
            ArcTxEnvelope::decode_2718(&mut &bytes[..]).map_err(|e| format!("decode: {e}"))?;
        let signer = env
            .recover_signer()
            .map_err(|e| format!("signature: {e}"))?;
        let len = bytes.len();
        let pooled = ArcPooledTx::new(
            alloy_consensus::transaction::Recovered::new_unchecked(env, signer),
            len,
        );
        let hash = *pooled.hash();
        let res = self
            .pool
            .add_transaction(TransactionOrigin::External, pooled)
            .await
            .map(|_| hash)
            .map_err(|e| e.to_string());
        Metrics::inc(match &res {
            Ok(_) => &self.metrics.admitted,
            Err(_) => &self.metrics.rejected,
        });
        res
    }

    /// One-line periodic snapshot: what can grow without bound, plus the
    /// operator counters (`Metrics::line`). Every field is a counter or a
    /// `len()` — nothing here walks the state, the log or the pool, so it is
    /// safe to call on a hot node.
    pub async fn stats_line(&self) -> String {
        let head = self.head.lock().await.1;
        let accounts = self.state.lock().await.accounts.len();
        let staged = self.staged.lock().await.len();
        let sync_queue = self.sync_queue.lock().await.len();
        let by_commitment = self.by_commitment.lock().await.len();
        let pool = self.pool.pool_size();
        format!(
            "lean_stats head={head} accounts={accounts} staged={staged} sync_queue={sync_queue} \
             by_commitment={by_commitment} pending={} queued={} rss_mb={} {}",
            pool.pending,
            pool.queued,
            rss_mb(),
            self.metrics.line(),
        )
    }

    /// `--stats`: log `stats_line()` at info every 10 s.
    pub fn spawn_stats(self: &std::sync::Arc<Self>) {
        if !self.cfg.stats {
            return;
        }
        let node = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                tick.tick().await;
                tracing::info!("{}", node.stats_line().await);
            }
        });
    }
}

pub struct RecoveryReport {
    pub snapshot_number: u64,
    pub replayed_blocks: u64,
    pub replayed_txs: u64,
    pub elapsed: std::time::Duration,
}

/// Decode a block's raw tx list back into recovered items (replay path: one
/// decode + one ecrecover per tx). Invalid bytes are skipped — a torn/bad tx
/// in an appended block cannot occur (we wrote it), but total-STF posture is
/// kept everywhere.
///
/// Decode + ecrecover in parallel on the rayon pool, keeping block order
/// (indexed par_iter + collect preserves it; skipped entries collapse via
/// flatten). Per-signature work stays off the tokio runtime threads.
pub fn decode_block_txs(txs: &[Vec<u8>]) -> Vec<(Address, LeanTx, B256)> {
    use rayon::prelude::*;
    txs.par_iter()
        .map(|bytes| {
            let env = ArcTxEnvelope::decode_2718(&mut bytes.as_slice()).ok()?;
            let signer = env.recover_signer().ok()?;
            match env {
                ArcTxEnvelope::Lean(lean) => {
                    let hash = *lean.hash();
                    Some((signer, lean.tx().clone(), hash))
                }
                _ => None,
            }
        })
        .collect::<Vec<_>>()
        .into_iter()
        .flatten()
        .collect()
}
