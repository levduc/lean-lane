//! `lean-bench native`: the lean half of `bench/native-transfer`.
//!
//! N = 1 workload, identical to the reth half (`bench/native-transfer/reth-side`):
//! `accounts` pre-funded accounts (index `i < txs` is sender `i`, a real key;
//! the rest are keyless synthetic addresses), `txs` transactions per block,
//! one per sender (sender `t` sends tx `t` of every block at nonce = block
//! index), recipient uniform over all accounts, fixed amount 1 gwei.
//! `warmup` + `blocks` consecutive blocks, each on the previous post-state.
//! Signing is never timed.
//!
//! Timed per block: frame+decode, recover (rayon), execute, then
//! `process_block` (the whole pipeline again, end to end) + `apply`. The
//! headline is `process_block + apply`, as in the engine mode. Optional: the
//! node's block-log append with fsync (`--log-dir`).

use crate::util::{median, peak_rss_mb, sender_address, sender_key, synthetic_address, Rng};
use alloy_primitives::{Address, B256};
use lean_core::framing::{commitment_of_list, to_wire};
use lean_core::{Acct, BlockHeader, EngineCtx, FlatState};
use lean_lane_node::chain::{BlockLog, LeanBlock};
use lean_native::{lane_domain, lean_fee, LeanTx, Output, AMOUNT_UNIT};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

pub const CHAIN_ID: u64 = 1338;
/// Genesis balance of every account, wei (1e6 ETH), same as the reth half.
pub const FUND: u128 = 1_000_000 * 1_000_000_000_000_000_000;
/// Output amount in AMOUNT_UNIT (1 gwei).
pub const AMOUNT: u64 = 1;

pub fn beneficiary() -> Address {
    Address::with_last_byte(0xbe)
}

#[derive(Clone, Debug)]
pub struct NativeOpts {
    pub accounts: u64,
    pub txs: u64,
    pub blocks: usize,
    pub warmup: usize,
    pub seed: u64,
    pub log_dir: Option<PathBuf>,
}

/// Recipient index of tx `t` in block `b` (shared scheme with the reth half).
pub fn recipient_index(seed: u64, b: usize, t: u64, accounts: u64) -> u64 {
    Rng::new(seed ^ ((b as u64) << 40) ^ (t << 1)).below(accounts)
}

