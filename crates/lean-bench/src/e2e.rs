//! End-to-end mode: two real lane nodes in-process (a proposer with a
//! pre-filled pool and a follower), driven unpaced by the stand-in CL logic
//! of `scripts/standin-cl.sh` — build on the proposer, stage everywhere,
//! anchor everywhere by commitment — for T seconds.
//!
//! Steps run sequentially per height (a real CL overlaps the follower's
//! stage with the proposer's work), so blocks/s here is conservative; the
//! per-step means are reported so they can be composed differently.

use crate::corpus::{account_address, Fanout, CHAIN_ID, FUND};
use crate::util::{median, sender_address, sender_key, Rng};
use alloy_primitives::Address;
use lean_lane_node::chain::BlockLog;
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome, StageOutcome};
use lean_lane_node::state::Acct;
use lean_native::{lane_domain, lean_gas, LeanTx, Output};
use rayon::prelude::*;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

pub struct E2eOpts {
    pub seconds: f64,
    pub fsync: bool,
    pub fanout: Fanout,
    pub budget: u64,
    pub accounts: u64,
    pub senders: u64,
    pub depth: u32,
    pub seed: u64,
    pub cache_dir: PathBuf,
}

#[derive(Debug, Default, Serialize)]
pub struct StepMs {
    pub mean: f64,
    pub p50: f64,
    pub max: f64,
}

fn step(v: &[f64]) -> StepMs {
    if v.is_empty() {
        return StepMs::default();
    }
    let mut s = v.to_vec();
    StepMs {
        mean: v.iter().sum::<f64>() / v.len() as f64,
        max: v.iter().cloned().fold(0.0, f64::max),
        p50: median(&mut s),
    }
}

#[derive(Debug, Serialize)]
pub struct E2eResult {
    pub mode: String,
    pub fanout: String,
    pub budget_gas: u64,
    pub accounts: u64,
    pub threads: usize,
    pub prefill_txs: usize,
    pub prefill_s: f64,
    /// measured blocks (the first full block is warm-up and excluded)
    pub blocks: usize,
    pub seconds: f64,
    pub txs_per_block: f64,
    pub payments_per_block: f64,
    pub fullness: f64,
    pub blocks_per_s: f64,
    pub tx_per_s: f64,
    pub payments_per_s: f64,
    /// proposer `arc_buildBlock` (pool pick + execute + self-stage)
    pub build_ms: StepMs,
    /// follower `arc_stageBlock` (framing, decode, recovery, execute)
    pub stage_ms: StepMs,
    /// proposer `arc_newBlock{commitment}` (promote: apply, log append, pool update)
    pub anchor_proposer_ms: StepMs,
    /// follower `arc_newBlock{commitment}`
    pub anchor_follower_ms: StepMs,
    /// `BlockLog::append` alone for the same blocks, this mode's fsync setting
    pub append_ms: StepMs,
    /// why the run ended: "time" or "pool drained"
    pub stopped_by: String,
}

/// Pre-signed pool fill: `senders` senders x `depth` consecutive nonces,
/// sender-major, cached like the engine corpora.
fn load_txs(o: &E2eOpts, senders: &[Address]) -> Vec<Vec<u8>> {
    let key = format!(
        "e2e-v1-a{}-n{}-k{}-d{}-s{}",
        o.accounts,
        o.fanout.spec().replace([':', ','], "_"),
        o.senders,
        o.depth,
        o.seed
    );
    let path = o.cache_dir.join(format!("{key}.bin"));
    if let Some(v) = crate::corpus::read_blob(&path, &key) {
        return v;
    }
    let domain = lane_domain(CHAIN_ID);
    let n_tx = o.senders as usize * o.depth as usize;
    let txs: Vec<Vec<u8>> = (0..n_tx)
        .into_par_iter()
        .map(|i| {
            let (k, d) = (
                i as u64 / o.depth as u64,
                (i as u64 % o.depth as u64) as u32,
            );
            let mut rng = Rng::new(o.seed ^ 0xe2e0_0000_0000 ^ (i as u64));
            let n = o.fanout.pick(&mut rng);
            let outputs = (0..n)
                .map(|_| Output {
                    to: account_address(rng.below(o.accounts), senders),
                    amount: 1 + rng.below(1000),
                })
                .collect();
            LeanTx::sign(d, outputs, &sender_key(k), &domain).encode()
        })
        .collect();
    if let Err(e) = crate::corpus::write_blob(&path, &key, &txs) {
        eprintln!("lean-bench: could not cache {}: {e}", path.display());
    }
    txs
}

fn open_node(dir: PathBuf, fsync: bool) -> eyre::Result<Arc<LaneNode>> {
    let cfg = Config {
        datadir: dir,
        chain_id: CHAIN_ID,
        snapshot_every: 0,
        fsync,
        ..Default::default()
    };
    Ok(Arc::new(LaneNode::open(cfg)?.0))
}

