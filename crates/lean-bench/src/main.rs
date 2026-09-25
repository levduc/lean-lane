//! `lean-bench` — the lean lane's standalone ceiling.
//!
//! Engine mode (default): lean-core's pipeline over cached, pre-signed,
//! budget-full corpora against an in-memory pre-funded state, phase by phase.
//!
//! ```text
//! lean-bench --all --json                 # every scenario, machine-readable
//! lean-bench --scenario 's1e6-n100-*'     # a subset (`*` wildcard)
//! lean-bench --list                       # scenario names
//! lean-bench e2e --seconds 10 [--no-fsync]  # two real nodes in-process
//! ```
//!
//! Scenario `s{accounts}-n{fanout}-g{budget}`: accounts in {1e5,1e6,1e7},
//! fanout in {1,10,100,mix (1:20,5:20,10:20,50:20,100:20)}, budget in
//! {225m,450m,1g} gas. Every block is FULL (not even the smallest tx fits).

mod corpus;
mod e2e;
mod engine_bench;
mod util;

use clap::{Parser, Subcommand};
use corpus::Fanout;
use engine_bench::{all_scenarios, glob, make_state, run_scenario, Opts, ScenarioResult};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "lean-bench",
    version,
    about = "Lean lane engine + e2e benchmark"
)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Run every scenario.
    #[arg(long)]
    all: bool,
    /// Scenario name or `*` pattern (repeatable).
    #[arg(long)]
    scenario: Vec<String>,
    /// List scenario names and exit.
    #[arg(long)]
    list: bool,
    #[command(flatten)]
    common: Common,
    /// Measured runs per scenario (median reported).
    #[arg(long, default_value_t = 5)]
    runs: usize,
    /// Warm-up runs per scenario (excluded).
    #[arg(long, default_value_t = 1)]
    warmup: usize,
    /// Consecutive blocks per run (each applied before the next).
    #[arg(long, default_value_t = 3)]
    blocks: usize,
    /// Skip the serial-recovery measurement.
    #[arg(long)]
    no_serial: bool,
    /// Add a single-threaded per-function CPU attribution of block 1.
    #[arg(long)]
    micro: bool,
}

#[derive(clap::Args, Clone)]
struct Common {
    /// rayon pool size (default: all CPUs the process may run on).
    #[arg(long, global = true)]
    threads: Option<usize>,
    /// Machine-readable JSON on stdout (progress stays on stderr).
    #[arg(long, global = true)]
    json: bool,
    /// Corpus seed.
    #[arg(long, default_value_t = 1, global = true)]
    seed: u64,
    /// Corpus cache (default: <target>/lean-bench-cache).
    #[arg(long, global = true)]
    cache_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Drive two real nodes in-process with the stand-in CL, unpaced.
    E2e(E2eArgs),
}

#[derive(clap::Args)]
struct E2eArgs {
    /// Measurement window (after one warm-up block).
    #[arg(long, default_value_t = 10.0)]
    seconds: f64,
    /// Do not fsync the block log.
    #[arg(long)]
    no_fsync: bool,
    /// Outputs per tx: a number, or `mix`.
    #[arg(long, default_value = "10")]
    fanout: String,
    /// Block budget in gas.
    #[arg(long, default_value_t = 225_000_000)]
    budget: u64,
    /// Accounts seeded into both nodes.
    #[arg(long, default_value_t = 100_000)]
    accounts: u64,
    /// Distinct senders in the pool fill (senders x depth must stay under
    /// the pool's 400k-tx cap).
    #[arg(long, default_value_t = 25_000)]
    senders: u64,
    /// Consecutive nonces per sender in the pool fill.
    #[arg(long, default_value_t = 15)]
    depth: u32,
}

#[derive(Serialize)]
struct StateInfo {
    accounts: u64,
    build_s: f64,
    rss_mb: f64,
}

#[derive(Serialize)]
struct Report {
    bench: &'static str,
    version: u32,
    cpus_available: usize,
    threads: usize,
    runs: usize,
    warmup_runs: usize,
    blocks_per_run: usize,
    seed: u64,
    states: Vec<StateInfo>,
    scenarios: Vec<ScenarioResult>,
}

fn parse_fanout(s: &str) -> eyre::Result<Fanout> {
    if s == "mix" {
        return Ok(Fanout::roadmap_mix());
    }
    Ok(Fanout::Fixed(s.parse()?))
}

