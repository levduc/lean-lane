//! reth half of `bench/native-transfer`: native ETH transfers imported by
//! reth v2.7.0, with no consensus, networking, mempool or JSON-RPC.
//!
//! Workload (identical to `lean-bench native`): `accounts` pre-funded
//! accounts (index `i < txs` is sender `i`, a real key; the rest are keyless
//! synthetic addresses), `txs` transactions per block, one per sender (sender
//! `t` sends tx `t` of every block, nonce = block index), recipient uniform
//! over all accounts, value 1 gwei. Each tx is a signed EIP-1559 transfer
//! (21,000 gas). All forks through Osaka are active from genesis; the four
//! system contracts (EIP-4788, 2935, 7002, 7251) are deployed at genesis, as
//! on a real chain. `warmup` + `blocks` consecutive blocks.
//!
//! Two passes over the same blocks, each on a fresh copy of the same genesis:
//!
//! DIRECT path, phase by phase, each block on the previous block's persisted
//! state (this pass also fills in each header's execution results):
//!   1. sender recovery (rayon, `recover_signers`);
//!   2. execution through `EthEvmConfig::executor` (`BasicBlockExecutor`) on
//!      a latest-state provider over MDBX;
//!   3. state root: `hashed_post_state` + `state_root_with_updates` (the
//!      incremental root against the trie already in MDBX; reth's serial
//!      state-root path, used by the engine as its fallback);
//!   4. persist: `save_blocks` + `commit`, what reth's persistence service
//!      does, into MDBX + static files + RocksDB (storage v2, the default for
//!      new nodes in v2.7.0).
//!
//! Informational (not in the headline rows): transactions root + receipts
//! root + logs bloom.
//!
//! ENGINE path (`engine.rs`): the finished blocks through reth's engine tree
//! as in-process `reth_newPayload` + `forkchoiceUpdated` messages: what a
//! running node does on newPayload (overlapped recovery/execution/state root,
//! caches, consensus checks). Persistence is deferred there, as in a node.

mod engine;

