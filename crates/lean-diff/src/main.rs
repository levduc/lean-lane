//! `lean-diff` — run the engine correctness checks at a chosen budget.
//!
//! ```text
//! lean-diff diff    --chains 5000 --blocks 20 [--seed S] [--rates 'k=v,…'] [--only-chain I]
//! lean-diff fuzz    --iters 1000000 [--seed S] [--from I]
//! lean-diff restart --chains 200 --blocks 30 [--seed S]
//! lean-diff golden
//! ```
//!
//! Each prints ONE JSON line on stdout (`{"check": …, "ok": true|false, …}`)
//! and exits non-zero on a failure; the reproducer goes to stderr and into
//! the JSON's `failure` field.

use clap::{Parser, Subcommand};
use lean_diff::gen::Rates;
use lean_diff::{diff, fuzz, golden, restart};
use serde_json::json;

#[derive(Parser)]
#[command(name = "lean-diff", about = "lean-core engine correctness checks")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Worker threads (default: LEAN_DIFF_THREADS or all CPUs).
    #[arg(long, global = true)]
    threads: Option<usize>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Differential test engine vs oracle.
    Diff {
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long, default_value_t = 200)]
        chains: u64,
        #[arg(long, default_value_t = 20)]
        blocks: u64,
        /// Rate overrides, e.g. `valid=10,bad_sig=0,empty_block=0.2`.
        #[arg(long)]
        rates: Option<String>,
        /// Run only this chain index (reproduce a failure).
        #[arg(long)]
        only_chain: Option<u64>,
    },
    /// Mutational decode fuzzer.
    Fuzz {
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long, default_value_t = 100_000)]
        iters: u64,
        /// First iteration index (reproduce a failure with `--iters 1`).
        #[arg(long, default_value_t = 0)]
        from: u64,
    },
    /// Snapshot/replay/restart equivalence.
    Restart {
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long, default_value_t = 50)]
        chains: u64,
        #[arg(long, default_value_t = 30)]
        blocks: u64,
    },
    /// SPEC.md Appendix A vectors against the engine.
    Golden,
}

fn main() {
    let cli = Cli::parse();
    let threads = cli.threads.unwrap_or_else(lean_diff::util::threads);
    let (ok, line) = match cli.cmd {
        Cmd::Diff {
            seed,
            chains,
            blocks,
            rates,
            only_chain,
        } => {
            let rates = match rates
                .map(|r| Rates::parse(&r))
                .unwrap_or_else(Rates::from_env)
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(2);
                }
            };
            let cfg = diff::DiffCfg {
                seed,
                chains,
                blocks,
                rates,
                only_chain,
                threads,
            };
            match diff::run(&cfg) {
                Ok(st) => {
                    eprintln!("diff: OK {}", diff::summary_line(&st));
                    (
                        true,
                        json!({"check": "diff", "ok": true, "seed": seed, "stats": st}),
                    )
                }
                Err(m) => {
                    eprintln!("{m}");
                    (
                        false,
                        json!({"check": "diff", "ok": false, "seed": seed, "chain": m.chain,
                               "block": m.block, "failure": m.summary, "report": m.report}),
                    )
                }
            }
        }
        Cmd::Fuzz { seed, iters, from } => match fuzz::run(seed, from, iters, threads) {
            Ok(st) => {
                eprintln!(
                    "fuzz: OK {} iterations ({} tx, {} block inputs; {} recoveries compared, \
                     {} recovered) in {:.1}s",
                    st.iterations,
                    st.tx_inputs,
                    st.block_inputs,
                    st.recoveries_compared,
                    st.recovered,
                    st.seconds
                );
                (
                    true,
                    json!({"check": "fuzz", "ok": true, "seed": seed, "stats": st}),
                )
            }
            Err(f) => {
                eprintln!("{f}");
                (
                    false,
                    json!({"check": "fuzz", "ok": false, "seed": seed, "iteration": f.iteration,
                           "failure": f.to_string()}),
                )
            }
        },
        Cmd::Restart {
            seed,
            chains,
            blocks,
        } => {
            let rates = Rates::from_env().unwrap_or_default();
            match restart::run(seed, chains, blocks, &rates) {
                Ok(st) => {
                    eprintln!(
                        "restart: OK {} chains x {} blocks, {} restarts in {:.1}s",
                        st.chains, blocks, st.restarts, st.seconds
                    );
                    (true, json!({"check": "restart", "ok": true, "stats": st}))
                }
                Err(e) => {
                    eprintln!("{e}");
                    (
                        false,
                        json!({"check": "restart", "ok": false, "failure": e}),
                    )
                }
            }
        }
        Cmd::Golden => match golden::check_all() {
            Ok(n) => {
                eprintln!("golden: OK {n} assertions");
                (
                    true,
                    json!({"check": "golden", "ok": true, "assertions": n}),
                )
            }
            Err(e) => {
                eprintln!("golden: FAIL {e}");
                (false, json!({"check": "golden", "ok": false, "failure": e}))
            }
        },
    };
    println!("{line}");
    if !ok {
        std::process::exit(1);
    }
}
