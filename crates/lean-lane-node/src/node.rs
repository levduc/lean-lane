//! The lane node composition: reth's pool as a LIBRARY (validator + ordering +
//! eviction, unmodified — exactly the 2a wiring) + flat-state total STF +
//! append-only log + a build-execute-commit driver loop.
//!
//! The driver is BOTH the offline harness and the future proposer path; the CL
//! shim splits it into `buildBlock(budget)` / `newBlock(bytes)` / `getHead`
//! (contract listed in LEAN-NATIVE.md; not implemented here).

use crate::chain::{genesis_commitment, BlockLog, LeanBlock};
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
    pub budget_gas: u64,
    pub snapshot_every: u64,
    pub beneficiary: Address,
    pub receipt_ring: usize,
    /// Other lean nodes' RPC urls (backfill source + push-on-append targets).
    pub peers: Vec<String>,
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
}

pub struct LaneNode {
    pub cfg: Config,
    pub pool: LanePool,
    pub provider: LeanProvider,
    pub state: Mutex<FlatState>,
    pub log: Mutex<BlockLog>,
    /// (head commitment, head number, head timestamp_ms)
    pub head: Mutex<(B256, u64, u64)>,
    pub receipts: Mutex<VecDeque<LeanReceipt>>,
    pub stats: Stats,
    /// Blocks that arrived ahead of our head, keyed by PARENT commitment
    /// (bounded; drained as soon as their parent lands).
    pub sync_queue: Mutex<std::collections::HashMap<B256, crate::chain::LeanBlock>>,
    /// Wakes the backfill task when a SYNCING entry is queued.
    pub sync_notify: tokio::sync::Notify,
    /// Highest announced block number (backfill target when nothing is queued).
    pub sync_target: AtomicU64,
    /// Vote-gap staged blocks keyed by commitment (bounded; all entries are
    /// invalidated whenever the head advances).
    pub staged: Mutex<std::collections::VecDeque<(B256, StagedBlock)>>,
    /// IN-FLIGHT stagings: commitment -> (staged_parent, completion signal).
    /// arc_newBlock awaits a matching in-flight staging (bounded guard)
    /// instead of redoing the full execution — a <=35ms wait beats a ~230ms
    /// redo (v1.3 lost ~half the anchor races at 2.6MB blocks).
    pub inflight: Mutex<std::collections::HashMap<B256, (B256, tokio::sync::watch::Receiver<bool>)>>,
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

impl LaneNode {
    /// Open (or create) the lane at `cfg.datadir`: newest snapshot + log-tail
    /// replay (each replayed tx pays one decode + one ecrecover — the honest
    /// recovery cost). Returns the node with the pool's validator seeded from
    /// the recovered state.
    pub fn open(cfg: Config) -> eyre::Result<(Self, RecoveryReport)> {
        set_lane_chain_id(cfg.chain_id);
        std::fs::create_dir_all(&cfg.datadir)?;
        let t0 = Instant::now();
        let (mut state, snap_commitment, snap_number, snap_ts) =
            match FlatState::load_newest_snapshot(&cfg.datadir)? {
                Some((s, c, n, t)) => (s, c, n, t),
                None => (FlatState::default(), genesis_commitment(cfg.chain_id), 0, 0),
            };
        let mut log = BlockLog::open(&cfg.datadir.join("blocks.log"))?;
        let mut head = (snap_commitment, snap_number, snap_ts);
        let beneficiary = cfg.beneficiary;
        let mut replayed_blocks = 0u64;
        let mut replayed_txs = 0u64;
        log.replay(snap_number, |block| {
            let items = decode_block_txs(&block.txs);
            replayed_txs += items.len() as u64;
            apply_block(&mut state, &items, block.number, beneficiary);
            head = (block.commitment, block.number, block.timestamp_ms);
            replayed_blocks += 1;
        })?;
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
                receipts: Mutex::new(VecDeque::new()),
                stats: Stats::default(),
                sync_queue: Mutex::new(Default::default()),
                sync_notify: tokio::sync::Notify::new(),
                sync_target: AtomicU64::new(0),
                staged: Mutex::new(Default::default()),
                inflight: Mutex::new(Default::default()),
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

    /// Execute + append + commit bookkeeping for a block whose parent-link has
    /// already been verified against head. The ONLY way the chain advances.
    async fn commit_block(
        &self,
        block: LeanBlock,
        items: Vec<(Address, LeanTx, B256)>,
        evict: Vec<B256>,
    ) -> eyre::Result<()> {
        let t_exec = Instant::now();
        let mut state = self.state.lock().await;
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
        let snapshot_due = block.number % self.cfg.snapshot_every == 0;
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
        let exec_ns = t_exec.elapsed().as_nanos() as u64;

        let t_append = Instant::now();
        self.log.lock().await.append(&block)?;
        if let Some(s) = digest_state {
            s.write_snapshot(&self.cfg.datadir, commitment, block.number, block.timestamp_ms)?;
        }
        let append_ns = t_append.elapsed().as_nanos() as u64;

        *self.head.lock().await = (block.commitment, block.number, block.timestamp_ms);
        self.pool.remove_transactions(evict);
        // re-base sender nonces/balances -> promotes now-connectable queued
        // txs to pending, discards stale ones (reth's canonical-update analog)
        self.pool.update_accounts(changed);
        {
            let mut ring = self.receipts.lock().await;
            for r in outcome.receipts {
                if ring.len() == self.cfg.receipt_ring {
                    ring.pop_front();
                }
                ring.push_back(r);
            }
        }
        // head advanced: every staged entry was built against the old head
        self.staged.lock().await.clear();
        let new_head = *self.head.lock().await;
        self.inflight.lock().await.retain(|_, (p, _)| *p == new_head.0);
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
        self.stats.build_ns.fetch_add(t_build.elapsed().as_nanos() as u64, Ordering::Relaxed);
        let bn = block.number;
        let cm = block.commitment;
        self.commit_block(block, items, hashes).await?;
        self.announce_to_peers(cm, bn);
        Ok(Some(bn))
    }

    /// Shim: `arc_buildBlock` — build a candidate on the CURRENT head against a
    /// STAGED state copy. Never appends, never evicts, never mutates committed
    /// state. Contract: parent must be the head commitment; number must be
    /// head+1 (a consistent CL can send nothing else — checked loudly).
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
        // staged execution: full state copy (I1 simplicity; CoW is a later
        // optimization) — proves the txs apply, commitment derives from content
        let mut staged = self.state.lock().await.clone();
        apply_block(&mut staged, &items, number, self.cfg.beneficiary);
        self.stats.build_ns.fetch_add(t_build.elapsed().as_nanos() as u64, Ordering::Relaxed);
        Ok((block.commitment, block.to_wire_bytes()))
    }