use alloy_consensus::{
    proofs, Header, SignableTransaction, TxEip1559, TxReceipt, EMPTY_OMMER_ROOT_HASH,
    EMPTY_ROOT_HASH,
};
use alloy_eips::{eip2935, eip4788, eip4895::Withdrawals, eip7002, eip7251};
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_primitives::{keccak256, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_rpc_types_engine::{ExecutionData, ExecutionPayload};
use clap::Parser;
use rayon::prelude::*;
use reth_chain_state::ExecutedBlock;
use reth_chainspec::{Chain, ChainSpec, ChainSpecBuilder, EthChainSpec};
use reth_db::{mdbx::DatabaseArguments, ClientVersion};
use reth_ethereum_primitives::{Block, BlockBody, Receipt, TransactionSigned};
use reth_evm::{execute::Executor, ConfigureEvm};
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::{transaction::recover::recover_signers, Account, SealedBlock};
use reth_provider::{
    test_utils::create_test_provider_factory_with_chain_spec_and_db_args, AccountReader,
    DBProvider, DatabaseProviderFactory, EvmStateProviderAdapter, HashedPostStateProvider,
    SaveBlocksInput, StateProviderFactory, StateRootProvider, StorageSettingsCache,
};
use reth_revm::database::StateProviderDatabase;
use reth_trie::{
    prefix_set::{PrefixSetMut, TriePrefixSets},
    StateRoot,
};
use reth_trie_common::ComputedTrieData;
use reth_trie_db::{DatabaseHashedCursorFactory, DatabaseStateRoot, DatabaseTrieCursorFactory};
use secp256k1::{Message, SecretKey, SECP256K1};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const CHAIN_ID: u64 = 1337;
/// Genesis balance of every account, wei (1e6 ETH), same as the lean half.
const FUND: u128 = 1_000_000 * 1_000_000_000_000_000_000;
/// Transfer value: 1 gwei (the lean half's 1-unit output).
const VALUE: u128 = 1_000_000_000;
/// Genesis base fee, wei (the EIP-1559 floor). Later blocks follow EIP-1559
/// from their parent (they are full, so it rises by 1 wei per block).
const GENESIS_BASE_FEE: u64 = 7;
const PRIORITY_FEE: u128 = 1_000_000_000;
const MAX_FEE: u128 = 2_000_000_000;
const TRANSFER_GAS: u64 = 21_000;

#[derive(Parser, Debug)]
#[command(
    name = "reth-native-bench",
    about = "reth v2.7.0 native-transfer import benchmark"
)]
struct Args {
    /// Pre-funded accounts.
    #[arg(long, default_value_t = 1_000_000)]
    accounts: u64,
    /// Transactions per block (= distinct senders).
    #[arg(long, default_value_t = 20_000)]
    txs: u64,
    /// Measured blocks.
    #[arg(long, default_value_t = 5)]
    blocks: usize,
    /// Warm-up blocks (run first, excluded from the medians).
    #[arg(long, default_value_t = 1)]
    warmup: usize,
    /// Thread budget: the rayon pool, and the CPU pool size of the engine
    /// path's runtime (default: all CPUs).
    #[arg(long)]
    threads: Option<usize>,
    /// Workload seed (must match the lean half for identical recipients).
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Parent directory for the temporary datadirs (MDBX, static files,
    /// RocksDB). Removed on exit.
    #[arg(long)]
    tmpdir: Option<PathBuf>,
    /// Skip the end-of-run full state-root recomputation from the database.
    #[arg(long)]
    no_verify_root: bool,
    /// Skip the engine path.
    #[arg(long)]
    no_engine: bool,
    /// Write the JSON report to this file.
    #[arg(long)]
    out: Option<PathBuf>,
}

// ---- workload scheme shared with lean-bench (crates/lean-bench/src/util.rs) ----

/// splitmix64, version-stable.
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

fn sender_key(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_byte_array(&b).expect("valid key")
}

fn sender_address(i: u64) -> Address {
    let pk = sender_key(i).public_key(SECP256K1).serialize_uncompressed();
    Address::from_slice(&keccak256(&pk[1..])[12..])
}

fn synthetic_address(i: u64) -> Address {
    let mut r = Rng(i ^ 0x5eed_0000_a11c_e000);
    let mut b = [0u8; 20];
    b[..8].copy_from_slice(&r.next_u64().to_le_bytes());
    b[8..16].copy_from_slice(&r.next_u64().to_le_bytes());
    b[16..].copy_from_slice(&r.next_u64().to_le_bytes()[..4]);
    Address::from(b)
}

fn account_address(idx: u64, senders: &[Address]) -> Address {
    if idx < senders.len() as u64 {
        senders[idx as usize]
    } else {
        synthetic_address(idx)
    }
}

fn recipient_index(seed: u64, b: usize, t: u64, accounts: u64) -> u64 {
    Rng(seed ^ ((b as u64) << 40) ^ (t << 1)).below(accounts)
}

fn beneficiary() -> Address {
    Address::with_last_byte(0xbe)
}

// ---- report ----

#[derive(Clone, Debug, Default, Serialize)]
struct BlockMs {
    block: u64,
    base_fee: u64,
    recover: f64,
    execute: f64,
    hashed_post_state: f64,
    trie_root: f64,
    state_root: f64,
    persist: f64,
    block_roots: f64,
    recover_execute: f64,
    full_import: f64,
}

#[derive(Clone, Debug, Serialize)]
struct Phase {
    phase: String,
    ms_per_block: f64,
    tx_per_s: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
struct AccountChecks {
    nonces_advanced: bool,
    balances_match_model: bool,
    conservation: bool,
    accounts_checked: usize,
}

impl AccountChecks {
    fn ok(&self) -> bool {
        self.nonces_advanced && self.balances_match_model && self.conservation
    }
}

#[derive(Clone, Debug, Default, Serialize)]
struct Checks {
    every_receipt_successful: bool,
    gas_used_is_21000_per_tx: bool,
    direct_accounts: AccountChecks,
    /// None if skipped (`--no-verify-root`)
    full_state_root_recompute_matches: Option<bool>,
    /// every block VALID (state root, receipts root, gas checked by the
    /// engine against the direct path's headers); None if `--no-engine`
    engine_all_blocks_valid: Option<bool>,
    engine_accounts: Option<AccountChecks>,
    all_passed: bool,
}

#[derive(Serialize)]
struct Report {
    engine: &'static str,
    reth_tag: &'static str,
    accounts: u64,
    txs_per_block: u64,
    payments_per_tx: u64,
    tx_bytes: usize,
    warmup_blocks: usize,
    measured_blocks: usize,
    threads: usize,
    seed: u64,
    storage: &'static str,
    setup_s: BTreeMap<&'static str, f64>,
    per_block_ms: Vec<BlockMs>,
    engine_per_block_ms: Vec<engine::EngineBlockMs>,
    phases: Vec<Phase>,
    checks: Checks,
    final_state_root: B256,
    peak_rss_mb: f64,
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let m = v.len() / 2;
    if v.len() % 2 == 1 {
        v[m]
    } else {
        (v[m - 1] + v[m]) / 2.0
    }
}

fn peak_rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok())
        })
        .map(|kb| kb / 1024.0)
        .unwrap_or(0.0)
}

