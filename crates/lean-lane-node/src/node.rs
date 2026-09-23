//! The lane node: reth's pool as a LIBRARY (validator + ordering + eviction,
//! unmodified) + flat-state total STF + append-only log, driven by the
//! consensus layer through the shim verbs (`arc_buildBlock`, `arc_stageBlock`,
//! `arc_newBlock`, `arc_getHead`, `arc_getBlockBytes`; contract in
//! `docs/integration.md`) plus node-to-node gossip and backfill.

use crate::chain::{genesis_commitment, BlockLog, LeanBlock};
use crate::exec::apply_block;
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

/// How long an anchor that holds none of the block waits for an
/// `arc_stageBlock` which has ARRIVED but not yet registered its commitment
/// (see `await_stage_race`). Only waited while `stage_pending > 0`.
const STAGE_RACE_GRACE: std::time::Duration = std::time::Duration::from_millis(40);

/// What `link_to_head` decided about a block that is about to be appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    /// `number == head + 1` and `parent == head commitment`: append it.
    Connects,
    /// Already ours at its own number, with this commitment: append nothing.
    AlreadyCanonical,
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

/// Where one `arc_stageBlock` spent its time (the `lean_stage` debug event).
#[derive(Default)]
struct StageTiming {
    decode_ms: f64,
    recover_ms: f64,
    apply_ms: f64,
    /// already staged against this head: nothing was executed
    noop: bool,
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
    /// post-state of every touched account (delta to write on promote)
    post_touched: Vec<(Address, crate::state::Acct)>,
    changed: Vec<reth_execution_types::ChangedAccount>,
    txs: usize,
    evict: Vec<B256>,
    noops: usize,
    outputs: usize,
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

    /// The last-moment linkage decision, taken under `commit_gate` and
    /// therefore in the same critical section as the append and the head write
    /// that follow it. Every caller of `commit_block`/`promote_staged` has
    /// already checked the link — that check is what lets them execute at all
    /// — but between that check and the append the head can move, and until
    /// this existed nothing looked again.
    ///
    /// Three answers, and the middle one is the point: a block that is already
    /// ours at its own number is a NO-OP, not an append. That is what makes a
    /// second committer of the same bytes (a stolen claim, a drain racing an
    /// anchor) harmless instead of a second record in `blocks.log` — the log
    /// diverges byte-for-byte from every other validator's while the state
    /// still converges, so nothing else in the node would ever notice.
    async fn link_to_head(&self, block: &LeanBlock) -> eyre::Result<Link> {
        let (head_c, head_n, _) = *self.head.lock().await;
        if block.number == head_n + 1 && block.parent == head_c {
            return Ok(Link::Connects);
        }
        if block.number > head_n {
            eyre::bail!(
                "block {} does not connect: parent {}, head {head_c} at {head_n}",
                block.number,
                block.parent,
            );
        }
        // At or below the head. The only benign case is that this exact block
        // is already canonical — someone else committed it while we executed.
        let known = if block.number == head_n {
            Some(head_c)
        } else {
            let mut log = self.log.lock().await;
            log.read_block(block.number)?.map(|b| b.commitment)
        };
        match known {
            Some(c) if c == block.commitment => Ok(Link::AlreadyCanonical),
            Some(c) => eyre::bail!(
                "conflicting block at known height {}: have {c}, got {}",
                block.number,
                block.commitment,
            ),
            None => eyre::bail!("height {} predates the log", block.number),
        }
    }

