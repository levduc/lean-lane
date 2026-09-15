//! The lane node composition: reth's pool as a LIBRARY (validator + ordering +
//! eviction, unmodified — exactly the 2a wiring) + flat-state total STF +
//! append-only log + a build-execute-commit driver loop.
//!
//! The driver is BOTH the offline harness and the future proposer path; the CL
//! shim splits it into `buildBlock(budget)` / `newBlock(bytes)` / `getHead`
//! (contract listed in LEAN-NATIVE.md; not implemented here).

use crate::chain::{genesis_commitment, AppendTiming, BlockLog, LeanBlock};
use crate::exec::{apply_block, LeanReceipt};
use crate::state::{Acct, FlatState};
use alloy_eips::eip2718::Decodable2718;
use alloy_consensus::Transaction as _;
use alloy_primitives::{Address, B256, U256};
use lean_native::envelope::{set_lane_chain_id, ArcTxEnvelope, LEAN_TX_TYPE};
use lean_native::pool::ArcPooledTx;
use lean_native::LeanTx;
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::SignerRecoverable;
use crate::provider::LeanProvider;
use reth_transaction_pool::blobstore::InMemoryBlobStore;
use reth_transaction_pool::validate::{EthTransactionValidator, EthTransactionValidatorBuilder};
use reth_transaction_pool::{
    CoinbaseTipOrdering, Pool, PoolConfig, PoolTransaction, SubPoolLimit, TransactionOrigin,
    TransactionPool, TransactionPoolExt,
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

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
    pub budget_gas: u64,
    /// Blocks between full state snapshots. **0 means NEVER snapshot** — the
    /// node then recovers by replaying the whole log from genesis, which is
    /// what a short-lived test or a throwaway datadir wants. Any other value
    /// is a modulus (see `snapshot_due`).
    pub snapshot_every: u64,
    pub beneficiary: Address,
    /// How many of the most recent receipts to keep in memory for
    /// `eth_getTransactionReceipt`. Nothing else reads them and they are never
    /// persisted (`exec::LeanReceipt` is derived, not stored), so this is pure
    /// RSS with a fan-out-dependent price: a receipt is 96 B plus 64 B per
    /// non-zero output, i.e. ~160 B at N=1 but ~6.4 KB at N=100 — 32 MB and
    /// ~1.3 GB respectively at the 200k default. Tune it to the fan-out the
    /// node actually carries (`--receipt-ring`); 0 keeps none at all.
    pub receipt_ring: usize,
    /// Other lean nodes' RPC urls (backfill source + push-on-append targets).
    pub peers: Vec<String>,
}

impl Config {
    /// Is block `number` a snapshot point? `snapshot_every == 0` disables
    /// snapshots entirely rather than panicking on `% 0`, which is what both
    /// append paths used to do the first time they committed a block.
    pub fn snapshot_due(&self, number: u64) -> bool {
        self.snapshot_every != 0 && number % self.snapshot_every == 0
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            datadir: PathBuf::from("lean-lane-data"),
            chain_id: 1338,
            budget_gas: 150_000_000,
            snapshot_every: 1024,
            beneficiary: Address::with_last_byte(0xbe),
            receipt_ring: 200_000,
            peers: Vec::new(),
        }
    }
}

#[derive(Default)]
pub struct Stats {
    pub admit_ns: AtomicU64,
    pub admitted: AtomicU64,
    pub rejected: AtomicU64,
    pub build_ns: AtomicU64,
    pub exec_ns: AtomicU64,
    pub append_ns: AtomicU64,
    pub blocks: AtomicU64,
    pub txs: AtomicU64,
    pub noops: AtomicU64,
    pub outputs: AtomicU64,
    /// gossip telemetry (bandwidth-gate observability)
    pub promoted: AtomicU64,
    pub announces_rx: AtomicU64,
    pub announces_known: AtomicU64,
    pub block_pulls: AtomicU64,
    /// Announcements ignored because we ALREADY HOLD the block (staged for
    /// the vote gap, a staging/commit in flight, or a staged entry at that
    /// height): our own CL's anchor is milliseconds away, so pulling 2.5 MB
    /// from the announcer only to promote the copy we already executed is
    /// pure duplicate work on the consensus critical path.
    pub announce_skipped_staged: AtomicU64,
    /// Block-byte pulls made by the BACKFILL loop specifically. `block_pulls`
    /// counts these AND the by-commitment anchor's peer fetch; this one is the
    /// "are we pulling blocks we did not need" number.
    pub backfill_pulls: AtomicU64,
    /// backward log scans started by `locate_by_commitment` (must stay 0 on the anchor path)
    pub log_scans: AtomicU64,
    /// Anchors (`arc_newBlock{commitment}`) that WAITED on a concurrent
    /// `arc_stageBlock` of the same block instead of pulling it from a peer.
    /// The CL fires the stage and then the anchor ~15 ms later, while the
    /// stage is still executing; every increment here is a ~200 ms peer round
    /// trip that did not happen. Read it next to `path=staged` +
    /// `wait_inflight_ms>0` in the `lean_anchor` line.
    pub stage_race_waits: AtomicU64,
    /// last-block service times (ns) for build / stage / append. The running
    /// totals above answer "per output over the whole run"; these answer "is
    /// this node slower NOW than it was ten minutes ago", which is the only
    /// question a drifting cadence asks.
    pub build_ns_last: AtomicU64,
    pub stage_ns_last: AtomicU64,
    pub append_ns_last: AtomicU64,
}

/// `LEAN_ANNOUNCE_GRACE_MS` (default 250, read once). How long an announcement
/// for head+1 that we do NOT already hold waits for our own CL to anchor that
/// height before arming the backfill. It exists only for the node whose CL is
/// parked or dead: on a healthy validator the anchor always wins this race, so
/// the grace costs nothing and saves a 2.5 MB pull per announcing peer per
/// height. `0` disables it (pull immediately, the pre-2026-09-15 behaviour).
pub fn announce_grace_ms() -> u64 {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MS.get_or_init(|| {
        std::env::var("LEAN_ANNOUNCE_GRACE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(250)
    })
}

/// `LEAN_STAGE_RACE_MS` (default 40, read once). How long an `arc_newBlock`
/// that holds none of the block waits for an `arc_stageBlock` which has
/// ARRIVED but not yet registered its commitment (see `await_stage_race`).
/// Only ever waited when `stage_pending > 0`. `0` disables the grace and
/// leaves only the in-flight-claim wait (the pre-2026-09-15 behaviour).
///
/// 40 ms is sized off the fleet: the CL issues the anchor ~15 ms after the
/// stage, and at 2.2 MB the stage needs ~15-20 ms of JSON parse + base64
/// decode + wire hash before it can name itself. The alternative on a miss is
/// a 200-250 ms peer round trip, so the grace is cheap even when it expires.
pub fn stage_race_ms() -> u64 {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MS.get_or_init(|| {
        std::env::var("LEAN_STAGE_RACE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(40)
    })
}

/// `LEAN_STATS=1` (read once). Gates every per-call timing line.
pub fn stats_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("LEAN_STATS").as_deref() == Ok("1"))
}

#[inline]
fn add_ns(acc: &AtomicU64, t: Instant) {
    acc.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

#[inline]
fn ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

/// Acquire a mutex, charging the WAIT to `acc`. The whole point of the anchor
/// line is to separate "this node was busy" from "this node was blocked
/// behind stage/ingest/serving", so every lock on the anchor path is taken
/// through here.
async fn timed_lock<'a, T>(
    m: &'a Mutex<T>,
    acc: &AtomicU64,
) -> tokio::sync::MutexGuard<'a, T> {
    let t = Instant::now();
    let g = m.lock().await;
    add_ns(acc, t);
    g
}

