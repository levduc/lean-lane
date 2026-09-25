//! Engine-only benchmark: the lean-core pipeline over cached corpora, phase
//! by phase, against an in-memory pre-funded state.

use crate::corpus::{self, beneficiary, Corpus, CorpusSpec, Fanout, CHAIN_ID};
use crate::util::{median, peak_rss_mb, reset_peak_rss, rss_mb};
use alloy_primitives::{keccak256, Address};
use lean_core::{Acct, EngineCtx, FlatState};
use lean_native::{lane_domain, pubkey_to_address, LeanTx, LEAN_TX_TYPE};
use serde::Serialize;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct Scenario {
    pub name: String,
    pub accounts: u64,
    pub fanout: Fanout,
    pub budget: u64,
}

pub const ACCOUNTS: [(u64, &str); 3] = [(100_000, "1e5"), (1_000_000, "1e6"), (10_000_000, "1e7")];
pub const BUDGETS: [(u64, &str); 3] = [
    (225_000_000, "225m"),
    (450_000_000, "450m"),
    (1_000_000_000, "1g"),
];

/// Every scenario, grouped by state size (so one state serves a group).
pub fn all_scenarios() -> Vec<Scenario> {
    let fanouts = [
        Fanout::Fixed(1),
        Fanout::Fixed(10),
        Fanout::Fixed(100),
        Fanout::roadmap_mix(),
    ];
    let mut v = Vec::new();
    for (accounts, al) in ACCOUNTS {
        for f in &fanouts {
            for (budget, bl) in BUDGETS {
                v.push(Scenario {
                    name: format!("s{al}-n{}-g{bl}", f.label()),
                    accounts,
                    fanout: f.clone(),
                    budget,
                });
            }
        }
    }
    v
}

/// `*` wildcard match (enough for `s1e6-*-g450m`).
pub fn glob(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let mut rest = s;
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(p) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(p);
        } else {
            match rest.find(p) {
                Some(at) => rest = &rest[at + p.len()..],
                None => return false,
            }
        }
    }
    true
}

pub struct Opts {
    pub runs: usize,
    pub warmup: usize,
    pub blocks: usize,
    pub serial: bool,
    pub micro: bool,
    pub seed: u64,
    pub cache_dir: PathBuf,
}