fn sign(tx: TxEip1559, sk: &SecretKey) -> TransactionSigned {
    let hash = tx.signature_hash();
    let (rec, bytes) = SECP256K1
        .sign_ecdsa_recoverable(&Message::from_digest(hash.0), sk)
        .serialize_compact();
    let sig = Signature::new(
        U256::from_be_slice(&bytes[..32]),
        U256::from_be_slice(&bytes[32..]),
        i32::from(rec) != 0,
    );
    tx.into_signed(sig).into()
}

fn system_contract(code: &Bytes) -> GenesisAccount {
    GenesisAccount::default()
        .with_nonce(Some(1))
        .with_code(Some(code.clone()))
}

/// A fresh tempdir provider factory (production MDBX arguments) with the
/// genesis written: accounts, hashed state, and the full trie.
fn new_chain(chain_spec: &Arc<ChainSpec>) -> eyre::Result<engine::Factory> {
    let factory = create_test_provider_factory_with_chain_spec_and_db_args(
        chain_spec.clone(),
        DatabaseArguments::new(ClientVersion::default()),
    );
    let genesis_hash = reth_db_common::init::init_genesis(&factory)?;
    eyre::ensure!(
        genesis_hash == chain_spec.genesis_hash(),
        "genesis hash mismatch"
    );
    Ok(factory)
}

/// Expected (nonce, balance delta) of every touched account.
struct Model {
    accounts: HashMap<Address, (u64, i128)>,
    burned: i128,
    senders: Vec<Address>,
    blocks: u64,
}

fn check_accounts(
    model: &Model,
    basic: impl Fn(&Address) -> eyre::Result<Option<Account>>,
) -> eyre::Result<AccountChecks> {
    let (mut nonces_ok, mut balances_ok) = (true, true);
    let mut sum_delta: i128 = 0;
    for (addr, (nonce, delta)) in &model.accounts {
        let acct = basic(addr)?.unwrap_or_default();
        let base: i128 = if *addr == beneficiary() {
            0
        } else {
            FUND as i128
        };
        let bal = acct.balance.to::<u128>() as i128;
        nonces_ok &= acct.nonce == *nonce;
        balances_ok &= bal == base + delta;
        sum_delta += bal - base;
    }
    for s in &model.senders {
        nonces_ok &= basic(s)?.map(|x| x.nonce) == Some(model.blocks);
    }
    Ok(AccountChecks {
        nonces_advanced: nonces_ok,
        balances_match_model: balances_ok,
        conservation: sum_delta == -model.burned,
        accounts_checked: model.accounts.len(),
    })
}