/// Which route an `arc_newBlock` took. Kept as a u8 so the timer can be
/// shared by `&` across awaits (a `Cell` would make the future !Send).
pub const PATH_OTHER: u8 = 0;
pub const PATH_INDEX: u8 = 1;
pub const PATH_STAGED: u8 = 2;
pub const PATH_QUEUED: u8 = 3;
pub const PATH_PEER: u8 = 4;
pub const PATH_SYNCING: u8 = 5;
pub const PATH_BYTES: u8 = 6;
const PATH_NAMES: [&str; 7] =
    ["other", "index", "staged", "queued", "peer", "syncing", "bytes"];

/// Per-call decomposition of ONE `arc_newBlock` (the decide anchor). All
/// atomics: the timer is passed by `&` through `shim_new` -> `promote_staged`
/// / `commit_block`, which are async and must stay Send.
///
/// `total_ms` is measured from RPC-HANDLER ENTRY, not from the first thing
/// the node does: `pre_ms` is everything before path dispatch (JSON parse,
/// base64 decode of a ~3.4 MB body, tokio scheduling), and `locate_ms` /
/// `peer_ms` cover the by-commitment resolve that used to be invisible. What
/// is left in the residual is the response encode and the `Instant` reads
/// themselves.
///
/// `n=0` means the height was never learned: a by-commitment call that
/// resolved to nothing (`path=syncing`).
#[derive(Default)]
pub struct AnchorTimer {
    pub number: AtomicU64,
    path: AtomicU8,
    /// RPC handler entry -> path dispatch. Taken BEFORE any lock, any `head`
    /// read and any parsing, so it is the one field nothing inside the node
    /// can be blamed for: param parse, base64 decode, and however long the
    /// task sat in the tokio queue before it ran.
    pub pre_ns: AtomicU64,
    /// `locate_wire` on the by-commitment route: staged map -> sync queue ->
    /// commitment index -> log read -> bounded backward scan. Previously
    /// UNMEASURED — a `path=index` hit that had to read and re-serialise the
    /// block out of the log (log mutex + a 2.5 MB copy) paid entirely here
    /// and surfaced only as `total_ms` residual. This is the ~380 ms that
    /// `index_ms≈6 decode_ms≈6` could not account for.
    pub locate_ns: AtomicU64,
    /// The peer `arc_getBlockBytes{commitment}` round trip (`path=peer`).
    pub peer_ns: AtomicU64,
    pub wait_inflight_ns: AtomicU64,
    pub lock_ns: AtomicU64,
    pub apply_ns: AtomicU64,
    pub append_ns: AtomicU64,
    pub fsync_ns: AtomicU64,
    pub index_ns: AtomicU64,
    pub announce_ns: AtomicU64,
    /// wire decode + ecrecover (not in the requested field set; reported as a
    /// trailing field because at 1.24 MB / 431 txs it is the largest residual)
    pub decode_ns: AtomicU64,
    /// reth pool bookkeeping: remove_transactions + update_accounts
    pub pool_ns: AtomicU64,
}

impl AnchorTimer {
    fn set_path(&self, p: u8) {
        self.path.store(p, Ordering::Relaxed);
    }
    /// Only refine the default/bytes label — never overwrite an explicit
    /// source (peer/queued/index) with a later, less specific one.
    fn refine_path(&self, p: u8) {
        let cur = self.path.load(Ordering::Relaxed);
        if cur == PATH_OTHER || cur == PATH_BYTES {
            self.path.store(p, Ordering::Relaxed);
        }
    }
    fn emit(&self, t0: Instant) {
        if !stats_enabled() {
            return;
        }
        println!("{}", self.line(t0));
    }
    /// The `lean_anchor` line itself. Split out from `emit` so the field set
    /// is gated by a test rather than by reading a fleet log and noticing
    /// something is missing (which is how `locate_ms` came to be needed).
    pub fn line(&self, t0: Instant) -> String {
        let g = |a: &AtomicU64| ms(a.load(Ordering::Relaxed));
        format!(
            concat!(
                "lean_anchor n={} path={} total_ms={:.2} pre_ms={:.2} ",
                "locate_ms={:.2} peer_ms={:.2} wait_inflight_ms={:.2} ",
                "lock_ms={:.2} apply_ms={:.2} append_ms={:.2} fsync_ms={:.2} ",
                "index_ms={:.2} announce_ms={:.2} rss_mb={} ",
                "decode_ms={:.2} pool_ms={:.2}",
            ),
            self.number.load(Ordering::Relaxed),
            PATH_NAMES[self.path.load(Ordering::Relaxed) as usize % PATH_NAMES.len()],
            ms(t0.elapsed().as_nanos() as u64),
            g(&self.pre_ns),
            g(&self.locate_ns),
            g(&self.peer_ns),
            g(&self.wait_inflight_ns),
            g(&self.lock_ns),
            g(&self.apply_ns),
            g(&self.append_ns),
            g(&self.fsync_ns),
            g(&self.index_ns),
            g(&self.announce_ns),
            rss_mb(),
            g(&self.decode_ns),
            g(&self.pool_ns),
        )
    }
}

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
///      take `head`, `state`, `log`, `receipts`, `by_commitment`,
///      `scan_misses`, `staged` and `inflight` under it — one at a time.
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
    pub receipts: Mutex<VecDeque<LeanReceipt>>,
    pub stats: Stats,
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
    /// commitment -> block number for canonical blocks known to this
    /// process (replayed at open, appended since). Misses fall back to a
    /// bounded backward scan of the log (see `locate_by_commitment`).
    pub by_commitment: Mutex<std::collections::HashMap<B256, u64>>,
    /// Commitments a bounded backward scan has already failed to find
    /// (bounded to 4096). Turns a repeated unknown-commitment lookup into one
    /// hash check instead of another full scan.
    ///
    /// Invalidation is EXACT, not wholesale: a miss for commitment `c` can
    /// only turn into a hit when `c` ITSELF is appended, so an append removes
    /// only its own commitment. The old blanket `clear()` on every append
    /// defeated the cache entirely in steady state (a node appending ~2
    /// blocks/s never kept a miss long enough to use it), so every repeated
    /// lookup of a commitment nobody has — exactly what the CL's value-sync
    /// serving path generates — paid another bounded backward log scan.
    pub scan_misses: Mutex<std::collections::HashSet<B256>>,
    /// Lowest block number `by_commitment` is COMPLETE from (everything at or
    /// above it that this node has appended is indexed: replay fills it at
    /// open, every append adds to it). A backward log scan can therefore only
    /// ever discover something BELOW this — above it, a miss is proof of
    /// absence. In steady state (head more than SCAN_LIMIT past the snapshot
    /// the process opened on) this makes the scan range empty, which is the
    /// point: serving `arc_getBlockBytes{commitment}` for a commitment we do
    /// not have used to read+hash up to 1024 blocks (1.24 MB each) per ask.
    pub index_floor: AtomicU64,
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
    /// post-state of every touched account (delta to write on promote)
    post_touched: Vec<(Address, crate::state::Acct)>,
    changed: Vec<reth_execution_types::ChangedAccount>,
    receipts: Vec<LeanReceipt>,
    evict: Vec<B256>,
    applied: usize,
    noops: usize,
    outputs: usize,
}