    /// Execute + append + commit bookkeeping. Re-checks the parent link under
    /// `commit_gate` immediately before touching state, so it is the ONLY way
    /// the chain advances *and* the place the "does this connect?" question is
    /// finally answered — no caller's earlier check is trusted. Idempotent:
    /// re-committing a block that is already canonical appends nothing and is
    /// still `Ok`, which is what the callers report as VALID.
    async fn commit_block(
        &self,
        block: LeanBlock,
        items: Vec<(Address, LeanTx, B256)>,
        evict: Vec<B256>,
    ) -> eyre::Result<()> {
        let _gate = self.commit_gate.lock().await;
        if self.link_to_head(&block).await? == Link::AlreadyCanonical {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        let outcome = apply_block(&mut state, &items, self.cfg.beneficiary);
        let mut touched: Vec<Address> = items.iter().map(|(s, _, _)| *s).collect();
        for (_, tx, _) in &items {
            touched.extend(tx.outputs.iter().map(|o| o.to));
        }
        touched.push(self.cfg.beneficiary);
        touched.sort_unstable();
        touched.dedup();
        for a in &touched {
            self.provider.upsert(*a, state.get(a));
        }
        self.provider.set_head(block.number);
        let snapshot_due = self.cfg.snapshot_due(block.number);
        let (digest_state, commitment) = (snapshot_due.then(|| state.clone()), block.commitment);
        // pool re-base info: post-block (nonce, balance) of every touched
        // account — without this the pool never learns mined nonces are
        // consumed and a sender's remaining txs demote to QUEUED forever
        // (the fleet regression: pending 0 / queued 25-31k / empty blocks).
        let changed: Vec<reth_execution_types::ChangedAccount> = touched
            .iter()
            .map(|a| {
                let acct = state.get(a);
                reth_execution_types::ChangedAccount {
                    address: *a,
                    nonce: acct.nonce,
                    balance: U256::from(acct.balance),
                }
            })
            .collect();
        drop(state);

        self.log.lock().await.append(&block)?;
        self.by_commitment
            .lock()
            .await
            .insert(block.commitment, block.number);
        if let Some(s) = digest_state {
            s.write_snapshot(
                &self.cfg.datadir,
                commitment,
                block.number,
                block.timestamp_ms,
            )?;
        }

        *self.head.lock().await = (block.commitment, block.number, block.timestamp_ms);
        self.pool.remove_transactions(evict);
        // re-base sender nonces/balances -> promotes now-connectable queued
        // txs to pending, discards stale ones (reth's canonical-update analog)
        self.pool.update_accounts(changed);
        // head advanced: keep entries that are still AHEAD of the new head
        // (losing candidates for this height are dropped when the height is
        // passed; bytes for the current height stay servable by commitment)
        let new_head = *self.head.lock().await;
        let new_head_n = new_head.1;
        self.staged
            .lock()
            .await
            .retain(|(_, e)| e.block.number >= new_head_n);
        self.inflight
            .lock()
            .await
            .retain(|_, (p, _, _)| *p == new_head.0);
        let m = &self.metrics;
        Metrics::inc(&m.blocks);
        Metrics::add(&m.txs, items.len() as u64);
        Metrics::add(&m.noops, outcome.noops as u64);
        Metrics::add(&m.outputs, outcome.outputs as u64);
        Ok(())
    }

    /// Assemble the memory-only staged entry for `block` from a POST-execution
    /// state copy. Shared by `stage_block` (someone else's bytes, arriving in
    /// the vote gap) and `shim_build` (our own proposal), so the two produce
    /// an identical entry — the proposer's own block must take exactly the
    /// same promote path as every other validator's.
    fn staged_entry(
        &self,
        block: LeanBlock,
        wire: Vec<u8>,
        staged_parent: B256,
        items: &[(Address, LeanTx, B256)],
        staged_state: &FlatState,
        outcome: crate::exec::BlockOutcome,
    ) -> StagedBlock {
        let mut touched: Vec<Address> = items.iter().map(|(s, _, _)| *s).collect();
        for (_, tx, _) in items {
            touched.extend(tx.outputs.iter().map(|o| o.to));
        }
        touched.push(self.cfg.beneficiary);
        touched.sort_unstable();
        touched.dedup();
        // ONLY addresses the execution actually WROTE. `touched` is the
        // syntactic set — every sender, every output of every tx, the
        // beneficiary — and it is deliberately wider than the post-state:
        // `apply_block` writes senders (always, even a no-op's), the outputs
        // of APPLIED txs, and the beneficiary. The outputs of a NO-OP tx (a
        // bankrupt or wrong-nonce sender) are never written.
        //
        // `staged_state.get()` would return `Acct::default()` for those and
        // promote would INSERT it, creating a zero entry that the direct
        // `commit_block` path never creates. `FlatState::digest` covers the
        // account COUNT, so a node that staged+promoted and a node that
        // executed the same bytes directly would report different digests and
        // write different snapshots — the recovery/agreement checks split on
        // an all-zero account nobody ever touched.
        //
        // Filtering on `contains_key` after execution reproduces exactly the
        // direct path's write set: an address already in the map keeps the
        // value it has (re-inserting it is a no-op), one that is not was not
        // written. `changed` is derived from the same filtered list, so the
        // pool is told about exactly the accounts whose (nonce, balance) can
        // have moved.
        let post_touched: Vec<(Address, crate::state::Acct)> = touched
            .iter()
            .filter_map(|a| staged_state.accounts.get(a).map(|acct| (*a, *acct)))
            .collect();
        let changed = post_touched
            .iter()
            .map(|(a, acct)| reth_execution_types::ChangedAccount {
                address: *a,
                nonce: acct.nonce,
                balance: U256::from(acct.balance),
            })
            .collect();
        let evict = items.iter().map(|(_, _, h)| *h).collect();
        StagedBlock {
            staged_parent,
            wire,
            post_touched,
            changed,
            txs: items.len(),
            evict,
            noops: outcome.noops,
            outputs: outcome.outputs,
            block,
        }
    }

    /// Publish a staged entry: replace any entry for the same commitment, keep
    /// the map bounded at 32. The ONLY writer of `self.staged` outside
    /// promotion's retention sweep.
    async fn insert_staged(&self, commitment: B256, entry: StagedBlock) {
        let mut q = self.staged.lock().await;
        q.retain(|(c, _)| *c != commitment);
        if q.len() >= 32 {
            q.pop_front();
        }
        q.push_back((commitment, entry));
        drop(q);
        // wake an anchor sitting in `await_anchor_grace` for exactly this
        // commitment: a staged entry carries no claim, so the staged map is
        // the only evidence it will ever see.
        self.stage_notify.notify_waiters();
    }

    /// Is `commitment` already staged against `head_c`? Used to make a second
    /// staging of the same bytes (our own `arc_buildBlock` result, then the
    /// CL's `arc_stageBlock` for it) a cheap no-op instead of a re-execution.
    async fn already_staged(&self, commitment: B256, head_c: B256) -> bool {
        self.staged
            .lock()
            .await
            .iter()
            .any(|(c, e)| *c == commitment && e.staged_parent == head_c)
    }

    /// Shim: `arc_buildBlock` — build a candidate on the CURRENT head against a
    /// STAGED state copy. Never appends, never evicts, never mutates committed
    /// state. Contract: parent must be the head commitment; number must be
    /// head+1 (a consistent CL can send nothing else — checked loudly).
    ///
    /// SELF-STAGES its result (2026-09-15). The proposer used to execute its
    /// own block on a throwaway copy and drop it: at decide its own
    /// `arc_newBlock{commitment}` found nothing staged and nothing queued, so
    /// it pulled ITS OWN block back over HTTP from a peer — on the consensus
    /// critical path. Measured on the 4-validator fleet: `path=peer` on
    /// 25-35% of heights (exactly the proposer's turn share), mean 150-300 ms
    /// against 8-11 ms for `path=staged`. Staging the build result costs one
    /// entry in a 32-slot map and turns that into the staged path.
    pub async fn shim_build(
        &self,
        parent: B256,
        number: u64,
        timestamp_ms: u64,
        budget_gas: u64,
    ) -> Result<(B256, Vec<u8>), String> {
        let t0 = Instant::now();
        let (head_c, head_n, _) = *self.head.lock().await;
        if parent != head_c {
            return Err(format!("stale parent: build on {parent}, head is {head_c}"));
        }
        if number != head_n + 1 {
            return Err(format!(
                "bad number {number}: head is {head_n} (want head+1)"
            ));
        }
        // pick under the CALLER's budget (may differ from cfg)
        let mut gas = 0u64;
        let mut items = Vec::new();
        let mut raw = Vec::new();
        for tx in self.pool.best_transactions() {
            let g = tx.transaction.gas_limit();
            if gas + g > budget_gas {
                break;
            }
            gas += g;
            let pooled: &ArcPooledTx = &tx.transaction;
            if let ArcTxEnvelope::Lean(lean) = pooled.0.transaction.inner() {
                items.push((
                    pooled.0.transaction.signer(),
                    lean.tx().clone(),
                    *lean.hash(),
                ));
                raw.push(lean.raw().to_vec());
            }
        }
        let block = LeanBlock::new(parent, number, timestamp_ms, raw);
        let commitment = block.commitment;
        let wire = block.to_wire_bytes();
        // Staged execution against a full state copy (I1 simplicity; CoW is a
        // later optimization) — proves the txs apply; the commitment derives
        // from content alone and is returned either way.
        //
        // The head is re-read and the state cloned in ONE critical section
        // (same lock order and for the same reason as `stage_claimed`): `head`
        // was read at the top of this function, before the pool pick, so a
        // block landing in between would otherwise make `staged` a post-state
        // of a DIFFERENT parent than the `staged_parent` recorded with it. The
        // post-hoc head check this replaces could only see a move that had
        // already finished, never one racing the clone itself.
        let on_head = {
            let head = self.head.lock().await;
            if head.0 == head_c {
                Some(self.state.lock().await.clone())
            } else {
                None
            }
        };
        if let Some(mut staged) = on_head {
            let outcome = apply_block(&mut staged, &items, self.cfg.beneficiary);
            if !self.already_staged(commitment, head_c).await {
                let entry =
                    self.staged_entry(block, wire.clone(), head_c, &items, &staged, outcome);
                self.insert_staged(commitment, entry).await;
            }
        }
        tracing::debug!(
            n = number,
            txs = items.len(),
            ms = t0.elapsed().as_secs_f64() * 1e3,
            "lean_build"
        );
        Ok((commitment, wire))
    }

    /// Drop OUR claim on `commitment` — and only ours. Returns whether the
    /// entry was removed.
    ///
    /// The id check is the whole point: a holder that stalled past the 2 s
    /// guard and was taken over must not delete the successor's claim when it
    /// eventually finishes. An unchecked `remove` there re-opens the window in
    /// which a third feed of the same bytes sees no claim, still passes the
    /// parent-link check (the head has not moved — the taker-over is still
    /// committing), and appends the same block a second time.
    pub async fn release_claim(&self, commitment: B256, id: u64) -> bool {
        use std::collections::hash_map::Entry;
        let mut fl = self.inflight.lock().await;
        match fl.entry(commitment) {
            Entry::Occupied(o) if o.get().1 == id => {
                o.remove();
                true
            }
            _ => false,
        }
    }

    /// Shim: `arc_stageBlock` — fully validate + EXECUTE against the current
    /// head state into a memory-only staged entry (vote-gap deferred anchor).
    /// Never appends, never queues (speculative). Bounded to 32 entries.
    ///
    /// CLAIM ORDER (2026-09-15, the stage/anchor race). The CL fires
    /// `arc_stageBlock` and then `arc_newBlock{commitment}` ~15 ms later,
    /// while the stage is still running (~18 ms at 2.2 MB: decode 6 + recover
    /// 5 + apply 7, plus the RPC layer's JSON parse and base64 decode of the
    /// ~3 MB body, which is *outside* that 18 ms). Whatever the anchor cannot
    /// see, it peer-pulls for 200-250 ms on the consensus critical path.
    ///
    /// So the commitment is published in `inflight` as the FIRST thing this
    /// function does — computed by `commitment_of_wire`, which hashes the
    /// borrowed wire bytes without materialising the tx list — and BEFORE the
    /// head read and the already-staged probe, both of which take locks that
    /// a concurrent commit can hold. A claim that turns out not to be ours is
    /// withdrawn microseconds later; an anchor that misses the claim pays 200
    /// ms. The claim is keyed on the block's DECLARED parent, which is what
    /// the head must equal for the stage to proceed, so the retention sweeps
    /// (`retain(|_, (p, _)| *p == new_head)`) see exactly what they saw before.
    pub async fn stage_block(&self, bytes: &[u8]) -> Result<StageOutcome, String> {
        let t0 = Instant::now();
        let hdr = LeanBlock::commitment_of_wire(bytes)?;
        let commitment_key = hdr.commitment;
        let number = hdr.number;

        // --- publish the claim before anything slow ---
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);
        let claim_id = next_claim_id();
        let claimed = {
            use std::collections::hash_map::Entry;
            let mut fl = self.inflight.lock().await;
            match fl.entry(commitment_key) {
                // someone already owns this commitment (a duplicate stage, or
                // a by-bytes commit): never clobber their signal
                Entry::Occupied(_) => None,
                Entry::Vacant(v) => {
                    v.insert((hdr.parent, claim_id, done_rx));
                    Some(claim_id)
                }
            }
        };
        if claimed.is_some() {
            self.stage_notify.notify_waiters();
        }

        let r = self.stage_claimed(bytes, hdr, t0).await;

        // --- release it in EVERY outcome: Staged, Syncing, and Err ---
        // A stage that bails on the head check (or on a decode error) must not
        // park the next anchor for the full 2 s guard. A panic drops `done_tx`
        // instead, which closes the channel and wakes waiters the same way.
        // `release_claim` is id-checked: a stage slow enough to have been taken
        // over signals its waiters but leaves the new owner's claim alone.
        if let Some(id) = claimed {
            let _ = done_tx.send(true);
            self.release_claim(commitment_key, id).await;
        }
        self.stage_notify.notify_waiters();

        let (outcome, t) = r?;
        if matches!(outcome, StageOutcome::Staged(_)) {
            tracing::debug!(
                n = number,
                total_ms = t0.elapsed().as_secs_f64() * 1e3,
                decode_ms = t.decode_ms,
                recover_ms = t.recover_ms,
                apply_ms = t.apply_ms,
                noop = t.noop,
                "lean_stage"
            );
        }
        Ok(outcome)
    }

