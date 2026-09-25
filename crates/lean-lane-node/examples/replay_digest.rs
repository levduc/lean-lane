//! Replay a datadir and print the head it reaches plus the state digest.
//!
//! `cargo run --release -p lean-lane-node --example replay_digest -- <datadir> [chain_id]`
//!
//! Opens the datadir exactly as the node does (newest snapshot + log-tail
//! replay through the engine) and prints one line:
//! `head=<n> commitment=<c> digest=<d> accounts=<k> replayed_blocks=<b> replayed_txs=<t>`.
//! Run it on a COPY: `open` truncates a torn log tail in place.
//!
//! Used to prove an execution refactor is behaviour-preserving: replay the
//! same log with the old and the new build and compare the line.

use lean_lane_node::node::{Config, LaneNode};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let mut args = std::env::args().skip(1);
    let datadir = args
        .next()
        .ok_or_else(|| eyre::eyre!("usage: replay_digest <datadir> [chain_id]"))?;
    let chain_id = args.next().map(|s| s.parse()).transpose()?.unwrap_or(1338);
    let cfg = Config {
        datadir: datadir.into(),
        chain_id,
        snapshot_every: 0,
        fsync: false,
        ..Default::default()
    };
    let (node, rec) = LaneNode::open(cfg)?;
    let (c, n, _) = *node.head.lock().await;
    let state = node.state.lock().await;
    println!(
        "head={n} commitment={c} digest={} accounts={} replayed_blocks={} replayed_txs={}",
        state.digest(),
        state.accounts.len(),
        rec.replayed_blocks,
        rec.replayed_txs
    );
    Ok(())
}