/// Per-block nanoseconds (median over runs of the per-run mean per block).
#[derive(Clone, Debug, Default, Serialize)]
pub struct PhaseNs {
    /// framing walk + commitment keccak + tx decode (parallel)
    pub frame_decode: f64,
    /// sender recovery on the rayon pool
    pub recover_parallel: f64,
    /// sender recovery on the calling thread (None if `--no-serial`)
    pub recover_serial: Option<f64>,
    /// total-STF execution
    pub execute: f64,
    /// delta written into the state
    pub apply: f64,
    /// `lean_core::process_block` end to end (parallel recovery)
    pub process_block: f64,
    /// process_block + apply: the engine's cost of one block
    pub total: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Hotspot {
    pub function: String,
    pub ns_per_block: f64,
    pub pct: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScenarioResult {
    pub scenario: String,
    pub accounts: u64,
    pub fanout: String,
    pub budget_gas: u64,
    pub threads: usize,
    pub blocks_per_run: usize,
    pub runs: usize,
    pub warmup_runs: usize,
    pub txs_per_block: f64,
    pub payments_per_block: f64,
    pub bytes_per_block: f64,
    /// gas used / budget, mean over the corpus blocks
    pub fullness: f64,
    pub corpus_cached: bool,
    pub corpus_load_s: f64,
    pub ns_per_block: PhaseNs,
    /// engine-only throughput (process_block + apply, parallel recovery)
    pub payments_per_s: f64,
    pub tx_per_s: f64,
    pub blocks_per_s: f64,
    /// the same with serial recovery (frame_decode + recover_serial + execute + apply)
    pub payments_per_s_serial_recovery: Option<f64>,
    pub tx_per_s_serial_recovery: Option<f64>,
    /// peak RSS during this scenario (state included), MB
    pub peak_rss_mb: f64,
    /// single-threaded CPU attribution of block 1 (`--micro`)
    pub hotspots: Option<Vec<Hotspot>>,
}

#[derive(Default, Clone, Copy)]
struct Sums {
    frame_decode: f64,
    rec_par: f64,
    rec_ser: f64,
    execute: f64,
    apply: f64,
    process: f64,
}

fn ns(t: Instant) -> f64 {
    t.elapsed().as_nanos() as f64
}

/// Measure one scenario against `state` (restored to its input value before
/// returning).
pub fn run_scenario(
    sc: &Scenario,
    state: &mut FlatState,
    senders: &[Address],
    opts: &Opts,
) -> ScenarioResult {
    reset_peak_rss();
    let t_load = Instant::now();
    let spec = CorpusSpec {
        accounts: sc.accounts,
        fanout: sc.fanout.clone(),
        budget: sc.budget,
        blocks: opts.blocks,
        seed: opts.seed,
    };
    let (corpus, cached) = corpus::load_or_build(&spec, senders, &opts.cache_dir);
    let corpus_load_s = t_load.elapsed().as_secs_f64();
    let par = EngineCtx::new(CHAIN_ID, beneficiary());
    let ser = par.with_parallel_recovery(false);
    let nb = corpus.blocks.len() as f64;
    let payments: u64 = corpus.stats.iter().map(|s| s.payments).sum();
    let txs: u64 = corpus.stats.iter().map(|s| s.txs).sum();

    let mut per_run: Vec<Sums> = Vec::new();
    for run in 0..opts.warmup + opts.runs {
        let check = run == opts.warmup;
        let mut s = Sums::default();
        let mut undo: Vec<Vec<(Address, Option<Acct>)>> = Vec::new();
        for (bi, wire) in corpus.blocks.iter().enumerate() {
            let t = Instant::now();
            let f = lean_core::frame(wire).expect("corpus framing");
            let decoded = lean_core::decode_txs(&f.txs, &par);
            s.frame_decode += ns(t);

            let d = decoded.clone();
            let t = Instant::now();
            let items = lean_core::recover(d, &par);
            s.rec_par += ns(t);

            if opts.serial {
                let d = decoded.clone();
                let t = Instant::now();
                let items_s = lean_core::recover(d, &ser);
                s.rec_ser += ns(t);
                if check {
                    assert_eq!(items_s, items, "serial and parallel recovery diverged");
                }
            }
            drop(decoded);

            let t = Instant::now();
            let delta = lean_core::execute(&*state, &items, &par);
            s.execute += ns(t);

            let t = Instant::now();
            let p = lean_core::process_block(&*state, wire, &par).expect("corpus block");
            s.process += ns(t);

            if check {
                assert_eq!(p.delta.writes, delta.writes, "process_block != phases");
                assert_eq!(p.commitment, f.commitment);
                let st = corpus.stats[bi];
                assert_eq!(p.delta.outcome.noops, 0, "corpus block {bi} has no-ops");
                assert_eq!(p.delta.outcome.applied as u64, st.txs);
                assert_eq!(p.delta.outcome.outputs as u64, st.payments);
            }
            undo.push(
                p.delta
                    .writes
                    .iter()
                    .map(|(a, _)| (*a, state.accounts.get(a).copied()))
                    .collect(),
            );
            let t = Instant::now();
            lean_core::apply(state, &p.delta);
            s.apply += ns(t);
            drop((items, delta, p));
        }
        for u in undo.into_iter().rev() {
            for (a, prev) in u {
                match prev {
                    Some(acct) => state.accounts.insert(a, acct),
                    None => state.accounts.remove(&a),
                };
            }
        }
        if run >= opts.warmup {
            per_run.push(s);
        }
    }

    let med = |f: fn(&Sums) -> f64| -> f64 {
        let mut v: Vec<f64> = per_run.iter().map(f).collect();
        median(&mut v) / nb
    };
    let totals: Vec<f64> = per_run.iter().map(|s| s.process + s.apply).collect();
    let mut pps: Vec<f64> = totals.iter().map(|t| payments as f64 / t * 1e9).collect();
    let mut tps: Vec<f64> = totals.iter().map(|t| txs as f64 / t * 1e9).collect();
    let serial_tot: Vec<f64> = per_run
        .iter()
        .map(|s| s.frame_decode + s.rec_ser + s.execute + s.apply)
        .collect();
    let mut pps_s: Vec<f64> = serial_tot
        .iter()
        .map(|t| payments as f64 / t * 1e9)
        .collect();
    let mut tps_s: Vec<f64> = serial_tot.iter().map(|t| txs as f64 / t * 1e9).collect();
    let total = med(|s| s.process + s.apply);
    let hotspots = opts.micro.then(|| micro(&corpus, state));
    ScenarioResult {
        scenario: sc.name.clone(),
        accounts: sc.accounts,
        fanout: sc.fanout.spec(),
        budget_gas: sc.budget,
        threads: rayon::current_num_threads(),
        blocks_per_run: corpus.blocks.len(),
        runs: opts.runs,
        warmup_runs: opts.warmup,
        txs_per_block: txs as f64 / nb,
        payments_per_block: payments as f64 / nb,
        bytes_per_block: corpus.stats.iter().map(|s| s.bytes).sum::<u64>() as f64 / nb,
        fullness: corpus.stats.iter().map(|s| s.gas as f64).sum::<f64>() / nb / sc.budget as f64,
        corpus_cached: cached,
        corpus_load_s,
        ns_per_block: PhaseNs {
            frame_decode: med(|s| s.frame_decode),
            recover_parallel: med(|s| s.rec_par),
            recover_serial: opts.serial.then(|| med(|s| s.rec_ser)),
            execute: med(|s| s.execute),
            apply: med(|s| s.apply),
            process_block: med(|s| s.process),
            total,
        },
        payments_per_s: median(&mut pps),
        tx_per_s: median(&mut tps),
        blocks_per_s: 1e9 / total,
        payments_per_s_serial_recovery: opts.serial.then(|| median(&mut pps_s)),
        tx_per_s_serial_recovery: opts.serial.then(|| median(&mut tps_s)),
        peak_rss_mb: peak_rss_mb(),
        hotspots,
    }
}

/// Single-threaded CPU attribution of the first corpus block, function by
/// function, each timed as one loop over the block (no sampling profiler
/// needed). Shares are of the sum, i.e. of the serial pipeline.
fn micro(corpus: &Corpus, state: &mut FlatState) -> Vec<Hotspot> {
    use secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
    use secp256k1::{Message, SECP256K1};
    let ctx = EngineCtx::new(CHAIN_ID, beneficiary()).with_parallel_recovery(false);
    let domain = lane_domain(CHAIN_ID);
    let wire = &corpus.blocks[0];
    let reps = 3;
    let mut out: Vec<(String, f64)> = Vec::new();
    let mut time = |name: &str, f: &mut dyn FnMut()| {
        let mut v: Vec<f64> = (0..reps)
            .map(|_| {
                let t = Instant::now();
                f();
                ns(t)
            })
            .collect();
        out.push((name.to_string(), median(&mut v)));
    };
    let (hdr, txs) = lean_core::scan(wire).unwrap();
    let parsed: Vec<LeanTx> = txs
        .iter()
        .map(|t| LeanTx::parse_after_type(&mut &t[1..]).unwrap())
        .collect();
    let msgs: Vec<[u8; 32]> = parsed.iter().map(|t| t.signing_hash(&domain).0).collect();
    let pks: Vec<secp256k1::PublicKey> = parsed
        .iter()
        .zip(&msgs)
        .map(|(t, m)| {
            let rec = RecoveryId::try_from(t.sig[64] as i32).unwrap();
            let sig = RecoverableSignature::from_compact(&t.sig[..64], rec).unwrap();
            SECP256K1
                .recover_ecdsa(&Message::from_digest(*m), &sig)
                .unwrap()
        })
        .collect();
    let items = lean_core::recover_txs(&txs, &ctx);
    let delta = lean_core::execute(&*state, &items, &ctx);
    let undo: Vec<(Address, Option<Acct>)> = delta
        .writes
        .iter()
        .map(|(a, _)| (*a, state.accounts.get(a).copied()))
        .collect();

    time("framing walk (engine::scan)", &mut || {
        black_box(lean_core::scan(black_box(wire)).unwrap());
    });
    time(
        "commitment keccak (framing::commitment_of_wire)",
        &mut || {
            black_box(lean_core::framing::commitment_of_wire(&hdr, wire));
        },
    );
    time("tx decode (LeanTx::parse_after_type)", &mut || {
        for t in &txs {
            debug_assert_eq!(t[0], LEAN_TX_TYPE);
            black_box(LeanTx::parse_after_type(&mut &t[1..]).unwrap());
        }
    });
    time("tx hash keccak (engine::decode_one)", &mut || {
        for t in &txs {
            black_box(keccak256(t));
        }
    });
    time(
        "signing hash: body re-encode + keccak (LeanTx::signing_hash)",
        &mut || {
            for t in &parsed {
                black_box(t.signing_hash(&domain));
            }
        },
    );
    time("ecrecover (secp256k1 recover_ecdsa)", &mut || {
        for (t, m) in parsed.iter().zip(&msgs) {
            let rec = RecoveryId::try_from(t.sig[64] as i32).unwrap();
            let sig = RecoverableSignature::from_compact(&t.sig[..64], rec).unwrap();
            black_box(
                SECP256K1
                    .recover_ecdsa(&Message::from_digest(*m), &sig)
                    .unwrap(),
            );
        }
    });
    time("pubkey -> address keccak (pubkey_to_address)", &mut || {
        for pk in &pks {
            black_box(pubkey_to_address(pk));
        }
    });
    time("execute (engine::execute, whole)", &mut || {
        black_box(lean_core::execute(&*state, &items, &ctx));
    });
    time("  of which: state lookups (FlatState::get)", &mut || {
        for (a, _) in &delta.writes {
            black_box(state.get(a));
        }
    });
    time("  of which: write-set sort", &mut || {
        let mut w = delta.writes.clone();
        w.reverse();
        w.sort_unstable_by_key(|(a, _)| *a);
        black_box(w);
    });
    // apply into the REAL state (the map size is the point), undone after
    // each repetition
    time("apply (FlatState insert)", &mut || {
        lean_core::apply(state, &delta);
    });
    for (a, prev) in &undo {
        match prev {
            Some(acct) => state.accounts.insert(*a, *acct),
            None => state.accounts.remove(a),
        };
    }
    let total: f64 = out
        .iter()
        .filter(|(n, _)| !n.starts_with("  "))
        .map(|(_, v)| v)
        .sum();
    out.into_iter()
        .map(|(function, v)| Hotspot {
            function,
            ns_per_block: v,
            pct: 100.0 * v / total,
        })
        .collect()
}

/// Build the state for `accounts`, reporting how long and how much RAM.
pub fn make_state(accounts: u64, senders: &[Address]) -> (FlatState, f64, f64) {
    let before = rss_mb();
    let t = Instant::now();
    let s = corpus::build_state(accounts, senders);
    (s, t.elapsed().as_secs_f64(), rss_mb() - before)
}