fn account_address(idx: u64, senders: &[Address]) -> Address {
    if idx < senders.len() as u64 {
        senders[idx as usize]
    } else {
        synthetic_address(idx)
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BlockMs {
    pub block: usize,
    pub frame_decode: f64,
    pub recover: f64,
    pub execute: f64,
    pub process_block: f64,
    pub apply: f64,
    /// process_block + apply (headline)
    pub total: f64,
    pub log_append_fsync: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Phase {
    pub phase: String,
    pub ms_per_block: f64,
    pub tx_per_s: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Checks {
    pub zero_noops: bool,
    pub every_tx_applied: bool,
    pub nonces_advanced: bool,
    pub balances_match_model: bool,
    pub conservation: bool,
    pub accounts_checked: usize,
    pub all_passed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct NativeReport {
    pub engine: &'static str,
    pub accounts: u64,
    pub txs_per_block: u64,
    pub payments_per_tx: u64,
    pub tx_bytes: usize,
    pub warmup_blocks: usize,
    pub measured_blocks: usize,
    pub threads: usize,
    pub seed: u64,
    pub setup_s: HashMap<&'static str, f64>,
    pub per_block_ms: Vec<BlockMs>,
    /// medians over the measured blocks
    pub phases: Vec<Phase>,
    pub checks: Checks,
    pub peak_rss_mb: f64,
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

pub fn run(o: &NativeOpts) -> eyre::Result<NativeReport> {
    eyre::ensure!(o.txs >= 1 && o.blocks >= 1, "need txs >= 1 and blocks >= 1");
    eyre::ensure!(
        o.accounts >= o.txs,
        "accounts ({}) must be >= txs per block ({}): every sender is a pre-funded account",
        o.accounts,
        o.txs
    );
    eyre::ensure!(o.warmup + o.blocks <= u32::MAX as usize, "too many blocks");
    let nblocks = o.warmup + o.blocks;
    let mut setup = HashMap::new();

    // keys and state (untimed)
    let t = Instant::now();
    let senders: Vec<Address> = (0..o.txs).into_par_iter().map(sender_address).collect();
    setup.insert("derive_sender_keys", t.elapsed().as_secs_f64());
    let t = Instant::now();
    let mut state = FlatState::default();
    state.accounts.reserve(o.accounts as usize + 1);
    for i in 0..o.accounts {
        state.accounts.insert(
            account_address(i, &senders),
            Acct {
                nonce: 0,
                balance: FUND,
            },
        );
    }
    setup.insert("build_state", t.elapsed().as_secs_f64());

    // pre-signed blocks (untimed)
    let t = Instant::now();
    let domain = lane_domain(CHAIN_ID);
    let mut parent: B256 = lean_core::genesis_commitment(CHAIN_ID);
    let mut wires = Vec::with_capacity(nblocks);
    for b in 0..nblocks {
        let txs: Vec<Vec<u8>> = (0..o.txs)
            .into_par_iter()
            .map(|t| {
                let to = account_address(recipient_index(o.seed, b, t, o.accounts), &senders);
                LeanTx::sign(
                    b as u32,
                    vec![Output { to, amount: AMOUNT }],
                    &sender_key(t),
                    &domain,
                )
                .encode()
            })
            .collect();
        let h = BlockHeader {
            parent,
            number: b as u64 + 1,
            timestamp_ms: 1_000 * (b as u64 + 1),
        };
        parent = commitment_of_list(&h, &txs);
        wires.push(to_wire(&h, &txs));
    }
    let tx_bytes = {
        let (_, txs) = lean_core::scan(&wires[0]).expect("framing");
        txs[0].len()
    };
    setup.insert("sign_blocks", t.elapsed().as_secs_f64());

    let mut log = match &o.log_dir {
        Some(d) => {
            std::fs::create_dir_all(d)?;
            let p = d.join("lean-native-blocks.log");
            let _ = std::fs::remove_file(&p);
            Some((BlockLog::open(&p, true)?, p))
        }
        None => None,
    };

    let ctx = EngineCtx::new(CHAIN_ID, beneficiary());
    let mut per_block = Vec::with_capacity(nblocks);
    let (mut zero_noops, mut every_applied) = (true, true);
    for (b, wire) in wires.iter().enumerate() {
        let mut m = BlockMs {
            block: b + 1,
            ..Default::default()
        };
        let t = Instant::now();
        let f = lean_core::frame(wire).expect("framing");
        let decoded = lean_core::decode_txs(&f.txs, &ctx);
        m.frame_decode = ms(t);

        let t = Instant::now();
        let items = lean_core::recover(decoded, &ctx);
        m.recover = ms(t);

        let t = Instant::now();
        let delta = lean_core::execute(&state, &items, &ctx);
        m.execute = ms(t);
        drop(items);

        let t = Instant::now();
        let p = lean_core::process_block(&state, wire, &ctx).expect("block");
        m.process_block = ms(t);
        assert_eq!(p.delta.writes, delta.writes, "process_block != phases");
        drop(delta);
        zero_noops &= p.delta.outcome.noops == 0;
        every_applied &=
            p.delta.outcome.applied as u64 == o.txs && p.delta.outcome.outputs as u64 == o.txs;

        let t = Instant::now();
        lean_core::apply(&mut state, &p.delta);
        m.apply = ms(t);
        m.total = m.process_block + m.apply;

        if let Some((log, _)) = log.as_mut() {
            let blk = LeanBlock::from_wire_bytes(wire).map_err(|e| eyre::eyre!(e))?;
            let t = Instant::now();
            log.append(&blk)?;
            m.log_append_fsync = Some(ms(t));
        }
        eprintln!(
            "  lean block {:>2}{}: total {:>8.2} ms (frame+dec {:.2} rec {:.2} exec {:.2} | \
             process_block {:.2} apply {:.2}){}",
            b + 1,
            if b < o.warmup { " (warm-up)" } else { "" },
            m.total,
            m.frame_decode,
            m.recover,
            m.execute,
            m.process_block,
            m.apply,
            m.log_append_fsync
                .map(|v| format!(" log+fsync {v:.2}"))
                .unwrap_or_default()
        );
        per_block.push(m);
    }
    if let Some((_, p)) = log.take() {
        let _ = std::fs::remove_file(p);
    }

    // correctness: every touched account against a model
    let fee = lean_fee(1);
    let amount_wei = AMOUNT as u128 * AMOUNT_UNIT;
    let mut model: HashMap<Address, (u64, i128)> = HashMap::new();
    for b in 0..nblocks {
        for t in 0..o.txs {
            let s = model.entry(senders[t as usize]).or_default();
            s.0 += 1;
            s.1 -= (amount_wei + fee) as i128;
            let to = account_address(recipient_index(o.seed, b, t, o.accounts), &senders);
            model.entry(to).or_default().1 += amount_wei as i128;
        }
    }
    let ben = model.entry(beneficiary()).or_default();
    ben.1 += (fee * o.txs as u128 * nblocks as u128) as i128;
    let mut nonces_ok = true;
    let mut balances_ok = true;
    let mut sum_delta: i128 = 0;
    for (a, (nonce, delta)) in &model {
        let acct = state.get(a);
        let base: u128 = if *a == beneficiary() { 0 } else { FUND };
        nonces_ok &= acct.nonce == *nonce;
        balances_ok &= acct.balance as i128 == base as i128 + delta;
        sum_delta += acct.balance as i128 - base as i128;
    }
    for t in 0..o.txs {
        nonces_ok &= state.get(&senders[t as usize]).nonce as usize == nblocks;
    }
    let conservation = sum_delta == 0 && state.accounts.len() as u64 == o.accounts + 1;
    let checks = Checks {
        zero_noops,
        every_tx_applied: every_applied,
        nonces_advanced: nonces_ok,
        balances_match_model: balances_ok,
        conservation,
        accounts_checked: model.len(),
        all_passed: zero_noops && every_applied && nonces_ok && balances_ok && conservation,
    };

    let measured = &per_block[o.warmup..];
    let med = |f: fn(&BlockMs) -> f64| -> f64 {
        let mut v: Vec<f64> = measured.iter().map(f).collect();
        median(&mut v)
    };
    let tps = |ms: f64| o.txs as f64 / (ms / 1e3);
    let mut phases = Vec::new();
    let mut push = |name: &str, v: f64| {
        phases.push(Phase {
            phase: name.into(),
            ms_per_block: v,
            tx_per_s: tps(v),
        })
    };
    push("frame + decode", med(|m| m.frame_decode));
    push("recover (parallel)", med(|m| m.recover));
    push("execute", med(|m| m.execute));
    push("apply", med(|m| m.apply));
    push(
        "decode + recover + execute + apply (process_block + apply)",
        med(|m| m.total),
    );
    if o.log_dir.is_some() {
        push(
            "block-log append + fsync",
            med(|m| m.log_append_fsync.unwrap_or(0.0)),
        );
        push(
            "engine + log append + fsync",
            med(|m| m.total + m.log_append_fsync.unwrap_or(0.0)),
        );
    }

    Ok(NativeReport {
        engine: "lean",
        accounts: o.accounts,
        txs_per_block: o.txs,
        payments_per_tx: 1,
        tx_bytes,
        warmup_blocks: o.warmup,
        measured_blocks: o.blocks,
        threads: rayon::current_num_threads(),
        seed: o.seed,
        setup_s: setup,
        per_block_ms: per_block,
        phases,
        checks,
        peak_rss_mb: peak_rss_mb(),
    })
}
