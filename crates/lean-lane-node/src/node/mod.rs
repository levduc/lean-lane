//! The lane node: reth's pool as a LIBRARY (validator + ordering + eviction,
//! unmodified) + flat-state total STF + append-only log, driven by the
//! consensus layer through the shim verbs (`arc_buildBlock`, `arc_stageBlock`,
//! `arc_newBlock`, `arc_getHead`, `arc_getBlockBytes`; contract in
//! `docs/integration.md`) plus node-to-node gossip and backfill.

use crate::chain::{genesis_commitment, BlockLog, LeanBlock};
use crate::exec::{apply_block, execute, Delta};
use crate::metrics::{rss_mb, AnchorPath, Metrics, PathCell};
use crate::pool::{ArcPooledTx, LanePool};
use crate::provider::LeanProvider;
use crate::state::{Acct, FlatState};
use alloy_consensus::Transaction as _;
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, B256, U256};
use lean_native::envelope::{set_lane_chain_id, ArcTxEnvelope, LEAN_TX_TYPE};
use lean_native::LeanTx;
use reth_primitives_traits::SignerRecoverable;
use reth_transaction_pool::{
    PoolTransaction, TransactionOrigin, TransactionPool, TransactionPoolExt,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use std::time::Instant;
use tokio::sync::Mutex;

mod build;
mod commit;
mod peers;
mod shim;
mod stage;
mod sync;

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
    /// How long an `arc_newBlock{commitment}` that holds none of the block,
    /// and sees no stage of it running, waits for one to arrive before it
    /// pulls from a peer (see `await_arrival`). 0 disables the wait for a
    /// stage that has not arrived; a stage already running is always waited
    /// for.
    pub anchor_grace_ms: u64,
    /// How many peers one block pull (anchor fallback, backfill) asks
    /// CONCURRENTLY; the first valid answer wins (see `peers::pull_block`).
    pub peer_fanout: usize,
    /// How many peers hear an announce-on-append: `None` = all of them, else
    /// a fresh random subset of this size per block.
    pub announce_fanout: Option<usize>,
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
            peer_fanout: 3,
            announce_fanout: None,
        }
    }
}

/// LOCK ORDER. Every lock is taken alone (acquired, used, dropped) except:
///
///   1. `commit_gate` -> anything: `commit_block` and `promote_staged` hold the
///      gate across the whole append and take the other locks under it, one at
///      a time.
///   2. `head` -> `state`, nested in the two staging paths (`stage_inner`,
///      `shim_build`) so the parent a staged entry names and the state it
///      executes on are read atomically. (1) takes them in the same order.
///
/// `in_progress` is a std mutex held for a map update only, never across an
/// await.
pub struct LaneNode {
    pub cfg: Config,
    pub pool: LanePool,
    pub provider: LeanProvider,
    pub state: Mutex<FlatState>,
    pub log: Mutex<BlockLog>,
    /// (head commitment, head number, head timestamp_ms)
    pub head: Mutex<(B256, u64, u64)>,
    /// THE APPEND GATE. Held for the whole of a commit, from the linkage
    /// decision (`link_to_head`) through the state apply and the log append to
    /// the head write, by `commit_block` and `promote_staged`, the only two
    /// functions that append. Every appender goes through it, so each block is
    /// appended exactly once whatever races feed it: a second feed of the same
    /// bytes finds it `AlreadyCanonical` and appends nothing. Uncontended in
    /// steady state (commits are serial anyway); staging, admission and the
    /// serving path never take it.
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
    /// Commitments being worked on right now (a stage executing, or a
    /// by-bytes commit), with a count per commitment. Not an exclusion: two
    /// feeds of the same block may both execute and the gate keeps the append
    /// single. It is what the arrival wait (`await_arrival`) watches, so an
    /// anchor that lands while its block is being executed here waits for that
    /// result instead of pulling the block from a peer.
    in_progress: std::sync::Mutex<std::collections::HashMap<B256, u32>>,
    /// Fires whenever a commitment enters or leaves `in_progress` and whenever
    /// a staged entry is inserted; the arrival wait sleeps on it.
    pub(crate) stage_notify: tokio::sync::Notify,
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
        let pool = crate::pool::new_lane_pool(provider.clone());
        let peer_clients = Self::peer_clients(&cfg.peers);
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
                in_progress: Default::default(),
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