    /// The body of `stage_block`, run with the commitment already claimed in
    /// `inflight`. Split out so that EVERY exit — including `?` on a decode
    /// error — passes through the caller's single release point.
    async fn stage_claimed(
        &self,
        bytes: &[u8],
        hdr: crate::chain::WireHeader,
        t_decode: Instant,
    ) -> Result<(StageOutcome, StageTiming), String> {
        let since = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
        let mut t = StageTiming::default();
        // PRELIMINARY head read: a cheap bail for a block that plainly does
        // not connect. NOT the one the entry is built on — see the atomic read
        // below.
        let (head_c0, head_n0, _) = *self.head.lock().await;
        if hdr.number != head_n0 + 1 || hdr.parent != head_c0 {
            return Ok((StageOutcome::Syncing { head: head_n0 }, t));
        }
        // Already staged against this head — our own `arc_buildBlock` result,
        // or a duplicate `arc_stageBlock`. A cheap no-op: the entry that is
        // there is a post-state of the same parent over the same bytes, so
        // re-executing would only pay ecrecover + a state clone to rebuild it.
        if self.already_staged(hdr.commitment, head_c0).await {
            t.decode_ms = since(t_decode);
            t.noop = true;
            return Ok((StageOutcome::Staged(hdr.commitment), t));
        }
        // the expensive half of the decode (tx bodies out of the wire). The
        // commitment is NOT recomputed: `hdr` is the header `commitment_of_wire`
        // produced over THESE bytes at the top of `stage_block` — the trust
        // boundary `from_wire_bytes_with_header` documents (it re-derives and
        // compares the three header fields always, the commitment in debug
        // builds, and takes `hdr.commitment` on faith in release).
        let block = LeanBlock::from_wire_bytes_with_header(bytes, hdr)?;
        t.decode_ms = since(t_decode);
        let t_recover = Instant::now();
        let items = decode_block_txs(&block.txs);
        t.recover_ms = since(t_recover);
        // ATOMIC (head, state). A staged entry is a post-state PLUS the parent
        // it claims to descend from, and promotion trusts that pairing rather
        // than re-executing. Reading the head in one critical section and
        // cloning the state in another let a commit land in between: the entry
        // then carried the OLD head as `staged_parent` and the NEW head's
        // state as its post-state — a block executed against a parent it does
        // not descend from, kept in the staged map and servable by commitment.
        //
        // LOCK ORDER: `head` then `state`, nested here and only here. It is the
        // order every committer already uses sequentially (`commit_gate` ->
        // `link_to_head`'s head read -> `state`), and nothing anywhere takes
        // `state` before `head`, so no cycle exists. The head guard spans
        // exactly one await — acquiring `state` — and nothing else; the clone
        // that follows was already paid under `state` alone, and a committer
        // blocked on `head` here would block on `state` a line later anyway.
        //
        // The re-check is also the authoritative linkage test: the preliminary
        // one above is only a fast bail.
        let t_apply = Instant::now();
        let (head_c, mut staged_state) = {
            let head = self.head.lock().await;
            let (head_c, head_n, _) = *head;
            if block.number != head_n + 1 || block.parent != head_c {
                return Ok((StageOutcome::Syncing { head: head_n }, t));
            }
            (head_c, self.state.lock().await.clone())
        };
        let outcome = apply_block(&mut staged_state, &items, self.cfg.beneficiary);
        let commitment = block.commitment;
        let entry = self.staged_entry(
            block,
            bytes.to_vec(),
            head_c,
            &items,
            &staged_state,
            outcome,
        );
        t.apply_ms = since(t_apply);
        // visible BEFORE the caller signals completion: a waiter woken by that
        // signal must find the entry it was waiting for.
        self.insert_staged(commitment, entry).await;
        Ok((StageOutcome::Staged(commitment), t))
    }