    /// Shim: `arc_stageBlock` — fully validate + EXECUTE against the current
    /// head state into a memory-only staged entry (vote-gap deferred anchor).
    /// Never appends, never queues (speculative). Bounded to 8 entries.
    pub async fn stage_block(&self, bytes: &[u8]) -> Result<StageOutcome, String> {
        let block = LeanBlock::from_wire_bytes(bytes)?;
        let (head_c, head_n, _) = *self.head.lock().await;
        if block.number != head_n + 1 || block.parent != head_c {
            return Ok(StageOutcome::Syncing { head: head_n });
        }
        let t0 = Instant::now();
        let commitment_key = block.commitment;
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);
        self.inflight.lock().await.insert(commitment_key, (head_c, done_rx));
        let items = decode_block_txs(&block.txs);
        // execute against a CLONE of the head state
        let mut staged_state = self.state.lock().await.clone();
        let outcome = apply_block(&mut staged_state, &items, block.number, self.cfg.beneficiary);
        let mut touched: Vec<Address> = items.iter().map(|(s, _, _)| *s).collect();
        for (_, tx, _) in &items {
            touched.extend(tx.outputs.iter().map(|o| o.to));
        }
        touched.push(self.cfg.beneficiary);
        touched.sort_unstable();
        touched.dedup();
        let post_touched: Vec<(Address, crate::state::Acct)> =
            touched.iter().map(|a| (*a, staged_state.get(a))).collect();
        let changed = post_touched
            .iter()
            .map(|(a, acct)| reth_execution_types::ChangedAccount {
                address: *a,
                nonce: acct.nonce,
                balance: U256::from(acct.balance),
            })
            .collect();
        let evict = items.iter().map(|(_, _, h)| *h).collect();
        let commitment = block.commitment;
        let entry = StagedBlock {
            staged_parent: head_c,
            wire: bytes.to_vec(),
            post_touched,
            changed,
            receipts: outcome.receipts,
            evict,
            applied: outcome.applied,
            noops: outcome.noops,
            outputs: outcome.outputs,
            block,
        };
        {
            let mut q = self.staged.lock().await;
            q.retain(|(c, _)| *c != commitment);
            if q.len() >= 8 {
                q.pop_front();
            }
            q.push_back((commitment, entry));
        }
        // signal AFTER the staged entry is visible, then deregister
        let _ = done_tx.send(true);
        self.inflight.lock().await.remove(&commitment_key);
        self.stats.exec_ns.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        Ok(StageOutcome::Staged(commitment))
    }