pub async fn run(o: E2eOpts) -> eyre::Result<E2eResult> {
    let mode = if o.fsync { "fsync" } else { "no-fsync" };
    let root = o
        .cache_dir
        .parent()
        .unwrap_or(&o.cache_dir)
        .join(format!("lean-bench-e2e/{}-{mode}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let senders: Vec<Address> = (0..o.senders).into_par_iter().map(sender_address).collect();
    eprintln!(
        "e2e[{mode}]: corpus {} senders x {} nonces",
        o.senders, o.depth
    );
    let txs = load_txs(&o, &senders);

    let a = open_node(root.join("proposer"), o.fsync)?;
    let b = open_node(root.join("follower"), o.fsync)?;
    eprintln!("e2e[{mode}]: seeding {} accounts on 2 nodes", o.accounts);
    for i in 0..o.accounts {
        let addr = account_address(i, &senders);
        let acct = Acct {
            nonce: 0,
            balance: FUND,
        };
        a.seed_account(addr, acct).await;
        b.seed_account(addr, acct).await;
    }

    eprintln!(
        "e2e[{mode}]: pre-filling the proposer pool with {} txs",
        txs.len()
    );
    let t_fill = Instant::now();
    let txs = Arc::new(txs);
    let workers = 16usize;
    let mut handles = Vec::new();
    for w in 0..workers {
        let (a, txs, depth, senders) = (a.clone(), txs.clone(), o.depth as usize, o.senders);
        handles.push(tokio::spawn(async move {
            let mut rejected = 0usize;
            let mut k = w as u64;
            while k < senders {
                for d in 0..depth {
                    if a.submit_raw(&txs[k as usize * depth + d]).await.is_err() {
                        rejected += 1;
                    }
                }
                k += workers as u64;
            }
            rejected
        }));
    }
    let mut rejected = 0;
    for h in handles {
        rejected += h.await?;
    }
    let prefill_s = t_fill.elapsed().as_secs_f64();
    if rejected > 0 {
        eprintln!("e2e[{mode}]: WARNING {rejected} txs rejected at admission");
    }

    let (mut build, mut stage, mut anc_a, mut anc_b) = (vec![], vec![], vec![], vec![]);
    let (mut n_txs, mut n_pay, mut n_gas) = (0u64, 0u64, 0u64);
    let mut appended: Vec<u64> = Vec::new();
    let mut stopped_by = "time";
    let mut t_start = Instant::now();
    let mut first = true;
    loop {
        if !first && t_start.elapsed().as_secs_f64() >= o.seconds {
            break;
        }
        let (c, n, ts) = *a.head.lock().await;
        let t = Instant::now();
        let (commitment, wire) = a
            .shim_build(c, n + 1, ts + 1, o.budget)
            .await
            .map_err(|e| eyre::eyre!(e))?;
        let t_build = ms(t);
        let (txs_n, pay_n, gas_n) = block_counts(&wire);
        if gas_n + lean_gas(1) <= o.budget && gas_n * 100 < o.budget * 99 {
            stopped_by = "pool drained";
            break;
        }
        let t = Instant::now();
        let sa = a.stage_block(&wire).await.map_err(|e| eyre::eyre!(e))?;
        let sb = b.stage_block(&wire).await.map_err(|e| eyre::eyre!(e))?;
        let t_stage = ms(t);
        eyre::ensure!(
            sa == StageOutcome::Staged(commitment) && sb == StageOutcome::Staged(commitment),
            "stage refused: {sa:?} / {sb:?}"
        );
        let t = Instant::now();
        let ra = a.shim_new_by_commitment(commitment).await;
        let t_anc_a = ms(t);
        let t = Instant::now();
        let rb = b.shim_new_by_commitment(commitment).await;
        let t_anc_b = ms(t);
        let want = Ok(NewBlockOutcome::Valid(commitment));
        eyre::ensure!(ra == want && rb == want, "anchor refused: {ra:?} / {rb:?}");
        if first {
            // warm-up block: excluded, clock starts after it
            first = false;
            t_start = Instant::now();
            continue;
        }
        build.push(t_build);
        stage.push(t_stage);
        anc_a.push(t_anc_a);
        anc_b.push(t_anc_b);
        n_txs += txs_n;
        n_pay += pay_n;
        n_gas += gas_n;
        appended.push(n + 1);
    }
    let secs = t_start.elapsed().as_secs_f64();
    let heads_agree = a.head.lock().await.0 == b.head.lock().await.0
        && a.state.lock().await.digest() == b.state.lock().await.digest();
    eyre::ensure!(heads_agree, "proposer and follower diverged");

    // append/fsync alone: re-append the measured blocks to a fresh log
    let mut append = Vec::new();
    {
        let mut src = a.log.lock().await;
        let dir = root.join("append-probe");
        std::fs::create_dir_all(&dir)?;
        let mut log = BlockLog::open(&dir.join("blocks.log"), o.fsync)?;
        for &num in &appended {
            let blk = src.read_block(num)?.expect("appended block");
            let t = Instant::now();
            log.append(&blk)?;
            append.push(ms(t));
        }
    }
    drop((a, b));
    let _ = std::fs::remove_dir_all(&root);

    let nb = appended.len().max(1) as f64;
    Ok(E2eResult {
        mode: mode.into(),
        fanout: o.fanout.spec(),
        budget_gas: o.budget,
        accounts: o.accounts,
        threads: rayon::current_num_threads(),
        prefill_txs: txs.len(),
        prefill_s,
        blocks: appended.len(),
        seconds: secs,
        txs_per_block: n_txs as f64 / nb,
        payments_per_block: n_pay as f64 / nb,
        fullness: n_gas as f64 / nb / o.budget as f64,
        blocks_per_s: appended.len() as f64 / secs,
        tx_per_s: n_txs as f64 / secs,
        payments_per_s: n_pay as f64 / secs,
        build_ms: step(&build),
        stage_ms: step(&stage),
        anchor_proposer_ms: step(&anc_a),
        anchor_follower_ms: step(&anc_b),
        append_ms: step(&append),
        stopped_by: stopped_by.into(),
    })
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

/// (txs, payments, gas) of a wire block.
fn block_counts(wire: &[u8]) -> (u64, u64, u64) {
    let (_, txs) = lean_core::scan(wire).expect("built block framing");
    let (mut p, mut g) = (0u64, 0u64);
    for t in &txs {
        let n = u16::from_le_bytes([t[5], t[6]]) as usize;
        p += n as u64;
        g += lean_gas(n);
    }
    (txs.len() as u64, p, g)
}