    /// Promote a staged block: append (fsync — the durability point), write
    /// the precomputed state delta, and do EXACTLY the bookkeeping of the full
    /// path — no re-validation, no re-execution.
    async fn promote_staged(&self, entry: StagedBlock) -> eyre::Result<()> {
        let StagedBlock {
            block,
            post_touched,
            changed,
            txs,
            evict,
            noops,
            outputs,
            ..
        } = entry;
        // Same gate, same last-moment link check as `commit_block`: taking the
        // staged entry out of `staged` makes this promote unique among
        // promotes, but not among APPENDS — a by-bytes feed that arrives after
        // the entry is taken finds nothing staged, takes the free claim, and
        // commits the same block.
        let _gate = self.commit_gate.lock().await;
        if self.link_to_head(&block).await? == Link::AlreadyCanonical {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        for (a, acct) in &post_touched {
            state.accounts.insert(*a, *acct);
        }
        for (a, acct) in &post_touched {
            self.provider.upsert(*a, *acct);
        }
        self.provider.set_head(block.number);
        let snapshot_due = self.cfg.snapshot_due(block.number);
        let (digest_state, commitment) = (snapshot_due.then(|| state.clone()), block.commitment);
        drop(state);

        self.log.lock().await.append(&block)?;
        self.by_commitment
            .lock()
            .await
            .insert(block.commitment, block.number);
        if let Some(s) = digest_state {
            s.write_snapshot(
                &self.cfg.datadir,
                commitment,
                block.number,
                block.timestamp_ms,
            )?;
        }

        *self.head.lock().await = (block.commitment, block.number, block.timestamp_ms);
        self.pool.remove_transactions(evict);
        self.pool.update_accounts(changed);
        // head advanced: keep entries that are still AHEAD of the new head
        // (losing candidates for this height are dropped when the height is
        // passed; bytes for the current height stay servable by commitment)
        let new_head = *self.head.lock().await;
        let new_head_n = new_head.1;
        self.staged
            .lock()
            .await
            .retain(|(_, e)| e.block.number >= new_head_n);
        self.inflight
            .lock()
            .await
            .retain(|_, (p, _, _)| *p == new_head.0);
        let m = &self.metrics;
        Metrics::inc(&m.blocks);
        Metrics::add(&m.txs, txs as u64);
        Metrics::add(&m.noops, noops as u64);
        Metrics::add(&m.outputs, outputs as u64);
        Metrics::inc(&m.promoted);
        Ok(())
    }

    /// Shim: `arc_newBlock` — decode strictly, recompute the commitment,
    /// verify the parent link, total-STF execute, append+fsync, evict.
    /// IDEMPOTENT by commitment: re-feeding the head or any ancestor returns
    /// its commitment as a no-op; a DIFFERENT block at a known height is a
    /// loud error (fork attempt); head+2 gaps are errors (feed in order).
    pub async fn shim_new(&self, bytes: &[u8]) -> Result<NewBlockOutcome, String> {
        let t0 = Instant::now();
        let path = PathCell::new(AnchorPath::Bytes);
        let r = self.shim_new_inner(bytes, &path).await;
        self.finish_anchor(&r, &path, t0);
        r
    }

    /// Count the route one `arc_newBlock` took and emit its debug event.
    fn finish_anchor(&self, r: &Result<NewBlockOutcome, String>, path: &PathCell, t0: Instant) {
        if matches!(r, Ok(NewBlockOutcome::Syncing { .. })) {
            path.set(AnchorPath::Syncing);
        }
        if r.is_ok() {
            self.metrics.record_anchor(path.get());
        }
        tracing::debug!(
            path = path.get().name(),
            ok = r.is_ok(),
            total_ms = t0.elapsed().as_secs_f64() * 1e3,
            "lean_anchor"
        );
    }

    /// The body of `arc_newBlock{blockBytes}`, labelling its route in `path`.
    async fn shim_new_inner(
        &self,
        bytes: &[u8],
        path: &PathCell,
    ) -> Result<NewBlockOutcome, String> {
        let block = LeanBlock::from_wire_bytes(bytes)?;
        let (head_c, head_n, _) = *self.head.lock().await;
        if block.number <= head_n {
            // known-height path: compare against our chain
            if block.number == 0 {
                return Err("block number 0 is the genesis sentinel".into());
            }
            let known = self
                .log
                .lock()
                .await
                .read_block(block.number)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("height {} predates the log", block.number))?;
            if known.commitment == block.commitment {
                path.set(AnchorPath::Index);
                return Ok(NewBlockOutcome::Valid(block.commitment));
            }
            return Err(format!(
                "conflicting block at known height {}: have {}, got {}",
                block.number, known.commitment, block.commitment
            ));
        }
        if block.number != head_n + 1 || block.parent != head_c {
            // Behind-the-tip machinery (Engine-API "SYNCING" analog): a block
            // that does not connect is QUEUED, never an error — the node heals
            // itself, consensus never orchestrates recovery. Commitments are
            // recomputed on ingest, so a forged queue entry can never apply.
            {
                let mut q = self.sync_queue.lock().await;
                if q.len() < 512 {
                    q.insert(block.parent, block);
                } // else: bounded — drop; a later push/backfill re-delivers
            }
            self.sync_notify.notify_one();
            return Ok(NewBlockOutcome::Syncing { head: head_n });
        }
        let commitment = block.commitment;
        let number = block.number;
        // deferred-anchor fast path: staged in the vote gap for THIS head?
        let take_staged = || async {
            let mut q = self.staged.lock().await;
            let pos = q
                .iter()
                .position(|(c, e)| *c == commitment && e.staged_parent == head_c);
            pos.map(|i| q.remove(i).unwrap().1)
        };
        let mut staged = take_staged().await;
        // v1.3.1 + 2026-09-15: the COMMIT of a block is now an exclusive claim
        // in `inflight`, taken get-or-insert under ONE guard, so two feeds of
        // the same bytes (the backfill's pull and the CL's anchor racing for
        // the same height) cannot both pass the parent-link check and append
        // it twice. Whoever loses the claim waits for the winner's result —
        // a bounded <=35 ms wait beats a ~230 ms redo — and then answers from
        // whatever the winner produced (a staged entry to promote, or a
        // canonical block to acknowledge).
        let mut owner: Option<(u64, tokio::sync::watch::Sender<bool>)> = None;
        if staged.is_none() {
            // Two rounds: wait for whoever holds the claim, then TAKE IT OVER
            // if it is still held. A holder that dies without signalling (a
            // panicked stager — `dead_inflight_staging_never_wedges_newblock`)
            // or one wedged past the guard must never cost us a block we can
            // commit: the second round reproduces exactly the pre-claim
            // behaviour of falling through to the full path.
            //
            // The takeover is ID-CHECKED: we only steal the claim we have
            // already waited out. A claim that appeared WHILE we waited belongs
            // to a holder that is live and may be committing right now, and
            // stealing that one puts two committers on the same block —
            // two identical records in `blocks.log`. In that case we fall out
            // of the loop with no claim and answer SYNCING, which is safe: the
            // CL retries and the backfill re-delivers.
            const CLAIM_ROUNDS: usize = 2;
            let mut waited_on: Option<u64> = None;
            for round in 0..CLAIM_ROUNDS {
                let (done_tx, done_rx) = tokio::sync::watch::channel(false);
                let my_id = next_claim_id();
                let waiter = {
                    let mut fl = self.inflight.lock().await;
                    match fl.get(&commitment) {
                        Some((_, id, rx)) if round + 1 < CLAIM_ROUNDS || waited_on != Some(*id) => {
                            waited_on = Some(*id);
                            Some(rx.clone())
                        }
                        _ => {
                            fl.insert(commitment, (head_c, my_id, done_rx));
                            None
                        }
                    }
                };
                let Some(mut rx) = waiter else {
                    owner = Some((my_id, done_tx));
                    break;
                };
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while !*rx.borrow() {
                        if rx.changed().await.is_err() {
                            break; // owner dropped (failed/panicked): fall through
                        }
                    }
                })
                .await;
                // a STAGING finished: promote its entry (the v1.3.1 case)
                staged = take_staged().await;
                if staged.is_some() {
                    break;
                }
                // a COMMIT finished: the block is ours already, anchoring it is
                // a no-op. Answer VALID instead of appending it a second time.
                let (head_c2, head_n2, _) = *self.head.lock().await;
                if head_c2 == commitment
                    || self.by_commitment.lock().await.get(&commitment) == Some(&number)
                {
                    path.refine(AnchorPath::Index);
                    return Ok(NewBlockOutcome::Valid(commitment));
                }
                if number != head_n2 + 1 || block.parent != head_c2 {
                    // head moved elsewhere while we waited: this no longer
                    // connects, so queue it rather than commit off-chain
                    {
                        let mut q = self.sync_queue.lock().await;
                        if q.len() < 512 {
                            q.insert(block.parent, block);
                        }
                    }
                    self.sync_notify.notify_one();
                    return Ok(NewBlockOutcome::Syncing { head: head_n2 });
                }
                // still committable and the claim is free again: take it
            }
        }
        match staged {
            Some(entry) => {
                debug_assert_eq!(entry.wire, bytes);
                path.refine(AnchorPath::Staged);
                if let Some((id, done_tx)) = owner.take() {
                    // never happens (a claim is only taken when nothing is
                    // staged), but leaving a claim behind would park the next
                    // feed of these bytes for the full 2 s guard
                    let _ = done_tx.send(true);
                    self.release_claim(commitment, id).await;
                }
                self.promote_staged(entry)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            None => {
                let Some((my_id, done_tx)) = owner else {
                    // Reachable BY DESIGN since the takeover became id-checked
                    // (see the claim rounds above): a claim that appeared while
                    // we waited belongs to a live holder, so we leave with none
                    // and refuse. Safe — the CL retries and the backfill
                    // re-delivers.
                    let head_n = self.head.lock().await.1;
                    return Ok(NewBlockOutcome::Syncing { head: head_n });
                };
                let items = decode_block_txs(&block.txs);
                let evict = items.iter().map(|(_, _, h)| *h).collect();
                let r = self.commit_block(block, items, evict).await;
                // release the claim in BOTH outcomes: `commit_block`'s own
                // retention sweep already drops the entry (its parent is the
                // old head), but a failed commit never reaches that sweep.
                let _ = done_tx.send(true);
                self.release_claim(commitment, my_id).await;
                r.map_err(|e| e.to_string())?;
            }
        }
        self.announce_to_peers(commitment, number);
        // Drain any queued children that now connect (loop: each apply may
        // unlock the next).
        let mut parent = commitment;
        loop {
            let next = self.sync_queue.lock().await.remove(&parent);
            let Some(nb) = next else { break };
            let items = decode_block_txs(&nb.txs);
            let evict = items.iter().map(|(_, _, h)| *h).collect();
            let c = nb.commitment;
            let num = nb.number;
            // Cheap early-out: the head may have moved past this child via
            // backfill while we committed its parent. This check is advisory
            // only — the drain holds NO claim, so an anchor or a backfill for
            // the same block can take the (free) claim in the window between
            // here and the append. `commit_block` re-takes the check under
            // `commit_gate` and answers that race with a no-op.
            let (hc, hn, _) = *self.head.lock().await;
            if nb.number != hn + 1 || nb.parent != hc {
                break;
            }
            self.commit_block(nb, items, evict)
                .await
                .map_err(|e| e.to_string())?;
            self.announce_to_peers(c, num);
            parent = c;
        }
        Ok(NewBlockOutcome::Valid(commitment))
    }

    /// Shim: `arc_newBlock{commitment}` — the CL only knows the commitment
    /// (it is in the EVM header). Idempotent for canonical blocks; otherwise
    /// apply from our staged/queued copy, else fetch the bytes from a peer by
    /// commitment, else SYNCING.
    pub async fn shim_new_by_commitment(&self, c: B256) -> Result<NewBlockOutcome, String> {
        let t0 = Instant::now();
        let path = PathCell::default();
        let r = self.new_by_commitment_inner(c, &path).await;
        self.finish_anchor(&r, &path, t0);
        r
    }

    /// Wait out a concurrent `arc_stageBlock` (or by-bytes commit) of exactly
    /// this commitment, so the anchor never peer-pulls a block that is being
    /// executed in this process right now. Returns whether it waited at all;
    /// the caller re-checks the index and `locate_wire`.
    ///
    /// Two windows, both covered:
    ///
    /// 1. The stage has NAMED itself — `inflight` holds `c`. Wait on its watch
    ///    channel, generously bounded (2 s), exactly as the by-bytes claim
    ///    path does. A dropped owner (panicked stager) closes the channel and
    ///    we fall through, so a dead stage can never wedge an anchor.
    /// 2. The stage has ARRIVED but not yet named itself — it is in the RPC
    ///    layer parsing ~3 MB of JSON/base64, or hashing the wire, and does
    ///    not know its own commitment yet. `stage_pending` is non-zero exactly
    ///    then. Wait on `stage_notify` up to `STAGE_RACE_GRACE`, rechecking
    ///    `inflight` each round. This window is real: the CL issues the anchor
    ///    ~15 ms after the stage, and the stage's pre-registration work is the
    ///    same order of magnitude.
    ///
    /// The grace in (2) is gated on `stage_pending > 0`, so a node doing a
    /// genuine backfill — nobody staging — adds zero latency to its peer
    /// pulls. Worst case when a stage is pending but is for a DIFFERENT block:
    /// `STAGE_RACE_GRACE` on top of a path that already costs 200-250 ms.
    async fn await_stage_race(&self, c: B256) -> bool {
        let grace = STAGE_RACE_GRACE;
        let t_wait = Instant::now();
        let mut waited = false;
        loop {
            let held = {
                let fl = self.inflight.lock().await;
                fl.get(&c).map(|(_, _, rx)| rx.clone())
            };
            if let Some(mut rx) = held {
                waited = true;
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while !*rx.borrow() {
                        if rx.changed().await.is_err() {
                            break; // owner dropped (failed/panicked): fall through
                        }
                    }
                })
                .await;
                break;
            }
            if self.stage_pending.load(Ordering::Relaxed) == 0 {
                break;
            }
            let left = grace.checked_sub(t_wait.elapsed()).unwrap_or_default();
            if left.is_zero() {
                break;
            }
            waited = true;
            // Bounded poll rather than a bare `notified()`: a notification
            // that fires between the `inflight` peek and this await would
            // otherwise be lost, and the cost of missing one is a 200 ms pull.
            let _ = tokio::time::timeout(
                left.min(std::time::Duration::from_millis(2)),
                self.stage_notify.notified(),
            )
            .await;
        }
        if waited {
            Metrics::inc(&self.metrics.stage_race_waits);
        }
        waited
    }

    /// THE COLD GRACE (`anchor_grace_ms`). `await_stage_race` covers a
    /// stage that is RUNNING or has ARRIVED; this covers the fleet's actual
    /// case, where the stage has done NEITHER when the anchor lands — nothing
    /// staged, nothing claimed, `stage_pending` zero — because the CL spawned
    /// it fire-and-forget and it is still encoding 2.5 MB of base64 (see
    /// `anchor_grace_ms`).
    ///
    /// Returns whether evidence of the stage appeared inside the window: a
    /// claim on `c`, a staged entry for `c`, or simply a stage ARRIVING (then
    /// `await_stage_race` takes over and waits it out properly). The caller
    /// re-resolves locally on `true` and falls through to the peer pull on
    /// `false`.
    ///
    /// THE LAGGARD. A node that is genuinely behind gets anchors for
    /// commitments no stage of its own will ever produce, and it must not pay
    /// the grace on every one of them. Two cheap reads say so — a non-empty
    /// `sync_queue` (blocks waiting on a parent we lack) or a `sync_target`
    /// above our head (the backfill is armed) — and both skip the grace
    /// entirely, exactly as `STAGE_RACE_GRACE` is skipped when no stage is
    /// pending. A node that is behind WITHOUT either flag set (its CL parked,
    /// or the first anchor after falling behind) still answers SYNCING/peer
    /// as before, just up to `anchor_grace_ms` later — 60 ms on top of a
    /// path that already costs 200-250 ms, and it re-converges by the same
    /// route it always did.
    async fn await_anchor_grace(&self, c: B256) -> bool {
        let grace = std::time::Duration::from_millis(self.cfg.anchor_grace_ms);
        if grace.is_zero() {
            return false;
        }
        // behind the network: no stage of ours is coming for this commitment
        let head_n = self.head.lock().await.1;
        if self.sync_target.load(Ordering::Relaxed) > head_n
            || !self.sync_queue.lock().await.is_empty()
        {
            return false;
        }
        let t_wait = Instant::now();
        let hit = loop {
            // A stage that has arrived at the RPC layer is enough: hand it to
            // `await_stage_race`, whose job is precisely that window.
            if self.stage_pending.load(Ordering::Relaxed) > 0 {
                break true;
            }
            if self.inflight.lock().await.contains_key(&c) {
                break true;
            }
            if self.staged.lock().await.iter().any(|(k, _)| *k == c) {
                break true;
            }
            let left = grace.checked_sub(t_wait.elapsed()).unwrap_or_default();
            if left.is_zero() {
                break false;
            }
            // Bounded poll rather than a bare `notified()`: `stage_notify` is
            // signalled on claim insert, on staged insert and when a pending
            // stage returns, but one that fires between the checks above and
            // this await would otherwise be lost — and missing one costs a
            // 200 ms pull, which is the whole thing we are here to avoid.
            let _ = tokio::time::timeout(
                left.min(std::time::Duration::from_millis(2)),
                self.stage_notify.notified(),
            )
            .await;
        };
        Metrics::inc(if hit {
            &self.metrics.anchor_grace_hits
        } else {
            &self.metrics.anchor_grace_misses
        });
        hit
    }

    /// Re-resolve `c` from LOCAL sources only: wait out any in-flight stage of
    /// it, re-read the commitment index, then `locate_wire` (staged -> sync
    /// queue -> index). `Ok(None)` means the block is still
    /// nowhere in this process and the caller may spend a peer round trip.
    ///
    /// `always_locate` forces the `locate_wire` even when `await_stage_race`
    /// waited on nothing. The hot path leaves it off on its second attempt —
    /// no wait means nothing can have changed since the first locate.
    /// The cold-grace caller turns it on, because its evidence may be a STAGED
    /// ENTRY, which carries no claim for `await_stage_race` to see.
    async fn resolve_local(
        &self,
        c: B256,
        path: &PathCell,
        always_locate: bool,
    ) -> Result<Option<NewBlockOutcome>, String> {
        let waited = self.await_stage_race(c).await;
        if waited && self.by_commitment.lock().await.contains_key(&c) {
            path.set(AnchorPath::Index);
            return Ok(Some(NewBlockOutcome::Valid(c)));
        }
        if !waited && !always_locate {
            return Ok(None);
        }
        if let Some((bytes, src)) = self.locate(c).await? {
            path.set(src);
            return self.shim_new_inner(&bytes, path).await.map(Some);
        }
        Ok(None)
    }

    async fn new_by_commitment_inner(
        &self,
        c: B256,
        path: &PathCell,
    ) -> Result<NewBlockOutcome, String> {
        // Idempotency check against the commitment index, which is complete
        // for every canonical block. The block being decided is by definition
        // not canonical yet, so a miss here is the normal case.
        if self.by_commitment.lock().await.contains_key(&c) {
            path.set(AnchorPath::Index);
            return Ok(NewBlockOutcome::Valid(c));
        }
        // A staging or a by-bytes COMMIT of exactly this block may be in
        // flight (our CL's own `arc_stageBlock`, fired ~15 ms ago and still
        // running; or the gossip backfill racing this anchor for the height).
        // Wait for that result instead of queueing our own `state` and `log`
        // locks behind it: if it was a commit, the block is canonical and this
        // anchor is an idempotent VALID; if it was a staging, `locate_wire`
        // finds the staged entry instead of falling through to a peer fetch.
        if let Some(o) = self.resolve_local(c, path, true).await? {
            return Ok(o);
        }
        // LAST CHANCE before spending 200-250 ms on a peer: a stage may have
        // registered while we ran `locate_wire`. Same bounded wait, then one
        // more locate — still cheaper than the round trip it replaces.
        if let Some(o) = self.resolve_local(c, path, false).await? {
            return Ok(o);
        }
        // ...and the fleet's real case: the stage has not ARRIVED yet, so
        // neither wait above had anything to wait on. Sit out the cold grace
        // and re-resolve once if it pays off (see `await_anchor_grace`).
        if self.await_anchor_grace(c).await {
            if let Some(o) = self.resolve_local(c, path, true).await? {
                return Ok(o);
            }
        }
        for cl in &self.peer_clients {
            use jsonrpsee::core::client::ClientT;
            let mut params = jsonrpsee::core::params::ObjectParams::new();
            let _ = params.insert("commitment", format!("{c}"));
            let r: Result<serde_json::Value, _> = cl.request("arc_getBlockBytes", params).await;
            let Ok(v) = r else { continue };
            let Some(b64) = v.get("blockBytes").and_then(|x| x.as_str()) else {
                continue;
            };
            let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
            else {
                continue;
            };
            match LeanBlock::from_wire_bytes(&bytes) {
                Ok(b) if b.commitment == c => {
                    Metrics::inc(&self.metrics.block_pulls);
                    path.set(AnchorPath::Peer);
                    return self.shim_new_inner(&bytes, path).await;
                }
                _ => continue, // peer served the wrong block: ignore it
            }
        }
        // every peer missed or failed
        let head_n = self.head.lock().await.1;
        Ok(NewBlockOutcome::Syncing { head: head_n })
    }

    /// Fire-and-forget gossip: after appending a block, ANNOUNCE it
    /// ({commitment, number}, ~100 bytes) — never the block bytes. On a
    /// healthy chain every validator's CL anchors every block into its own
    /// node, so full-block pushes were 100% redundant link saturation
    /// (measured: wifi validator moved 11.2MB per 1.65MB block under v1.1
    /// push). Receivers pull only what they don't have.
    fn announce_to_peers(&self, commitment: B256, number: u64) {
        for c in self.peer_clients.clone() {
            tokio::spawn(async move {
                use jsonrpsee::core::client::ClientT;
                let mut params = jsonrpsee::core::params::ObjectParams::new();
                let _ = params.insert("commitment", format!("{commitment}"));
                let _ = params.insert("number", number);
                let _ = c
                    .request::<serde_json::Value, _>("arc_announceBlock", params)
                    .await;
            });
        }
    }

    /// Do we already hold the announced block? Three ways, all hash lookups:
    /// the exact commitment is STAGED (we validated and executed it for
    /// consensus in the vote gap), a staging/commit for it is IN FLIGHT, or —
    /// for the head+1 case — we hold *some* staged candidate at that height
    /// (the announcer may be naming a different candidate for the height our
    /// CL is about to anchor; either way the anchor, not a pull, settles it).
    ///
    /// No log read, no commitment resolve.
    async fn holds_announced(&self, commitment: B256, number: u64, head_n: u64) -> bool {
        {
            let q = self.staged.lock().await;
            if q.iter().any(|(c, e)| {
                *c == commitment || (number == head_n + 1 && e.block.number == number)
            }) {
                return true;
            }
        }
        self.inflight.lock().await.contains_key(&commitment)
    }

    /// Handle a peer's announcement: known -> ignore (the universal in-sync
    /// case, zero further traffic); ALREADY HELD (staged/in-flight) -> ignore,
    /// our own CL anchors it in milliseconds; head+1 and not held -> wait out
    /// `announce_grace_ms` for that anchor, then arm the backfill if it
    /// never came; further ahead -> arm the backfill at once (existing
    /// machinery — this is the genuine laggard).
    ///
    /// WHY the grace (2026-09-15). A validator stages every height for the
    /// vote gap, so when a peer's announcement for head+1 lands it already has
    /// the block, fully executed, in memory — and the CL's `arc_newBlock`
    /// anchor for it is milliseconds behind. Arming the backfill there pulled
    /// 2.5 MB per announcing peer per height and ran a SECOND commit of the
    /// same block (`apply_block` + provider upserts under the `state` lock)
    /// that the anchor then had to wait behind: measured on the 4-validator
    /// fleet as ~1.5 `lean_anchor` lines per height, duplicate on 617 of 781
    /// heights, `pre_ms` p90 226-443 ms and max 1.5 s. The self-healing
    /// property is untouched: a node whose CL is parked holds nothing staged,
    /// so it arms after the grace, and a node more than one height behind arms
    /// immediately.
    ///
    /// This path never touches the log: it decides on numbers plus two
    /// in-memory hash lookups — a block above our head cannot be in our log,
    /// and one at or below it is never pulled backwards.
    pub async fn on_announce(&self, commitment: B256, number: u64) {
        Metrics::inc(&self.metrics.announces_rx);
        let head_n = self.head.lock().await.1;
        if number <= head_n {
            // known height: our chain already has a block there (commitment
            // equality is irrelevant to traffic — never pull backwards)
            Metrics::inc(&self.metrics.announces_known);
            return;
        }
        if self.holds_announced(commitment, number, head_n).await {
            Metrics::inc(&self.metrics.announce_skipped_staged);
            return;
        }
        if number == head_n + 1 {
            // One height behind and nothing staged for it. Either our CL is
            // about to anchor it (the common case: the announcement simply
            // beat our own `arc_stageBlock`/`arc_newBlock`) or our CL is gone.
            // Give the anchor the grace window before spending a pull.
            let grace = self.cfg.announce_grace_ms;
            if grace > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(grace)).await;
                let head_n = self.head.lock().await.1;
                if number <= head_n {
                    Metrics::inc(&self.metrics.announces_known);
                    return;
                }
                if self.holds_announced(commitment, number, head_n).await {
                    Metrics::inc(&self.metrics.announce_skipped_staged);
                    return;
                }
            }
        }
        // genuinely behind: raise the target and let the backfill pull the gap
        // (for head+1 the backfill's first fetch IS the direct pull-from-peers;
        // the announcer is in the peer set and provably serves it)
        self.sync_target.fetch_max(number, Ordering::Relaxed);
        self.sync_notify.notify_one();
    }

    /// Background backfill: when a SYNCING entry exists, fetch gap blocks
    /// (head+1 upward) from peers and apply in order; queued blocks drain via
    /// the shim_new apply path. Gives up after ~10s without progress; retried
    /// on the next trigger.
    pub fn spawn_sync(self: &std::sync::Arc<Self>) {
        let node = self.clone();
        tokio::spawn(async move {
            loop {
                node.sync_notify.notified().await;
                let mut last_progress = std::time::Instant::now();
                loop {
                    let behind_target =
                        node.head.lock().await.1 < node.sync_target.load(Ordering::Relaxed);
                    if node.sync_queue.lock().await.is_empty() && !behind_target {
                        break;
                    }
                    if last_progress.elapsed() > std::time::Duration::from_secs(10) {
                        break; // give up; next SYNCING trigger retries
                    }
                    let (_, head_n, _) = *node.head.lock().await;
                    let want = head_n + 1;
                    let mut got = false;
                    for c in &node.peer_clients {
                        use jsonrpsee::core::client::ClientT;
                        let mut params = jsonrpsee::core::params::ObjectParams::new();
                        let _ = params.insert("number", want);
                        let r: Result<serde_json::Value, _> =
                            c.request("arc_getBlockBytes", params).await;
                        if let Ok(v) = r {
                            if let Some(b64) = v.get("blockBytes").and_then(|x| x.as_str()) {
                                if let Ok(bytes) = base64::Engine::decode(
                                    &base64::engine::general_purpose::STANDARD,
                                    b64,
                                ) {
                                    Metrics::inc(&node.metrics.block_pulls);
                                    Metrics::inc(&node.metrics.backfill_pulls);
                                    let path = PathCell::default();
                                    if matches!(
                                        node.shim_new_inner(&bytes, &path).await,
                                        Ok(NewBlockOutcome::Valid(_))
                                    ) {
                                        got = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    if got {
                        last_progress = std::time::Instant::now();
                        continue; // progress: immediately try the next number
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        });
    }

    /// Shim: `arc_getBlockBytes` — serve a block's consensus wire bytes from
    /// the log (sync path).
    pub async fn block_wire_bytes(&self, number: u64) -> Result<Option<Vec<u8>>, String> {
        // Read under the log mutex, SERIALISE outside it. `to_wire_bytes`
        // copies the whole block (2.5 MB on the fleet) and the log mutex is
        // exactly what the decide anchor's append needs — as one temporary in
        // the return expression the guard lived across the copy, so every
        // peer/CL `arc_getBlockBytes` stalled the next height's append by a
        // memcpy it had no reason to wait for.
        let block = {
            self.log
                .lock()
                .await
                .read_block(number)
                .map_err(|e| e.to_string())?
        };
        Ok(block.map(|b| b.to_wire_bytes()))
    }

    /// Resolve a commitment to block bytes: staged, then the sync queue, then
    /// the commitment index (complete from height 1, so a miss there is final).
    /// Returns the bytes and WHERE they came from.
    async fn locate(&self, c: B256) -> Result<Option<(Vec<u8>, AnchorPath)>, String> {
        let t0 = Instant::now();
        let r = self.locate_inner(c).await;
        tracing::debug!(
            hit = match &r {
                Ok(Some((_, p))) => p.name(),
                Ok(None) => "miss",
                Err(_) => "error",
            },
            ms = t0.elapsed().as_secs_f64() * 1e3,
            "lean_locate"
        );
        r
    }

    async fn locate_inner(&self, c: B256) -> Result<Option<(Vec<u8>, AnchorPath)>, String> {
        // Staged and queued copies are exactly what the decide anchor asks for;
        // check them before anything that can touch the log.
        if let Some((_, e)) = self.staged.lock().await.iter().find(|(k, _)| *k == c) {
            return Ok(Some((e.wire.clone(), AnchorPath::Staged)));
        }
        if let Some(b) = self
            .sync_queue
            .lock()
            .await
            .values()
            .find(|b| b.commitment == c)
        {
            return Ok(Some((b.to_wire_bytes(), AnchorPath::Queued)));
        }
        // Drop the index guard BEFORE reading the log: `block_wire_bytes`
        // awaits the log lock, and holding the commitment index across that
        // await serialises every other lookup behind one disk read.
        let hit = { self.by_commitment.lock().await.get(&c).copied() };
        match hit {
            Some(n) => Ok(self
                .block_wire_bytes(n)
                .await?
                .map(|w| (w, AnchorPath::Index))),
            None => Ok(None),
        }
    }

    /// Shim: `arc_getBlockBytes{commitment}` — canonical, staged or queued
    /// block with that commitment.
    pub async fn block_wire_bytes_by_commitment(&self, c: B256) -> Result<Option<Vec<u8>>, String> {
        Ok(self.locate(c).await?.map(|(w, _)| w))
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