/// Replay `log` onto `state`, starting from the snapshot `snap` = (commitment,
/// number, timestamp_ms) that `state` was loaded from. Each replayed tx pays
/// one decode + one ecrecover — the honest recovery cost. Returns the new head
/// plus what it took to get there: (head, blocks, txs, commitment index).
fn replay_onto(
    state: &mut FlatState,
    log: &mut BlockLog,
    snap: (B256, u64, u64),
    beneficiary: Address,
) -> std::io::Result<((B256, u64, u64), u64, u64, std::collections::HashMap<B256, u64>)> {
    let (mut head, mut blocks, mut txs) = (snap, 0u64, 0u64);
    let mut by_commitment = std::collections::HashMap::new();
    log.replay(snap.1, |block| {
        let items = decode_block_txs(&block.txs);
        txs += items.len() as u64;
        apply_block(state, &items, block.number, beneficiary);
        head = (block.commitment, block.number, block.timestamp_ms);
        by_commitment.insert(block.commitment, block.number);
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
        let mut log = BlockLog::open(&cfg.datadir.join("blocks.log"))?;
        let beneficiary = cfg.beneficiary;
        let snap = (snap_commitment, snap_number, snap_ts);
        let (mut head, mut replayed_blocks, mut replayed_txs, mut by_commitment) =
            replay_onto(&mut state, &mut log, snap, beneficiary)?;

        // SNAPSHOT AHEAD OF THE LOG (only reachable under `LEAN_FSYNC=0`, which
        // fsyncs the snapshot but not the append — see `chain::fsync_enabled`).
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
                None => (FlatState::default(), (genesis_commitment(cfg.chain_id), 0, 0)),
            };
            eprintln!(
                concat!(
                    "lean_recovery WARNING snapshot {} is AHEAD of the block log ",
                    "(last intact record {}): heights {}..={} were lost with the ",
                    "unsynced log tail. Restarting from snapshot {} and replaying; ",
                    "the backfill re-pulls the rest from peers.",
                ),
                snap_number,
                log_head,
                log_head + 1,
                snap_number,
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
        let big = SubPoolLimit { max_txs: 400_000, max_size: 512 * 1024 * 1024 };
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
                receipts: Mutex::new(VecDeque::new()),
                stats: Stats::default(),
                sync_queue: Mutex::new(Default::default()),
                sync_notify: tokio::sync::Notify::new(),
                sync_target: AtomicU64::new(0),
                staged: Mutex::new(Default::default()),
                by_commitment: Mutex::new(by_commitment),
                scan_misses: Mutex::new(Default::default()),
                // replay indexed every block above the snapshot it started
                // from; below that the index is cold and a scan can still pay
                index_floor: AtomicU64::new(snap_number + 1),
                inflight: Mutex::new(Default::default()),
                stage_pending: AtomicU64::new(0),
                stage_notify: tokio::sync::Notify::new(),
                peer_clients,
            },
            recovery,
        ))
    }

    /// Push a block's receipts into the ring, evicting the oldest past
    /// `cfg.receipt_ring`.
    ///
    /// `>=`, not `==`: a ring sized 0 means keep none, and the old `==` test
    /// grew UNBOUNDED there (len 0 pops nothing, pushes one, and never matches
    /// 0 again). A shrunk ring also has to drain down to its new size rather
    /// than sit above it forever.
    fn keep_receipts(
        &self,
        ring: &mut VecDeque<LeanReceipt>,
        receipts: impl IntoIterator<Item = LeanReceipt>,
    ) {
        let cap = self.cfg.receipt_ring;
        if cap == 0 {
            ring.clear();
            return;
        }
        for r in receipts {
            while ring.len() >= cap {
                ring.pop_front();
            }
            ring.push_back(r);
        }
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
        let t0 = Instant::now();
        if bytes.first() != Some(&LEAN_TX_TYPE) {
            self.stats.rejected.fetch_add(1, Ordering::Relaxed);
            return Err("only lean (0x50) transactions are accepted on the lane".into());
        }
        let env = ArcTxEnvelope::decode_2718(&mut &bytes[..])
            .map_err(|e| format!("decode: {e}"))?;
        let signer = env.recover_signer().map_err(|e| format!("signature: {e}"))?;
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
        match &res {
            Ok(_) => {
                self.stats.admitted.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.stats.rejected.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.stats.admit_ns.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        res
    }

    /// Pull best txs from the pool under the gas budget (no state change).
    async fn pick_txs(&self) -> (Vec<(Address, LeanTx, B256)>, Vec<Vec<u8>>, Vec<B256>) {
        let mut gas = 0u64;
        let mut items = Vec::new();
        let mut raw = Vec::new();
        let mut hashes = Vec::new();
        for tx in self.pool.best_transactions() {
            let g = tx.transaction.gas_limit();
            if gas + g > self.cfg.budget_gas {
                break;
            }
            gas += g;
            let pooled: &ArcPooledTx = &tx.transaction;
            let signer = pooled.0.transaction.signer();
            let hash = *PoolTransaction::hash(pooled);
            match pooled.0.transaction.inner() {
                ArcTxEnvelope::Lean(lean) => {
                    raw.push(lean.raw().to_vec());
                    items.push((signer, lean.tx().clone(), hash));
                    hashes.push(hash);
                }
                ArcTxEnvelope::Eth(_) => unreachable!("lane admits 0x50 only"),
            }
        }
        (items, raw, hashes)
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
    async fn link_to_head(&self, block: &LeanBlock, tm: &AnchorTimer) -> eyre::Result<Link> {
        let (head_c, head_n, _) = *timed_lock(&self.head, &tm.lock_ns).await;
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
            let mut log = timed_lock(&self.log, &tm.lock_ns).await;
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
        tm: &AnchorTimer,
    ) -> eyre::Result<()> {
        tm.number.store(block.number, Ordering::Relaxed);
        let _gate = timed_lock(&self.commit_gate, &tm.lock_ns).await;
        if self.link_to_head(&block, tm).await? == Link::AlreadyCanonical {
            return Ok(());
        }
        let t_exec = Instant::now();
        let mut state = timed_lock(&self.state, &tm.lock_ns).await;
        let t_apply = Instant::now();
        let outcome = apply_block(&mut state, &items, block.number, self.cfg.beneficiary);
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
        add_ns(&tm.apply_ns, t_apply);
        let exec_ns = t_exec.elapsed().as_nanos() as u64;

        let t_append = Instant::now();
        let at = {
            let mut log = timed_lock(&self.log, &tm.lock_ns).await;
            log.append(&block)?
        };
        tm.append_ns.fetch_add(at.write_ns, Ordering::Relaxed);
        tm.fsync_ns.fetch_add(at.fsync_ns, Ordering::Relaxed);
        let t_index = Instant::now();
        self.by_commitment.lock().await.insert(block.commitment, block.number);
        // exact invalidation: only THIS commitment can have gone miss -> hit
        self.scan_misses.lock().await.remove(&block.commitment);
        add_ns(&tm.index_ns, t_index);
        if let Some(s) = digest_state {
            let t_snap = Instant::now();
            s.write_snapshot(&self.cfg.datadir, commitment, block.number, block.timestamp_ms)?;
            add_ns(&tm.append_ns, t_snap);
        }
        let append_ns = t_append.elapsed().as_nanos() as u64;
        self.stats.append_ns_last.store(append_ns, Ordering::Relaxed);

        *timed_lock(&self.head, &tm.lock_ns).await =
            (block.commitment, block.number, block.timestamp_ms);
        let t_pool = Instant::now();
        self.pool.remove_transactions(evict);
        // re-base sender nonces/balances -> promotes now-connectable queued
        // txs to pending, discards stale ones (reth's canonical-update analog)
        self.pool.update_accounts(changed);
        add_ns(&tm.pool_ns, t_pool);
        {
            let mut ring = timed_lock(&self.receipts, &tm.lock_ns).await;
            self.keep_receipts(&mut ring, outcome.receipts);
        }
        // head advanced: keep entries that are still AHEAD of the new head
        // (losing candidates for this height are dropped when the height is
        // passed; bytes for the current height stay servable by commitment)
        let new_head = *timed_lock(&self.head, &tm.lock_ns).await;
        let new_head_n = new_head.1;
        timed_lock(&self.staged, &tm.lock_ns).await.retain(|(_, e)| e.block.number >= new_head_n);
        timed_lock(&self.inflight, &tm.lock_ns).await.retain(|_, (p, _, _)| *p == new_head.0);
        let s = &self.stats;
        s.exec_ns.fetch_add(exec_ns, Ordering::Relaxed);
        s.append_ns.fetch_add(append_ns, Ordering::Relaxed);
        s.blocks.fetch_add(1, Ordering::Relaxed);
        s.txs.fetch_add(items.len() as u64, Ordering::Relaxed);
        s.noops.fetch_add(outcome.noops as u64, Ordering::Relaxed);
        s.outputs.fetch_add(outcome.outputs as u64, Ordering::Relaxed);
        Ok(())
    }

    /// One SELF-DRIVING tick (standalone mode): build on head and commit.
    /// In shim mode this is never called — only `shim_new` advances the chain.
    pub async fn produce_block(&self, timestamp_ms: u64) -> eyre::Result<Option<u64>> {
        let t_build = Instant::now();
        let (items, raw, hashes) = self.pick_txs().await;
        if items.is_empty() {
            return Ok(None);
        }
        let (parent, number, _) = *self.head.lock().await;
        let block = LeanBlock::new(parent, number + 1, timestamp_ms, raw);
        let build_ns = t_build.elapsed().as_nanos() as u64;
        self.stats.build_ns.fetch_add(build_ns, Ordering::Relaxed);
        self.stats.build_ns_last.store(build_ns, Ordering::Relaxed);
        let bn = block.number;
        let cm = block.commitment;
        self.commit_block(block, items, hashes, &AnchorTimer::default()).await?;
        self.announce_to_peers(cm, bn);
        Ok(Some(bn))
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
            receipts: outcome.receipts,
            evict,
            applied: outcome.applied,
            noops: outcome.noops,
            outputs: outcome.outputs,
            block,
        }
    }

    /// Publish a staged entry: replace any entry for the same commitment, keep
    /// the map bounded at 32. The ONLY writer of `self.staged` outside
    /// promotion's retention sweep.
    async fn insert_staged(&self, commitment: B256, entry: StagedBlock, lock_ns: &AtomicU64) {
        let mut q = timed_lock(&self.staged, lock_ns).await;
        q.retain(|(c, _)| *c != commitment);
        if q.len() >= 32 {
            q.pop_front();
        }
        q.push_back((commitment, entry));
    }

    /// Is `commitment` already staged against `head_c`? Used to make a second
    /// staging of the same bytes (our own `arc_buildBlock` result, then the
    /// CL's `arc_stageBlock` for it) a cheap no-op instead of a re-execution.
    async fn already_staged(&self, commitment: B256, head_c: B256, lock_ns: &AtomicU64) -> bool {
        timed_lock(&self.staged, lock_ns)
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
        let t_build = Instant::now();
        let (head_c, head_n, _) = *self.head.lock().await;
        if parent != head_c {
            return Err(format!("stale parent: build on {parent}, head is {head_c}"));
        }
        if number != head_n + 1 {
            return Err(format!("bad number {number}: head is {head_n} (want head+1)"));
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
                items.push((pooled.0.transaction.signer(), lean.tx().clone(), *lean.hash()));
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
        let lock_ns = AtomicU64::new(0);
        let on_head = {
            let head = timed_lock(&self.head, &lock_ns).await;
            if head.0 == head_c {
                Some(timed_lock(&self.state, &lock_ns).await.clone())
            } else {
                None
            }
        };
        if let Some(mut staged) = on_head {
            let outcome = apply_block(&mut staged, &items, number, self.cfg.beneficiary);
            if !self.already_staged(commitment, head_c, &lock_ns).await {
                let entry = self.staged_entry(block, wire.clone(), head_c, &items, &staged, outcome);
                self.insert_staged(commitment, entry, &lock_ns).await;
            }
        }
        let build_ns = t_build.elapsed().as_nanos() as u64;
        self.stats.build_ns.fetch_add(build_ns, Ordering::Relaxed);
        self.stats.build_ns_last.store(build_ns, Ordering::Relaxed);
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
    pub async fn release_claim(&self, commitment: B256, id: u64, lock_ns: &AtomicU64) -> bool {
        use std::collections::hash_map::Entry;
        let mut fl = timed_lock(&self.inflight, lock_ns).await;
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
        let lock_ns = AtomicU64::new(0);
        let t_decode = Instant::now();
        let hdr = LeanBlock::commitment_of_wire(bytes)?;
        let commitment_key = hdr.commitment;
        let number = hdr.number;

        // --- publish the claim before anything slow ---
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);
        let claim_id = next_claim_id();
        let claimed = {
            use std::collections::hash_map::Entry;
            let mut fl = timed_lock(&self.inflight, &lock_ns).await;
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

        let r = self.stage_claimed(bytes, hdr, t_decode, &lock_ns).await;

        // --- release it in EVERY outcome: Staged, Syncing, and Err ---
        // A stage that bails on the head check (or on a decode error) must not
        // park the next anchor for the full 2 s guard. A panic drops `done_tx`
        // instead, which closes the channel and wakes waiters the same way.
        // `release_claim` is id-checked: a stage slow enough to have been taken
        // over signals its waiters but leaves the new owner's claim alone.
        if let Some(id) = claimed {
            let _ = done_tx.send(true);
            self.release_claim(commitment_key, id, &lock_ns).await;
        }
        self.stage_notify.notify_waiters();

        let (outcome, decode_ns, recover_ns, apply_ns, noop) = r?;
        let stage_ns = t0.elapsed().as_nanos() as u64;
        // as before: only a stage that actually EXECUTED counts, so a no-op
        // (a few ms of wire hash) cannot make `stage_ms_last` read healthy.
        if matches!(outcome, StageOutcome::Staged(_)) && !noop {
            self.stats.exec_ns.fetch_add(stage_ns, Ordering::Relaxed);
            self.stats.stage_ns_last.store(stage_ns, Ordering::Relaxed);
        }
        if stats_enabled() && matches!(outcome, StageOutcome::Staged(_)) {
            println!(
                concat!(
                    "lean_stage n={} total_ms={:.2} decode_ms={:.2} ",
                    "recover_ms={:.2} apply_ms={:.2} lock_ms={:.2} noop={}",
                ),
                number,
                ms(stage_ns),
                ms(decode_ns),
                ms(recover_ns),
                ms(apply_ns),
                ms(lock_ns.load(Ordering::Relaxed)),
                u8::from(noop),
            );
        }
        Ok(outcome)
    }

    /// The body of `stage_block`, run with the commitment already claimed in
    /// `inflight`. Split out so that EVERY exit — including `?` on a decode
    /// error — passes through the caller's single release point.
    /// Returns (outcome, decode_ns, recover_ns, apply_ns, noop).
    async fn stage_claimed(
        &self,
        bytes: &[u8],
        hdr: crate::chain::WireHeader,
        t_decode: Instant,
        lock_ns: &AtomicU64,
    ) -> Result<(StageOutcome, u64, u64, u64, bool), String> {
        // PRELIMINARY head read: a cheap bail for a block that plainly does
        // not connect. NOT the one the entry is built on — see the atomic read
        // below.
        let (head_c0, head_n0, _) = *timed_lock(&self.head, lock_ns).await;
        if hdr.number != head_n0 + 1 || hdr.parent != head_c0 {
            return Ok((StageOutcome::Syncing { head: head_n0 }, 0, 0, 0, false));
        }
        // Already staged against this head — our own `arc_buildBlock` result,
        // or a duplicate `arc_stageBlock`. A cheap no-op: the entry that is
        // there is a post-state of the same parent over the same bytes, so
        // re-executing would only pay ecrecover + a state clone to rebuild it.
        if self.already_staged(hdr.commitment, head_c0, lock_ns).await {
            let decode_ns = t_decode.elapsed().as_nanos() as u64;
            return Ok((StageOutcome::Staged(hdr.commitment), decode_ns, 0, 0, true));
        }
        // the expensive half of the decode (tx bodies out of the wire). The
        // commitment is NOT recomputed: `hdr` is the header `commitment_of_wire`
        // produced over THESE bytes at the top of `stage_block` — the trust
        // boundary `from_wire_bytes_with_header` documents (it re-derives and
        // compares the three header fields always, the commitment in debug
        // builds, and takes `hdr.commitment` on faith in release).
        let block = LeanBlock::from_wire_bytes_with_header(bytes, hdr)?;
        let decode_ns = t_decode.elapsed().as_nanos() as u64;
        let t_recover = Instant::now();
        let items = decode_block_txs(&block.txs);
        let recover_ns = t_recover.elapsed().as_nanos() as u64;
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
            let head = timed_lock(&self.head, lock_ns).await;
            let (head_c, head_n, _) = *head;
            if block.number != head_n + 1 || block.parent != head_c {
                return Ok((
                    StageOutcome::Syncing { head: head_n },
                    decode_ns,
                    recover_ns,
                    0,
                    false,
                ));
            }
            (head_c, timed_lock(&self.state, lock_ns).await.clone())
        };
        let outcome = apply_block(&mut staged_state, &items, block.number, self.cfg.beneficiary);
        let commitment = block.commitment;
        let entry =
            self.staged_entry(block, bytes.to_vec(), head_c, &items, &staged_state, outcome);
        let apply_ns = t_apply.elapsed().as_nanos() as u64;
        // visible BEFORE the caller signals completion: a waiter woken by that
        // signal must find the entry it was waiting for.
        self.insert_staged(commitment, entry, lock_ns).await;
        Ok((StageOutcome::Staged(commitment), decode_ns, recover_ns, apply_ns, false))
    }

    /// Promote a staged block: append (fsync — the durability point), write
    /// the precomputed state delta, and do EXACTLY the bookkeeping of the full
    /// path — no re-validation, no re-execution.
    async fn promote_staged(&self, entry: StagedBlock, tm: &AnchorTimer) -> eyre::Result<()> {
        let StagedBlock {
            block,
            post_touched,
            changed,
            receipts,
            evict,
            applied: _,
            noops,
            outputs,
            ..
        } = entry;
        let n_items = receipts.len();
        tm.number.store(block.number, Ordering::Relaxed);
        // Same gate, same last-moment link check as `commit_block`: taking the
        // staged entry out of `staged` makes this promote unique among
        // promotes, but not among APPENDS — a by-bytes feed that arrives after
        // the entry is taken finds nothing staged, takes the free claim, and
        // commits the same block.
        let _gate = timed_lock(&self.commit_gate, &tm.lock_ns).await;
        if self.link_to_head(&block, tm).await? == Link::AlreadyCanonical {
            return Ok(());
        }
        let t_exec = Instant::now();
        let mut state = timed_lock(&self.state, &tm.lock_ns).await;
        let t_apply = Instant::now();
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
        add_ns(&tm.apply_ns, t_apply);
        let exec_ns = t_exec.elapsed().as_nanos() as u64;

        let t_append = Instant::now();
        let at: AppendTiming = {
            let mut log = timed_lock(&self.log, &tm.lock_ns).await;
            log.append(&block)?
        };
        tm.append_ns.fetch_add(at.write_ns, Ordering::Relaxed);
        tm.fsync_ns.fetch_add(at.fsync_ns, Ordering::Relaxed);
        let t_index = Instant::now();
        self.by_commitment.lock().await.insert(block.commitment, block.number);
        // exact invalidation: only THIS commitment can have gone miss -> hit
        self.scan_misses.lock().await.remove(&block.commitment);
        add_ns(&tm.index_ns, t_index);
        if let Some(s) = digest_state {
            let t_snap = Instant::now();
            s.write_snapshot(&self.cfg.datadir, commitment, block.number, block.timestamp_ms)?;
            add_ns(&tm.append_ns, t_snap);
        }
        let append_ns = t_append.elapsed().as_nanos() as u64;
        self.stats.append_ns_last.store(append_ns, Ordering::Relaxed);

        *timed_lock(&self.head, &tm.lock_ns).await =
            (block.commitment, block.number, block.timestamp_ms);
        let t_pool = Instant::now();
        self.pool.remove_transactions(evict);
        self.pool.update_accounts(changed);
        add_ns(&tm.pool_ns, t_pool);
        {
            let mut ring = timed_lock(&self.receipts, &tm.lock_ns).await;
            self.keep_receipts(&mut ring, receipts);
        }
        // head advanced: keep entries that are still AHEAD of the new head
        // (losing candidates for this height are dropped when the height is
        // passed; bytes for the current height stay servable by commitment)
        let new_head = *timed_lock(&self.head, &tm.lock_ns).await;
        let new_head_n = new_head.1;
        timed_lock(&self.staged, &tm.lock_ns).await.retain(|(_, e)| e.block.number >= new_head_n);
        timed_lock(&self.inflight, &tm.lock_ns).await.retain(|_, (p, _, _)| *p == new_head.0);
        let s = &self.stats;
        s.exec_ns.fetch_add(exec_ns, Ordering::Relaxed);
        s.append_ns.fetch_add(append_ns, Ordering::Relaxed);
        s.blocks.fetch_add(1, Ordering::Relaxed);
        s.txs.fetch_add(n_items as u64, Ordering::Relaxed);
        s.noops.fetch_add(noops as u64, Ordering::Relaxed);
        s.outputs.fetch_add(outputs as u64, Ordering::Relaxed);
        s.promoted.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Shim: `arc_newBlock` — decode strictly, recompute the commitment,
    /// verify the parent link, total-STF execute, append+fsync, evict.
    /// IDEMPOTENT by commitment: re-feeding the head or any ancestor returns
    /// its commitment as a no-op; a DIFFERENT block at a known height is a
    /// loud error (fork attempt); head+2 gaps are errors (feed in order).
    pub async fn shim_new(&self, bytes: &[u8]) -> Result<NewBlockOutcome, String> {
        self.shim_new_at(bytes, Instant::now()).await
    }

    /// `shim_new` with the RPC handler's ENTRY instant, so `total_ms` covers
    /// the whole call and `pre_ms` isolates everything before dispatch (param
    /// parse, base64 decode of the body, tokio scheduling). The plain
    /// `shim_new` is the harness/test entry point: it starts the clock here.
    pub async fn shim_new_at(
        &self,
        bytes: &[u8],
        t0: Instant,
    ) -> Result<NewBlockOutcome, String> {
        let tm = AnchorTimer::default();
        add_ns(&tm.pre_ns, t0);
        tm.set_path(PATH_BYTES);
        let r = self.shim_new_inner(bytes, &tm).await;
        if matches!(r, Ok(NewBlockOutcome::Syncing { .. })) {
            tm.set_path(PATH_SYNCING);
        }
        tm.emit(t0);
        r
    }

    /// `shim_new` without the timing line: the by-commitment entry point owns
    /// the timer so exactly ONE `lean_anchor` line is printed per RPC call.
    async fn shim_new_inner(
        &self,
        bytes: &[u8],
        tm: &AnchorTimer,
    ) -> Result<NewBlockOutcome, String> {
        let t_decode = Instant::now();
        let block = LeanBlock::from_wire_bytes(bytes)?;
        add_ns(&tm.decode_ns, t_decode);
        tm.number.store(block.number, Ordering::Relaxed);
        let (head_c, head_n, _) = *timed_lock(&self.head, &tm.lock_ns).await;
        if block.number <= head_n {
            // known-height path: compare against our chain
            if block.number == 0 {
                return Err("block number 0 is the genesis sentinel".into());
            }
            let t_index = Instant::now();
            let known = self
                .log
                .lock()
                .await
                .read_block(block.number)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("height {} predates the log", block.number))?;
            add_ns(&tm.index_ns, t_index);
            if known.commitment == block.commitment {
                tm.set_path(PATH_INDEX);
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
                let mut q = timed_lock(&self.sync_queue, &tm.lock_ns).await;
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
            let mut q = timed_lock(&self.staged, &tm.lock_ns).await;
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
                    let mut fl = timed_lock(&self.inflight, &tm.lock_ns).await;
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
                let t_wait = Instant::now();
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while !*rx.borrow() {
                        if rx.changed().await.is_err() {
                            break; // owner dropped (failed/panicked): fall through
                        }
                    }
                })
                .await;
                add_ns(&tm.wait_inflight_ns, t_wait);
                // a STAGING finished: promote its entry (the v1.3.1 case)
                staged = take_staged().await;
                if staged.is_some() {
                    break;
                }
                // a COMMIT finished: the block is ours already, anchoring it is
                // a no-op. Answer VALID instead of appending it a second time.
                let (head_c2, head_n2, _) = *timed_lock(&self.head, &tm.lock_ns).await;
                if head_c2 == commitment
                    || self.by_commitment.lock().await.get(&commitment) == Some(&number)
                {
                    tm.refine_path(PATH_INDEX);
                    return Ok(NewBlockOutcome::Valid(commitment));
                }
                if number != head_n2 + 1 || block.parent != head_c2 {
                    // head moved elsewhere while we waited: this no longer
                    // connects, so queue it rather than commit off-chain
                    {
                        let mut q = timed_lock(&self.sync_queue, &tm.lock_ns).await;
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
                tm.refine_path(PATH_STAGED);
                if let Some((id, done_tx)) = owner.take() {
                    // never happens (a claim is only taken when nothing is
                    // staged), but leaving a claim behind would park the next
                    // feed of these bytes for the full 2 s guard
                    let _ = done_tx.send(true);
                    self.release_claim(commitment, id, &tm.lock_ns).await;
                }
                self.promote_staged(entry, tm).await.map_err(|e| e.to_string())?;
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
                let t_recover = Instant::now();
                let items = decode_block_txs(&block.txs);
                add_ns(&tm.decode_ns, t_recover);
                let evict = items.iter().map(|(_, _, h)| *h).collect();
                let r = self.commit_block(block, items, evict, tm).await;
                // release the claim in BOTH outcomes: `commit_block`'s own
                // retention sweep already drops the entry (its parent is the
                // old head), but a failed commit never reaches that sweep.
                let _ = done_tx.send(true);
                self.release_claim(commitment, my_id, &tm.lock_ns).await;
                r.map_err(|e| e.to_string())?;
            }
        }
        let t_ann = Instant::now();
        self.announce_to_peers(commitment, number);
        add_ns(&tm.announce_ns, t_ann);
        // Drain any queued children that now connect (loop: each apply may
        // unlock the next).
        let mut parent = commitment;
        loop {
            let next = timed_lock(&self.sync_queue, &tm.lock_ns).await.remove(&parent);
            let Some(nb) = next else { break };
            let t_recover = Instant::now();
            let items = decode_block_txs(&nb.txs);
            add_ns(&tm.decode_ns, t_recover);
            let evict = items.iter().map(|(_, _, h)| *h).collect();
            let c = nb.commitment;
            let num = nb.number;
            // Cheap early-out: the head may have moved past this child via
            // backfill while we committed its parent. This check is advisory
            // only — the drain holds NO claim, so an anchor or a backfill for
            // the same block can take the (free) claim in the window between
            // here and the append. `commit_block` re-takes the check under
            // `commit_gate` and answers that race with a no-op.
            let (hc, hn, _) = *timed_lock(&self.head, &tm.lock_ns).await;
            if nb.number != hn + 1 || nb.parent != hc {
                break;
            }
            self.commit_block(nb, items, evict, tm).await.map_err(|e| e.to_string())?;
            let t_ann = Instant::now();
            self.announce_to_peers(c, num);
            add_ns(&tm.announce_ns, t_ann);
            parent = c;
        }
        Ok(NewBlockOutcome::Valid(commitment))
    }

    /// Shim: `arc_newBlock{commitment}` — the CL only knows the commitment
    /// (it is in the EVM header). Idempotent for canonical blocks; otherwise
    /// apply from our staged/queued copy, else fetch the bytes from a peer by
    /// commitment, else SYNCING.
    pub async fn shim_new_by_commitment(&self, c: B256) -> Result<NewBlockOutcome, String> {
        self.shim_new_by_commitment_at(c, Instant::now()).await
    }

    /// As above, with the RPC handler's ENTRY instant (see `shim_new_at`).
    pub async fn shim_new_by_commitment_at(
        &self,
        c: B256,
        t0: Instant,
    ) -> Result<NewBlockOutcome, String> {
        let tm = AnchorTimer::default();
        add_ns(&tm.pre_ns, t0);
        let r = self.new_by_commitment_inner(c, &tm).await;
        if matches!(r, Ok(NewBlockOutcome::Syncing { .. })) {
            tm.set_path(PATH_SYNCING);
        }
        tm.emit(t0);
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
    ///    then. Wait on `stage_notify` up to `LEAN_STAGE_RACE_MS`, rechecking
    ///    `inflight` each round. This window is real: the CL issues the anchor
    ///    ~15 ms after the stage, and the stage's pre-registration work is the
    ///    same order of magnitude.
    ///
    /// The grace in (2) is gated on `stage_pending > 0`, so a node doing a
    /// genuine backfill — nobody staging — adds zero latency to its peer
    /// pulls. Worst case when a stage is pending but is for a DIFFERENT block:
    /// `LEAN_STAGE_RACE_MS` on top of a path that already costs 200-250 ms.
    async fn await_stage_race(&self, c: B256, tm: &AnchorTimer) -> bool {
        let grace = std::time::Duration::from_millis(stage_race_ms());
        let t_wait = Instant::now();
        let mut waited = false;
        loop {
            let held = {
                let fl = timed_lock(&self.inflight, &tm.lock_ns).await;
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
            add_ns(&tm.wait_inflight_ns, t_wait);
            self.stats.stage_race_waits.fetch_add(1, Ordering::Relaxed);
        }
        waited
    }

    async fn new_by_commitment_inner(
        &self,
        c: B256,
        tm: &AnchorTimer,
    ) -> Result<NewBlockOutcome, String> {
        // Idempotency check against the in-process index ONLY. The block being
        // decided is by definition not canonical yet, so a miss here is the
        // normal case; running the bounded backward log scan for it cost one
        // full-chain read+hash per decide (measured: +1.1 ms per block of
        // history, 331 ms at height 300) and collapsed cadence. A canonical
        // block older than the index still anchors correctly through the
        // bytes path below (known-height comparison in shim_new).
        let t_index = Instant::now();
        let known = self.by_commitment.lock().await.get(&c).copied();
        add_ns(&tm.index_ns, t_index);
        if let Some(n) = known {
            // the index knows the height; report it rather than n=0
            tm.number.store(n, Ordering::Relaxed);
            tm.set_path(PATH_INDEX);
            return Ok(NewBlockOutcome::Valid(c));
        }
        // A staging or a by-bytes COMMIT of exactly this block may be in
        // flight (our CL's own `arc_stageBlock`, fired ~15 ms ago and still
        // running; or the gossip backfill racing this anchor for the height).
        // Wait for that result instead of queueing our own `state` and `log`
        // locks behind it: if it was a commit, the block is canonical and this
        // anchor is an idempotent VALID; if it was a staging, `locate_wire`
        // below finds the staged entry instead of falling through to a peer
        // fetch.
        if self.await_stage_race(c, tm).await {
            let known = { self.by_commitment.lock().await.get(&c).copied() };
            if let Some(n) = known {
                tm.number.store(n, Ordering::Relaxed);
                tm.set_path(PATH_INDEX);
                return Ok(NewBlockOutcome::Valid(c));
            }
        }
        let t_locate = Instant::now();
        let located = self.locate_wire(c, "new_block").await?;
        add_ns(&tm.locate_ns, t_locate);
        if let Some((bytes, src)) = located {
            tm.set_path(match src {
                "staged" => PATH_STAGED,
                "queued" => PATH_QUEUED,
                _ => PATH_INDEX,
            });
            return self.shim_new_inner(&bytes, tm).await;
        }
        // LAST CHANCE before spending 200-250 ms on a peer: a stage may have
        // registered while we ran `locate_wire`. Same bounded wait, then one
        // more locate — still cheaper than the round trip it replaces.
        if self.await_stage_race(c, tm).await {
            let known = { self.by_commitment.lock().await.get(&c).copied() };
            if let Some(n) = known {
                tm.number.store(n, Ordering::Relaxed);
                tm.set_path(PATH_INDEX);
                return Ok(NewBlockOutcome::Valid(c));
            }
            let t_locate = Instant::now();
            let located = self.locate_wire(c, "new_block").await?;
            add_ns(&tm.locate_ns, t_locate);
            if let Some((bytes, src)) = located {
                tm.set_path(match src {
                    "staged" => PATH_STAGED,
                    "queued" => PATH_QUEUED,
                    _ => PATH_INDEX,
                });
                return self.shim_new_inner(&bytes, tm).await;
            }
        }
        let t_peer = Instant::now();
        for cl in &self.peer_clients {
            use jsonrpsee::core::client::ClientT;
            let mut params = jsonrpsee::core::params::ObjectParams::new();
            let _ = params.insert("commitment", format!("{c}"));
            let r: Result<serde_json::Value, _> = cl.request("arc_getBlockBytes", params).await;
            let Ok(v) = r else { continue };
            let Some(b64) = v.get("blockBytes").and_then(|x| x.as_str()) else { continue };
            let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
            else {
                continue;
            };
            match LeanBlock::from_wire_bytes(&bytes) {
                Ok(b) if b.commitment == c => {
                    self.stats.block_pulls.fetch_add(1, Ordering::Relaxed);
                    add_ns(&tm.peer_ns, t_peer);
                    tm.set_path(PATH_PEER);
                    return self.shim_new_inner(&bytes, tm).await;
                }
                _ => continue, // peer served the wrong block: ignore it
            }
        }
        // every peer missed or failed: that whole sweep is still peer time
        add_ns(&tm.peer_ns, t_peer);
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
                let _ = c.request::<serde_json::Value, _>("arc_announceBlock", params).await;
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
    /// No log read, no commitment resolve: the `announce_never_scans_the_log`
    /// invariant holds.
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
    /// `LEAN_ANNOUNCE_GRACE_MS` for that anchor, then arm the backfill if it
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
    /// INVARIANT (tested: `announce_never_scans_the_log`): this path never
    /// resolves a commitment against the LOG, so it can never reach the
    /// backward log scan. It decides on numbers plus two in-memory hash
    /// lookups — a block above our head cannot be in our log, and one at or
    /// below it is never pulled backwards.
    pub async fn on_announce(&self, commitment: B256, number: u64) {
        self.stats.announces_rx.fetch_add(1, Ordering::Relaxed);
        let head_n = self.head.lock().await.1;
        if number <= head_n {
            // known height: our chain already has a block there (commitment
            // equality is irrelevant to traffic — never pull backwards)
            self.stats.announces_known.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.holds_announced(commitment, number, head_n).await {
            self.stats.announce_skipped_staged.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if number == head_n + 1 {
            // One height behind and nothing staged for it. Either our CL is
            // about to anchor it (the common case: the announcement simply
            // beat our own `arc_stageBlock`/`arc_newBlock`) or our CL is gone.
            // Give the anchor the grace window before spending a pull.
            let grace = announce_grace_ms();
            if grace > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(grace)).await;
                let head_n = self.head.lock().await.1;
                if number <= head_n {
                    self.stats.announces_known.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                if self.holds_announced(commitment, number, head_n).await {
                    self.stats.announce_skipped_staged.fetch_add(1, Ordering::Relaxed);
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
                                    node.stats.block_pulls.fetch_add(1, Ordering::Relaxed);
                                    node.stats.backfill_pulls.fetch_add(1, Ordering::Relaxed);
                                    if matches!(
                                        node.shim_new(&bytes).await,
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
        let block = { self.log.lock().await.read_block(number).map_err(|e| e.to_string())? };
        Ok(block.map(|b| b.to_wire_bytes()))
    }

    /// Resolve a commitment to block bytes: staged, then the sync queue, then
    /// the in-process index, then — only where the index is provably
    /// incomplete — a bounded backward scan of the log. Returns the bytes and
    /// WHERE they came from (the `hit=` field of `lean_locate`).
    ///
    /// The scan is the expensive branch: it reads and keccaks up to
    /// `SCAN_LIMIT` blocks (1.24 MB each on the fleet), so it is now capped at
    /// `index_floor - 1`. `by_commitment` is complete from `index_floor`
    /// upward, so above that line a miss is proof of absence and scanning
    /// cannot find anything the index did not already have — which is exactly
    /// the case a peer's `arc_getBlockBytes{commitment}` for a block we have
    /// not appended hits, on every ask, at a cost that grows with height.
    /// Below `index_floor` (blocks older than the snapshot this process opened
    /// on) the scan still runs, unchanged.
    ///
    /// A scan that finds nothing is remembered in `scan_misses` (exact
    /// invalidation: only the appended commitment is removed) so a repeated
    /// unknown commitment costs one hash check.
    async fn locate_inner(
        &self,
        c: B256,
        scanned: &mut u64,
    ) -> Result<Option<(Vec<u8>, &'static str)>, String> {
        // Staged and queued copies are exactly what the decide anchor asks for;
        // check them before anything that can touch the log.
        if let Some((_, e)) = self.staged.lock().await.iter().find(|(k, _)| *k == c) {
            return Ok(Some((e.wire.clone(), "staged")));
        }
        if let Some(b) = self.sync_queue.lock().await.values().find(|b| b.commitment == c) {
            return Ok(Some((b.to_wire_bytes(), "queued")));
        }
        // Drop the index guard BEFORE reading the log: `block_wire_bytes`
        // awaits the log lock, and holding the commitment index across that
        // await serialises every other lookup behind one disk read.
        let hit = { self.by_commitment.lock().await.get(&c).copied() };
        if let Some(n) = hit {
            return Ok(self.block_wire_bytes(n).await?.map(|w| (w, "index")));
        }
        if self.scan_misses.lock().await.contains(&c) {
            return Ok(None);
        }
        const SCAN_LIMIT: u64 = 1024;
        let head_n = self.head.lock().await.1;
        let floor = head_n.saturating_sub(SCAN_LIMIT).max(1);
        // never re-walk what the index already covers completely
        let ceil = self.index_floor.load(Ordering::Relaxed).saturating_sub(1).min(head_n);
        if ceil >= floor {
            self.stats.log_scans.fetch_add(1, Ordering::Relaxed);
            for n in (floor..=ceil).rev() {
                *scanned += 1;
                let Some(b) = self.log.lock().await.read_block(n).map_err(|e| e.to_string())?
                else {
                    continue;
                };
                self.by_commitment.lock().await.insert(b.commitment, n);
                if b.commitment == c {
                    return Ok(Some((b.to_wire_bytes(), "scan")));
                }
            }
        }
        let mut misses = self.scan_misses.lock().await;
        if misses.len() >= 4096 {
            misses.clear();
        }
        misses.insert(c);
        Ok(None)
    }

    /// `locate_inner` + one `lean_locate` line per call under `LEAN_STATS=1`.
    /// `caller` is the reason we are looking: `new_block` (the decide anchor),
    /// `get_bytes` (a peer or the CL's value-sync serving path), `announce`,
    /// `other` (tests/tools).
    async fn locate_wire(
        &self,
        c: B256,
        caller: &'static str,
    ) -> Result<Option<(Vec<u8>, &'static str)>, String> {
        let t0 = Instant::now();
        let mut scanned = 0u64;
        let r = self.locate_inner(c, &mut scanned).await;
        if stats_enabled() {
            let hit = match &r {
                Ok(Some((_, h))) => *h,
                Ok(None) => "miss",
                Err(_) => "error",
            };
            println!(
                "lean_locate caller={} hit={} scanned={} ms={:.2}",
                caller,
                hit,
                scanned,
                ms(t0.elapsed().as_nanos() as u64),
            );
        }
        r
    }

    /// Shim: `arc_getBlockBytes{commitment}` — canonical, staged or queued
    /// block with that commitment.
    pub async fn block_wire_bytes_by_commitment(
        &self,
        c: B256,
    ) -> Result<Option<Vec<u8>>, String> {
        self.block_wire_bytes_by_commitment_for(c, "other").await
    }

    /// As above, labelling the caller in the `lean_locate` line.
    pub async fn block_wire_bytes_by_commitment_for(
        &self,
        c: B256,
        caller: &'static str,
    ) -> Result<Option<Vec<u8>>, String> {
        Ok(self.locate_wire(c, caller).await?.map(|(w, _)| w))
    }

    pub async fn receipt(&self, hash: B256) -> Option<LeanReceipt> {
        self.receipts.lock().await.iter().rev().find(|r| r.tx_hash == hash).cloned()
    }

    /// One-line periodic snapshot of everything that can grow without bound,
    /// plus the last block's service times. Every field is a counter or a
    /// `len()` — nothing here walks the state, the log or the pool, so it is
    /// safe to call on a hot node. Emitted by `spawn_stats` under
    /// `LEAN_STATS=1`; `grep lean_stats` in a node log gives the whole
    /// growth-vs-time picture that a cadence drift needs.
    pub async fn stats_line(&self) -> String {
        let head = self.head.lock().await.1;
        let accounts = self.state.lock().await.accounts.len();
        let staged = self.staged.lock().await.len();
        let sync_queue = self.sync_queue.lock().await.len();
        let by_commitment = self.by_commitment.lock().await.len();
        let scan_misses = self.scan_misses.lock().await.len();
        let pending = self.pool.pool_size().pending;
        let s = &self.stats;
        let ms = |ns: u64| ns as f64 / 1_000_000.0;
        format!(
            // concat!, never a `\`-continued literal: a continuation keeps the
            // next line's indentation and pads the log line with spaces.
            // concat! cannot see captured variables, so every arg is positional.
            concat!(
                "lean_stats head={} accounts={} staged={} ",
                "sync_queue={} by_commitment={} scan_misses={} log_scans={} ",
                "announce_skipped_staged={} backfill_pulls={} ",
                "stage_race_waits={} ",
                "pool={} build_ms_last={:.1} stage_ms_last={:.1} ",
                "append_ms_last={:.1} rss_mb={}",
            ),
            head,
            accounts,
            staged,
            sync_queue,
            by_commitment,
            scan_misses,
            s.log_scans.load(Ordering::Relaxed),
            s.announce_skipped_staged.load(Ordering::Relaxed),
            s.backfill_pulls.load(Ordering::Relaxed),
            s.stage_race_waits.load(Ordering::Relaxed),
            pending,
            ms(s.build_ns_last.load(Ordering::Relaxed)),
            ms(s.stage_ns_last.load(Ordering::Relaxed)),
            ms(s.append_ns_last.load(Ordering::Relaxed)),
            rss_mb(),
        )
    }

    /// `LEAN_STATS=1` -> print `stats_line()` every 10 s. Off by default (a
    /// fleet node's log stays exactly as it is unless the flag is set). The
    /// binary has no tracing subscriber — stdout IS its INFO channel, as for
    /// every other line it prints.
    pub fn spawn_stats(self: &std::sync::Arc<Self>) {
        if !stats_enabled() {
            return;
        }
        let node = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                tick.tick().await;
                println!("{}", node.stats_line().await);
            }
        });
    }
}

/// Resident set size in MiB, from `/proc/self/statm` field 2 (resident pages).
/// 0 on any platform or permission that does not give us the file — a stats
/// line must never be the thing that fails.
pub fn rss_mb() -> u64 {
    const PAGE: u64 = 4096;
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or(0, |pages| pages * PAGE / (1024 * 1024))
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
pub fn decode_block_txs(txs: &[Vec<u8>]) -> Vec<(Address, LeanTx, B256)> {
    if parallel_recovery_enabled() {
        decode_block_txs_parallel(txs)
    } else {
        decode_block_txs_serial(txs)
    }
}

/// Rayon-parallel sender recovery, DEFAULT ON since 2026-08-26 (roadmap §8
/// adoption rule satisfied: V3 differential + V4 replay invariance + V5 mixed
/// convergence + V7 fleet mechanism run with zero burned rounds + 2 h mixed
/// soak with zero divergence). Measured: 4.1x payments/s at a signature-heavy
/// mix — serial recovery on the runtime threads was also starving admission.
/// `LEAN_PARALLEL_RECOVERY=0` is the rollback for one release.
fn parallel_recovery_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("LEAN_PARALLEL_RECOVERY").as_deref() != Ok("0"))
}

pub fn decode_block_txs_serial(txs: &[Vec<u8>]) -> Vec<(Address, LeanTx, B256)> {
    let mut items = Vec::with_capacity(txs.len());
    for bytes in txs {
        let Ok(env) = ArcTxEnvelope::decode_2718(&mut bytes.as_slice()) else { continue };
        let Ok(signer) = env.recover_signer() else { continue };
        if let ArcTxEnvelope::Lean(lean) = env {
            let hash = *lean.hash();
            items.push((signer, lean.tx().clone(), hash));
        }
    }
    items
}

/// Identical semantics to the serial path — same skips, same order (rayon's
/// indexed par_iter + collect preserves input order; skipped entries collapse
/// via flatten) — only the per-tx decode + ecrecover run on the thread pool.
pub fn decode_block_txs_parallel(txs: &[Vec<u8>]) -> Vec<(Address, LeanTx, B256)> {
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