    /// Promote a staged block: append (fsync — the durability point), write
    /// the precomputed state delta, and do EXACTLY the bookkeeping of the full
    /// path — no re-validation, no re-execution.
    async fn promote_staged(&self, entry: StagedBlock) -> eyre::Result<()> {
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
        let t_exec = Instant::now();
        let mut state = self.state.lock().await;
        for (a, acct) in &post_touched {
            state.accounts.insert(*a, *acct);
        }
        for (a, acct) in &post_touched {
            self.provider.upsert(*a, *acct);
        }
        self.provider.set_head(block.number);
        let snapshot_due = block.number % self.cfg.snapshot_every == 0;
        let (digest_state, commitment) = (snapshot_due.then(|| state.clone()), block.commitment);
        drop(state);
        let exec_ns = t_exec.elapsed().as_nanos() as u64;

        let t_append = Instant::now();
        self.log.lock().await.append(&block)?;
        if let Some(s) = digest_state {
            s.write_snapshot(&self.cfg.datadir, commitment, block.number, block.timestamp_ms)?;
        }
        let append_ns = t_append.elapsed().as_nanos() as u64;

        *self.head.lock().await = (block.commitment, block.number, block.timestamp_ms);
        self.pool.remove_transactions(evict);
        self.pool.update_accounts(changed);
        {
            let mut ring = self.receipts.lock().await;
            for r in receipts {
                if ring.len() == self.cfg.receipt_ring {
                    ring.pop_front();
                }
                ring.push_back(r);
            }
        }
        self.staged.lock().await.clear();
        let new_head = *self.head.lock().await;
        self.inflight.lock().await.retain(|_, (p, _)| *p == new_head.0);
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
        if staged.is_none() {
            // v1.3.1: an IN-FLIGHT staging for this commitment on this head?
            // Await its completion (bounded guard) instead of re-executing.
            let waiter = self
                .inflight
                .lock()
                .await
                .get(&commitment)
                .filter(|(p, _)| *p == head_c)
                .map(|(_, rx)| rx.clone());
            if let Some(mut rx) = waiter {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while !*rx.borrow() {
                        if rx.changed().await.is_err() {
                            break; // stager dropped (failed/panicked): fall through
                        }
                    }
                })
                .await;
                staged = take_staged().await;
            }
        }
        match staged {
            Some(entry) => {
                debug_assert_eq!(entry.wire, bytes);
                self.promote_staged(entry).await.map_err(|e| e.to_string())?;
            }
            None => {
                let items = decode_block_txs(&block.txs);
                let evict = items.iter().map(|(_, _, h)| *h).collect();
                self.commit_block(block, items, evict).await.map_err(|e| e.to_string())?;
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
            // re-check the link (head may have moved past it via backfill)
            let (hc, hn, _) = *self.head.lock().await;
            if nb.number != hn + 1 || nb.parent != hc {
                break;
            }
            self.commit_block(nb, items, evict).await.map_err(|e| e.to_string())?;
            self.announce_to_peers(c, num);
            parent = c;
        }
        Ok(NewBlockOutcome::Valid(commitment))
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

    /// Handle a peer's announcement: known -> ignore (the universal in-sync
    /// case, zero further traffic); head+1 -> pull the block from peers (the
    /// announcer is among them and provably has it); further ahead -> arm the
    /// backfill (existing machinery).
    pub async fn on_announce(&self, commitment: B256, number: u64) {
        self.stats.announces_rx.fetch_add(1, Ordering::Relaxed);
        let (head_c, head_n, _) = *self.head.lock().await;
        if number <= head_n {
            // known height: our chain already has a block there (commitment
            // equality is irrelevant to traffic — never pull backwards)
            let _ = commitment;
            self.stats.announces_known.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let _ = head_c;
        // ahead of us: raise the target and let the backfill pull the gap
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
        Ok(self
            .log
            .lock()
            .await
            .read_block(number)
            .map_err(|e| e.to_string())?
            .map(|b| b.to_wire_bytes()))
    }

    pub async fn receipt(&self, hash: B256) -> Option<LeanReceipt> {
        self.receipts.lock().await.iter().rev().find(|r| r.tx_hash == hash).cloned()
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