fn main() -> eyre::Result<()> {
    let a = Args::parse();
    eyre::ensure!(a.txs >= 1 && a.blocks >= 1, "need txs >= 1 and blocks >= 1");
    eyre::ensure!(
        a.accounts >= a.txs,
        "accounts ({}) must be >= txs per block ({})",
        a.accounts,
        a.txs
    );
    let threads = a.threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()?;
    // All datadirs go under one private dir, removed at the end.
    let scratch = a
        .tmpdir
        .clone()
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("reth-native-bench-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;
    // reth's tempdir datadir helper creates its directory under TMPDIR.
    std::env::set_var("TMPDIR", &scratch);
    let result = run(&a, threads);
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn run(a: &Args, threads: usize) -> eyre::Result<()> {
    let nblocks = a.warmup + a.blocks;
    let mut setup = BTreeMap::new();
    eprintln!(
        "reth-native-bench (reth v2.7.0): {} accounts, {} txs/block, {}+{} blocks, {} threads",
        a.accounts, a.txs, a.warmup, a.blocks, threads
    );

    // ---- genesis: every account funded, system contracts deployed ----
    let t = Instant::now();
    let senders: Vec<Address> = (0..a.txs).into_par_iter().map(sender_address).collect();
    setup.insert("derive_sender_keys", t.elapsed().as_secs_f64());
    let t = Instant::now();
    let mut alloc = BTreeMap::new();
    for i in 0..a.accounts {
        alloc.insert(
            account_address(i, &senders),
            GenesisAccount::default().with_balance(U256::from(FUND)),
        );
    }
    eyre::ensure!(
        alloc.len() as u64 == a.accounts,
        "address collision in the account set"
    );
    alloc.insert(
        eip4788::BEACON_ROOTS_ADDRESS,
        system_contract(&eip4788::BEACON_ROOTS_CODE),
    );
    alloc.insert(
        eip2935::HISTORY_STORAGE_ADDRESS,
        system_contract(&eip2935::HISTORY_STORAGE_CODE),
    );
    alloc.insert(
        eip7002::WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS,
        system_contract(&eip7002::WITHDRAWAL_REQUEST_PREDEPLOY_CODE),
    );
    alloc.insert(
        eip7251::CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS,
        system_contract(&eip7251::CONSOLIDATION_REQUEST_PREDEPLOY_CODE),
    );
    let gas_limit = TRANSFER_GAS * a.txs + 10_000_000;
    let genesis = Genesis {
        gas_limit,
        base_fee_per_gas: Some(GENESIS_BASE_FEE as u128),
        excess_blob_gas: Some(0),
        blob_gas_used: Some(0),
        alloc,
        ..Default::default()
    };
    let chain_spec: Arc<ChainSpec> = Arc::new(
        ChainSpecBuilder::default()
            .chain(Chain::from_id(CHAIN_ID))
            .genesis(genesis)
            .osaka_activated()
            .build(),
    );
    setup.insert("chain_spec_with_genesis_root", t.elapsed().as_secs_f64());

    let t = Instant::now();
    let factory = new_chain(&chain_spec)?;
    setup.insert("init_genesis_direct", t.elapsed().as_secs_f64());
    eprintln!(
        "  genesis written in {:.1}s (state root {})",
        t.elapsed().as_secs_f64(),
        chain_spec.genesis_header().state_root
    );

    // ---- pre-built, pre-signed blocks (untimed) ----
    let t = Instant::now();
    let bodies: Vec<BlockBody> = (0..nblocks)
        .map(|b| {
            let transactions: Vec<TransactionSigned> = (0..a.txs)
                .into_par_iter()
                .map(|t| {
                    let to = account_address(recipient_index(a.seed, b, t, a.accounts), &senders);
                    let tx = TxEip1559 {
                        chain_id: CHAIN_ID,
                        nonce: b as u64,
                        gas_limit: TRANSFER_GAS,
                        max_fee_per_gas: MAX_FEE,
                        max_priority_fee_per_gas: PRIORITY_FEE,
                        to: TxKind::Call(to),
                        value: U256::from(VALUE),
                        access_list: Default::default(),
                        input: Bytes::new(),
                    };
                    sign(tx, &sender_key(t))
                })
                .collect();
            BlockBody {
                transactions,
                ommers: vec![],
                withdrawals: Some(Withdrawals::default()),
            }
        })
        .collect();
    let tx_roots: Vec<B256> = bodies
        .iter()
        .map(|b| proofs::calculate_transaction_root(&b.transactions))
        .collect();
    let tx_bytes = {
        use alloy_eips::eip2718::Encodable2718;
        bodies[0].transactions[0].encode_2718_len()
    };
    setup.insert("sign_blocks", t.elapsed().as_secs_f64());

    // ---- DIRECT path ----
    let evm_config = EthEvmConfig::new(chain_spec.clone());
    let mut parent: Header = chain_spec.genesis_header().clone();
    let mut parent_hash = chain_spec.genesis_hash();
    let mut per_block = Vec::with_capacity(nblocks);
    let mut payloads: Vec<ExecutionData> = Vec::with_capacity(nblocks);
    let mut hashes: Vec<B256> = Vec::with_capacity(nblocks);
    let (mut receipts_ok, mut gas_ok) = (true, true);
    let mut last_root = B256::ZERO;
    for (b, body) in bodies.into_iter().enumerate() {
        let number = b as u64 + 1;
        let timestamp = parent.timestamp + 12;
        let base_fee = parent
            .next_block_base_fee(chain_spec.base_fee_params_at_timestamp(timestamp))
            .expect("London active");
        let mut m = BlockMs {
            block: number,
            base_fee,
            ..Default::default()
        };
        // The header a proposer would send, minus the execution results (filled
        // in below from our own execution, as a builder would).
        let mut header = Header {
            parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: beneficiary(),
            state_root: B256::ZERO,
            transactions_root: tx_roots[b],
            receipts_root: B256::ZERO,
            withdrawals_root: Some(EMPTY_ROOT_HASH),
            logs_bloom: Default::default(),
            difficulty: U256::ZERO,
            number,
            gas_limit,
            gas_used: 0,
            timestamp,
            mix_hash: keccak256(number.to_be_bytes()),
            nonce: Default::default(),
            base_fee_per_gas: Some(base_fee),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(keccak256((number + 1_000_000).to_be_bytes())),
            requests_hash: Some(alloy_eips::eip7685::EMPTY_REQUESTS_HASH),
            extra_data: Bytes::new(),
            ..Default::default()
        };

        // 1. sender recovery (parallel)
        let t = Instant::now();
        let signers = recover_signers(&body.transactions)?;
        m.recover = ms(t);
        let recovered =
            SealedBlock::<Block>::seal_parts(header.clone(), body).with_senders(signers);

        // 2. execution
        let t = Instant::now();
        let db = StateProviderDatabase::new(EvmStateProviderAdapter(factory.latest()?));
        let output = evm_config.executor(db).execute(&recovered)?;
        m.execute = ms(t);
        let receipts: &Vec<Receipt> = &output.result.receipts;
        receipts_ok &= receipts.len() as u64 == a.txs && receipts.iter().all(|r| r.success);
        gas_ok &= output.result.gas_used == TRANSFER_GAS * a.txs
            && receipts.last().map(|r| r.cumulative_gas_used) == Some(TRANSFER_GAS * a.txs);

        // 3. state root: hashed post-state + incremental root over the MDBX trie
        let t = Instant::now();
        let sp = factory.latest()?;
        let hashed = sp.hashed_post_state(&output.state)?;
        m.hashed_post_state = ms(t);
        let t2 = Instant::now();
        let (root, trie_updates) = sp.state_root_with_updates(hashed.clone())?;
        drop(sp);
        m.trie_root = ms(t2);
        m.state_root = ms(t);
        last_root = root;

        // informational: the block roots a validating node recomputes
        let t = Instant::now();
        let tx_root = proofs::calculate_transaction_root(&recovered.body().transactions);
        let with_bloom: Vec<_> = receipts.iter().map(|r| r.with_bloom_ref()).collect();
        let receipts_root = proofs::calculate_receipt_root(&with_bloom);
        let logs_bloom = with_bloom
            .iter()
            .fold(alloy_primitives::Bloom::default(), |acc, r| {
                acc | r.logs_bloom
            });
        m.block_roots = ms(t);
        eyre::ensure!(tx_root == tx_roots[b], "transactions root mismatch");

        // the final header carries our results; re-seal (cheap) and persist that
        header.state_root = root;
        header.receipts_root = receipts_root;
        header.logs_bloom = logs_bloom;
        header.gas_used = output.result.gas_used;
        header.requests_hash = Some(output.result.requests.requests_hash());
        let (sealed, signers) = recovered.split_sealed();
        let (_, body) = sealed.split_header_body();
        let final_block = SealedBlock::<Block>::seal_parts(header.clone(), body);
        parent_hash = final_block.hash();
        parent = header;
        if !a.no_engine {
            // the same block as an engine payload (untimed)
            let (payload, sidecar) =
                ExecutionPayload::from_block_unchecked(parent_hash, &final_block.clone_block());
            payloads.push(ExecutionData::new(payload, sidecar));
            hashes.push(parent_hash);
        }
        let final_block = final_block.with_senders(signers);

        // 4. persist: sort trie data, save_blocks, commit
        let t = Instant::now();
        let trie_data = ComputedTrieData::new(
            Arc::new(hashed.into_sorted()),
            Arc::new(trie_updates.into_sorted()),
        );
        let executed = ExecutedBlock::new(Arc::new(final_block), Arc::new(output), trie_data);
        let provider_rw = factory.database_provider_rw()?;
        provider_rw.save_blocks(&SaveBlocksInput::new(
            vec![executed],
            number - 1,
            number - 1,
            number,
            number,
        ))?;
        provider_rw.commit()?;
        m.persist = ms(t);

        m.recover_execute = m.recover + m.execute;
        m.full_import = m.recover + m.execute + m.state_root + m.persist;
        eprintln!(
            "  reth block {:>2}{}: full {:>8.2} ms = recover {:.2} + execute {:.2} + state root {:.2} \
             (hash {:.2}, trie {:.2}) + persist {:.2}  | roots {:.2}",
            number,
            if b < a.warmup { " (warm-up)" } else { "" },
            m.full_import,
            m.recover,
            m.execute,
            m.state_root,
            m.hashed_post_state,
            m.trie_root,
            m.persist,
            m.block_roots
        );
        per_block.push(m);
    }

    // ---- correctness of the direct path: every touched account vs a model ----
    let t = Instant::now();
    let mut model = Model {
        accounts: HashMap::new(),
        burned: 0,
        senders: senders.clone(),
        blocks: nblocks as u64,
    };
    for (b, blk) in per_block.iter().enumerate() {
        let fee_per_tx = TRANSFER_GAS as u128 * (blk.base_fee as u128 + PRIORITY_FEE);
        model.burned += (TRANSFER_GAS as u128 * blk.base_fee as u128 * a.txs as u128) as i128;
        for t in 0..a.txs {
            let s = model.accounts.entry(senders[t as usize]).or_default();
            s.0 += 1;
            s.1 -= (VALUE + fee_per_tx) as i128;
            let to = account_address(recipient_index(a.seed, b, t, a.accounts), &senders);
            model.accounts.entry(to).or_default().1 += VALUE as i128;
        }
    }
    let tips = TRANSFER_GAS as u128 * PRIORITY_FEE * a.txs as u128 * nblocks as u128;
    model.accounts.entry(beneficiary()).or_default().1 += tips as i128;
    let sp = factory.latest()?;
    let direct_accounts = check_accounts(&model, |x| Ok(sp.basic_account(x)?))?;
    drop(sp);
    setup.insert("verify_accounts_direct", t.elapsed().as_secs_f64());

    let full_root_ok = if a.no_verify_root {
        None
    } else {
        let t = Instant::now();
        let provider = factory.database_provider_ro()?;
        // Every account path and every system-contract storage path marked
        // changed: the root is rebuilt from the hashed state, ignoring the
        // stored branch nodes the incremental runs produced.
        let storage_prefix_sets = [
            eip4788::BEACON_ROOTS_ADDRESS,
            eip2935::HISTORY_STORAGE_ADDRESS,
            eip7002::WITHDRAWAL_REQUEST_PREDEPLOY_ADDRESS,
            eip7251::CONSOLIDATION_REQUEST_PREDEPLOY_ADDRESS,
        ]
        .into_iter()
        .map(|addr| (keccak256(addr), PrefixSetMut::all().freeze()))
        .collect();
        let prefix_sets = TriePrefixSets {
            account_prefix_set: PrefixSetMut::all().freeze(),
            storage_prefix_sets,
            destroyed_accounts: Default::default(),
        };
        let full = reth_trie_db::with_adapter!(provider, |A| {
            <StateRoot<DatabaseTrieCursorFactory<_, A>, DatabaseHashedCursorFactory<_>> as DatabaseStateRoot<_>>::from_tx(provider.tx_ref())
                .with_prefix_sets(prefix_sets)
                .root()?
        });
        setup.insert("verify_full_state_root", t.elapsed().as_secs_f64());
        eprintln!(
            "  full state-root recompute: {full} in {:.1}s",
            t.elapsed().as_secs_f64()
        );
        Some(full == last_root)
    };
    drop(factory); // frees the direct path's datadir before the engine path

    // ---- ENGINE path ----
    let mut engine_ms = Vec::new();
    let (mut engine_valid, mut engine_accounts) = (None, None);
    if !a.no_engine {
        let t = Instant::now();
        let factory = new_chain(&chain_spec)?;
        setup.insert("init_genesis_engine", t.elapsed().as_secs_f64());
        let (ms_, provider) = engine::run(
            factory,
            chain_spec.clone(),
            payloads,
            &hashes,
            threads,
            a.warmup,
        )?;
        engine_ms = ms_;
        engine_valid = Some(engine_ms.len() == nblocks);
        let t = Instant::now();
        let sp = provider.latest()?;
        engine_accounts = Some(check_accounts(&model, |x| Ok(sp.basic_account(x)?))?);
        setup.insert("verify_accounts_engine", t.elapsed().as_secs_f64());
    }

    let checks = Checks {
        every_receipt_successful: receipts_ok,
        gas_used_is_21000_per_tx: gas_ok,
        all_passed: receipts_ok
            && gas_ok
            && direct_accounts.ok()
            && full_root_ok.unwrap_or(true)
            && engine_valid.unwrap_or(true)
            && engine_accounts.as_ref().is_none_or(AccountChecks::ok),
        direct_accounts,
        full_state_root_recompute_matches: full_root_ok,
        engine_all_blocks_valid: engine_valid,
        engine_accounts,
    };

    let measured = &per_block[a.warmup..];
    let med = |f: fn(&BlockMs) -> f64| -> f64 {
        let mut v: Vec<f64> = measured.iter().map(f).collect();
        median(&mut v)
    };
    let mut phases = Vec::new();
    let mut push = |name: &str, v: f64| {
        phases.push(Phase {
            phase: name.into(),
            ms_per_block: v,
            tx_per_s: a.txs as f64 / (v / 1e3),
        })
    };
    push("1. recover senders (parallel)", med(|m| m.recover));
    push("2. execute (EVM, BasicBlockExecutor)", med(|m| m.execute));
    push(
        "3. state root (hashed state + incremental trie)",
        med(|m| m.state_root),
    );
    push("   3a. hashed post-state", med(|m| m.hashed_post_state));
    push(
        "   3b. incremental trie root (serial)",
        med(|m| m.trie_root),
    );
    push("4. persist (save_blocks + commit)", med(|m| m.persist));
    push("recover + execute", med(|m| m.recover_execute));
    push(
        "full import = recover + execute + state root + persist",
        med(|m| m.full_import),
    );
    push(
        "(info) tx root + receipts root + bloom",
        med(|m| m.block_roots),
    );
    if !engine_ms.is_empty() {
        let em = &engine_ms[a.warmup..];
        let emed = |f: fn(&engine::EngineBlockMs) -> f64| -> f64 {
            let mut v: Vec<f64> = em.iter().map(f).collect();
            median(&mut v)
        };
        let np = emed(|m| m.new_payload);
        push("engine newPayload (recover+execute+root, overlapped)", np);
        push("engine forkchoiceUpdated", emed(|m| m.forkchoice_updated));
        let persist = med(|m| m.persist);
        push("engine newPayload + persist (4.)", np + persist);
    }

    let report = Report {
        engine: "reth",
        reth_tag: "v2.7.0",
        accounts: a.accounts,
        txs_per_block: a.txs,
        payments_per_tx: 1,
        tx_bytes,
        warmup_blocks: a.warmup,
        measured_blocks: a.blocks,
        threads,
        seed: a.seed,
        storage: "reth storage v2 (MDBX + static files + RocksDB), MDBX SyncMode::Durable",
        setup_s: setup,
        per_block_ms: per_block,
        engine_per_block_ms: engine_ms,
        phases,
        checks,
        final_state_root: last_root,
        peak_rss_mb: peak_rss_mb(),
    };
    let json = serde_json::to_string_pretty(&report)?;
    match &a.out {
        Some(p) => std::fs::write(p, &json)?,
        None => println!("{json}"),
    }
    for p in &report.phases {
        eprintln!(
            "{:<58} {:>10.2} ms/block {:>12.0} tx/s",
            p.phase, p.ms_per_block, p.tx_per_s
        );
    }
    eprintln!("checks: {:?}", report.checks);
    eyre::ensure!(
        report.checks.all_passed,
        "correctness checks FAILED: {:?}",
        report.checks
    );
    Ok(())
}