fn main() -> eyre::Result<()> {
    let cli = Cli::parse();
    if let Some(n) = cli.common.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()?;
    }
    let cache_dir = cli
        .common
        .cache_dir
        .clone()
        .unwrap_or_else(util::default_cache_dir);
    match cli.cmd {
        Some(Cmd::E2e(a)) => {
            let o = e2e::E2eOpts {
                seconds: a.seconds,
                fsync: !a.no_fsync,
                fanout: parse_fanout(&a.fanout)?,
                budget: a.budget,
                accounts: a.accounts,
                senders: a.senders,
                depth: a.depth,
                seed: cli.common.seed,
                cache_dir,
            };
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let r = rt.block_on(e2e::run(o))?;
            if cli.common.json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                println!(
                    "e2e[{}] fanout={} budget={} blocks={} in {:.1}s: {:.2} blk/s, {:.0} tx/s, \
                     {:.0} payments/s (fullness {:.1}%)\n  build {:.1} ms · follower stage {:.1} ms · \
                     proposer anchor {:.1} ms · follower anchor {:.1} ms · append alone {:.2} ms \
                     (stopped by {})",
                    r.mode,
                    r.fanout,
                    r.budget_gas,
                    r.blocks,
                    r.seconds,
                    r.blocks_per_s,
                    r.tx_per_s,
                    r.payments_per_s,
                    r.fullness * 100.0,
                    r.build_ms.mean,
                    r.stage_ms.mean,
                    r.anchor_proposer_ms.mean,
                    r.anchor_follower_ms.mean,
                    r.append_ms.mean,
                    r.stopped_by
                );
            }
            Ok(())
        }
        None => engine(cli, cache_dir),
    }
}

fn engine(cli: Cli, cache_dir: PathBuf) -> eyre::Result<()> {
    let all = all_scenarios();
    if cli.list {
        for s in &all {
            println!("{}", s.name);
        }
        return Ok(());
    }
    let picked: Vec<_> = all
        .into_iter()
        .filter(|s| cli.all || cli.scenario.iter().any(|p| glob(p, &s.name)))
        .collect();
    eyre::ensure!(
        !picked.is_empty(),
        "no scenario selected: pass --all or --scenario <name|pattern> (see --list)"
    );
    let opts = Opts {
        runs: cli.runs.max(1),
        warmup: cli.warmup,
        blocks: cli.blocks.max(1),
        serial: !cli.no_serial,
        micro: cli.micro,
        seed: cli.common.seed,
        cache_dir,
    };
    eprintln!(
        "lean-bench: {} scenarios, {} rayon threads, {} runs (+{} warm-up) x {} blocks",
        picked.len(),
        rayon::current_num_threads(),
        opts.runs,
        opts.warmup,
        opts.blocks
    );
    let t0 = std::time::Instant::now();
    let senders = corpus::senders();
    eprintln!(
        "  derived {} sender keys in {:.2}s",
        senders.len(),
        t0.elapsed().as_secs_f64()
    );
    let mut states = Vec::new();
    let mut results = Vec::new();
    let mut i = 0;
    while i < picked.len() {
        let accounts = picked[i].accounts;
        let (mut state, build_s, rss) = make_state(accounts, &senders);
        eprintln!("  state {accounts} accounts: built in {build_s:.2}s, +{rss:.0} MB RSS");
        states.push(StateInfo {
            accounts,
            build_s,
            rss_mb: rss,
        });
        while i < picked.len() && picked[i].accounts == accounts {
            let r = run_scenario(&picked[i], &mut state, &senders, &opts);
            eprintln!(
                "  {:<20} {:>9.0} pay/s {:>8.0} tx/s  blk {:>7.2} ms (frame+dec {:.2} rec {:.2} \
                 exec {:.2} apply {:.2})  full {:.2}%  rss {:.0} MB{}",
                r.scenario,
                r.payments_per_s,
                r.tx_per_s,
                r.ns_per_block.total / 1e6,
                r.ns_per_block.frame_decode / 1e6,
                r.ns_per_block.recover_parallel / 1e6,
                r.ns_per_block.execute / 1e6,
                r.ns_per_block.apply / 1e6,
                r.fullness * 100.0,
                r.peak_rss_mb,
                if r.corpus_cached {
                    ""
                } else {
                    "  (corpus generated)"
                }
            );
            if let Some(h) = &r.hotspots {
                for x in h {
                    eprintln!(
                        "      {:>6.2}%  {:>10.0} ns  {}",
                        x.pct, x.ns_per_block, x.function
                    );
                }
            }
            results.push(r);
            i += 1;
        }
    }
    let report = Report {
        bench: "lean-bench",
        version: 1,
        cpus_available: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        threads: rayon::current_num_threads(),
        runs: opts.runs,
        warmup_runs: opts.warmup,
        blocks_per_run: opts.blocks,
        seed: opts.seed,
        states,
        scenarios: results,
    };
    if cli.common.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "{:<20} {:>12} {:>10} {:>10} {:>9} {:>9} {:>9} {:>9} {:>8}",
            "scenario",
            "payments/s",
            "tx/s",
            "blk ms",
            "fr+dec",
            "recover",
            "execute",
            "apply",
            "serial"
        );
        for r in &report.scenarios {
            let n = &r.ns_per_block;
            println!(
                "{:<20} {:>12.0} {:>10.0} {:>10.2} {:>9.2} {:>9.2} {:>9.2} {:>9.2} {:>8}",
                r.scenario,
                r.payments_per_s,
                r.tx_per_s,
                n.total / 1e6,
                n.frame_decode / 1e6,
                n.recover_parallel / 1e6,
                n.execute / 1e6,
                n.apply / 1e6,
                r.payments_per_s_serial_recovery
                    .map(|v| format!("{v:.0}"))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}
