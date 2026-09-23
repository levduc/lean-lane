//! Lean lane node binary. `lean-lane-node run --help` lists every flag.

use alloy_primitives::Address;
use clap::{Parser, Subcommand};
use lean_lane_node::node::{Config, LaneNode};
use lean_lane_node::rpc;
use lean_lane_node::state::Acct;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "lean-lane-node",
    version,
    about = "Lean payment-lane execution node"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the node.
    Run(RunArgs),
}

#[derive(clap::Args)]
struct RunArgs {
    /// Data directory (block log + state snapshots).
    #[arg(long, default_value = "lean-lane-data")]
    datadir: std::path::PathBuf,
    /// RPC listen port.
    #[arg(long, default_value_t = 8560)]
    port: u16,
    /// RPC listen address.
    #[arg(long, default_value = "127.0.0.1")]
    bind: std::net::IpAddr,
    /// Lane chain id (signing domain + genesis commitment).
    #[arg(long, default_value_t = 1338)]
    chain_id: u64,
    /// Blocks between state snapshots; 0 = never (recovery replays the log).
    #[arg(long, default_value_t = 1024)]
    snapshot_every: u64,
    /// Comma-separated peer lean-node RPC urls (backfill + announce targets).
    #[arg(long, value_delimiter = ',')]
    peers: Vec<String>,
    /// One 0x-address per line, each funded with --fund-balance. Applied only
    /// to a fresh datadir (recovered state wins).
    #[arg(long)]
    fund_file: Option<std::path::PathBuf>,
    /// Genesis balance per funded account, in wei.
    #[arg(long, default_value_t = 1_000_000_000_000_000)]
    fund_balance: u128,
    /// Accepted and ignored: the consensus layer always drives the chain.
    #[arg(long, hide = true)]
    shim: bool,
    /// Do not fsync the block log after each append (a crash may lose the
    /// log tail; peers backfill it).
    #[arg(long)]
    no_fsync: bool,
    /// Log a `lean_stats` line (counters and sizes) at info every 10 s.
    #[arg(long, env = "LEAN_STATS", value_parser = clap::builder::BoolishValueParser::new())]
    stats: bool,
    /// Wait for our own CL's anchor before pulling an announced head+1 (ms).
    #[arg(long, default_value_t = 250, env = "LEAN_ANNOUNCE_GRACE_MS")]
    announce_grace_ms: u64,
    /// Wait for a not-yet-arrived stage before an anchor pulls from a peer (ms).
    #[arg(long, default_value_t = 60, env = "LEAN_ANCHOR_GRACE_MS")]
    anchor_grace_ms: u64,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    // RUST_LOG overrides; default: info for this node, warn for dependencies.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,lean_lane_node=info")),
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();
    match Cli::parse().cmd {
        Cmd::Run(a) => run(a).await,
    }
}

async fn run(args: RunArgs) -> eyre::Result<()> {
    for gone in [
        "LEAN_FSYNC",
        "LEAN_STAGE_RACE_MS",
        "LEAN_PARALLEL_RECOVERY",
        "ARC_LEAN_SHIM",
    ] {
        if std::env::var_os(gone).is_some() {
            tracing::warn!("{gone} is no longer read (see `lean-lane-node run --help`)");
        }
    }
    let cfg = Config {
        datadir: args.datadir,
        chain_id: args.chain_id,
        snapshot_every: args.snapshot_every,
        peers: args
            .peers
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        fsync: !args.no_fsync,
        stats: args.stats,
        announce_grace_ms: args.announce_grace_ms,
        anchor_grace_ms: args.anchor_grace_ms,
        ..Default::default()
    };
    let port = args.port;
    let fund_balance = args.fund_balance;
    if !cfg.fsync {
        tracing::info!(
            "--no-fsync: the block-log append does NOT fsync (a crash may lose the log tail; \
             peers backfill it and the CL certificate binds the bytes, so no fork)"
        );
    }
    let (node, rec) = LaneNode::open(cfg)?;
    let node = Arc::new(node);
    node.spawn_sync();
    // --stats: one `lean_stats ...` line every 10 s (see stats_line).
    node.spawn_stats();
    tracing::info!(
        "recovered: snapshot@{} + {} blocks / {} txs replayed in {:?}",
        rec.snapshot_number,
        rec.replayed_blocks,
        rec.replayed_txs,
        rec.elapsed
    );
    if let Some(fund_file) = args
        .fund_file
        .filter(|_| rec.replayed_blocks == 0 && rec.snapshot_number == 0)
    {
        let mut n = 0usize;
        for line in std::fs::read_to_string(&fund_file)?.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let a: Address = line
                .parse()
                .map_err(|e| eyre::eyre!("fund-file '{line}': {e}"))?;
            node.seed_account(
                a,
                Acct {
                    nonce: 0,
                    balance: fund_balance,
                },
            )
            .await;
            n += 1;
        }
        // Persist the seeded genesis (recovery gotcha: unsnapshotted seeds
        // silently diverge on restart).
        let (c, num, t) = *node.head.lock().await;
        node.state
            .lock()
            .await
            .write_snapshot(&node.cfg.datadir, c, num, t)?;
        tracing::info!(
            "funded {n} accounts @ {fund_balance} wei from {}",
            fund_file.display()
        );
    }
    let (addr, _handle) = rpc::serve(node.clone(), (args.bind, port).into()).await?;
    tracing::info!("rpc: http://{addr}");
    tokio::signal::ctrl_c().await?;
    Ok(())
}
