//! Replay/restart equivalence (SPEC.md §5): every path that reaches height h
//! must give the same digest. Paths checked here, per generated chain:
//!
//! 1. straight: `process_block` + `apply`, block by block (parallel recovery);
//! 2. replay: `frame` + `recover_txs` + `apply_block` — the node's
//!    `replay_onto` path — (serial recovery);
//! 3. restart: at 1–3 random cut heights, the straight state is written with
//!    the node's own snapshot writer, read back with its loader (hash-map
//!    order differs), and the rest of the chain is applied onto it.
//!
//! All three are also checked against the oracle's digest at every height.
//! The full node-level restart (LaneNode::open recovery, staged+promoted and
//! direct commits) is `tests/node_restart.rs`.

use crate::diff::genesis_pair;
use crate::gen::{ChainGen, GenBlock, Rates};
use crate::norm::oracle_block;
use crate::rng::{mix, Rng};
use crate::util::ctx;
use crate::CHAIN_ID;
use alloy_primitives::B256;
use lean_core::FlatState;
use lean_lane_node::state::Snapshots;
use lean_oracle::{state_digest, State};
use serde::Serialize;

/// A generated chain with the oracle's view of it.
pub struct Generated {
    pub fund_file: String,
    pub fund_balance: u128,
    pub genesis_oracle: State,
    pub genesis_engine: FlatState,
    pub blocks: Vec<GenBlock>,
    /// Per block: `Some((commitment, number, ts, post-digest))` if the oracle
    /// accepted it, `None` if the framing was rejected.
    pub oracle: Vec<Option<Expected>>,
}

/// `(commitment, number, timestamp_ms, post-state digest)` of an accepted block.
pub type Expected = ([u8; 32], u64, u64, [u8; 32]);

/// Generate a chain (the oracle is the model and records the expected
/// commitment/digest after every block).
pub fn generate(seed: u64, blocks: u64, rates: &Rates) -> Result<Generated, String> {
    let mut gen = ChainGen::new(seed, rates.clone());
    let (genesis_oracle, genesis_engine) = genesis_pair(&gen)?;
    let mut state = genesis_oracle.clone();
    let mut head = (lean_oracle::genesis_commitment(CHAIN_ID), 0u64);
    let mut out = Vec::new();
    let mut expect = Vec::new();
    for _ in 0..blocks {
        let blk = gen.next_block(&state, head.0, head.1 + 1);
        let (n, writes) = oracle_block(&state, &blk.wire);
        match (n.accepted(), writes) {
            (Some(a), Some(w)) => {
                lean_oracle::stf::apply_writes(&mut state, &w);
                head = (a.commitment, a.number);
                expect.push(Some((
                    a.commitment,
                    a.number,
                    a.timestamp_ms,
                    state_digest(&state),
                )));
            }
            _ => expect.push(None),
        }
        out.push(blk);
    }
    Ok(Generated {
        fund_file: gen.fund_file,
        fund_balance: gen.fund_balance,
        genesis_oracle,
        genesis_engine,
        blocks: out,
        oracle: expect,
    })
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RestartStats {
    pub chains: u64,
    pub blocks: u64,
    pub restarts: u64,
    pub seconds: f64,
}

/// Run `chains` chains of `blocks` blocks.
pub fn run(seed: u64, chains: u64, blocks: u64, rates: &Rates) -> Result<RestartStats, String> {
    let t0 = std::time::Instant::now();
    let mut st = RestartStats::default();
    for c in 0..chains {
        st.restarts += check_chain(mix(seed ^ 0x5e57, c), blocks, rates)
            .map_err(|e| format!("restart seed={seed} chain={c}: {e}"))?;
        st.chains += 1;
        st.blocks += blocks;
    }
    st.seconds = t0.elapsed().as_secs_f64();
    Ok(st)
}

fn apply_straight(state: &mut FlatState, wire: &[u8]) -> Option<B256> {
    let p = lean_core::process_block(&*state, wire, &ctx(true)).ok()?;
    lean_core::apply(state, &p.delta);
    Some(p.commitment)
}

fn apply_replay(state: &mut FlatState, wire: &[u8]) -> Option<B256> {
    let f = lean_core::frame(wire).ok()?;
    let items = lean_core::recover_txs(&f.txs, &ctx(false));
    lean_core::apply_block(state, &items, &ctx(false));
    Some(f.commitment)
}

/// Check one chain; returns the number of restarts exercised.
pub fn check_chain(seed: u64, blocks: u64, rates: &Rates) -> Result<u64, String> {
    let g = generate(seed, blocks, rates)?;
    let mut rng = Rng::new(seed ^ 0xc07);
    let cuts: Vec<usize> = (0..rng.usize(1, 3))
        .map(|_| rng.usize(0, g.blocks.len()))
        .collect();
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;

    let mut straight = g.genesis_engine.clone();
    let mut replay = g.genesis_engine.clone();
    // Restarted states (one per cut), created when their cut height is reached.
    let mut restarted: Vec<(usize, FlatState)> = Vec::new();
    let mut head = (lean_core::genesis_commitment(CHAIN_ID), 0u64, 0u64);
    for (i, blk) in g.blocks.iter().enumerate() {
        for (k, &cut) in cuts.iter().enumerate() {
            if cut == i {
                let sub = dir.path().join(format!("cut{k}"));
                std::fs::create_dir_all(&sub).map_err(|e| e.to_string())?;
                straight
                    .write_snapshot(&sub, head.0, head.1, head.2)
                    .map_err(|e| e.to_string())?;
                let (s, c, n, t) = FlatState::load_newest_snapshot(&sub)
                    .map_err(|e| e.to_string())?
                    .ok_or("snapshot vanished")?;
                if (c, n, t) != head || s.digest() != straight.digest() {
                    return Err(format!("snapshot round-trip at height {} differs", head.1));
                }
                restarted.push((cut, s));
            }
        }
        let cs = apply_straight(&mut straight, &blk.wire);
        let cr = apply_replay(&mut replay, &blk.wire);
        let mut crs = Vec::new();
        for (_, s) in restarted.iter_mut() {
            crs.push(apply_straight(s, &blk.wire));
        }
        let want = g.oracle[i];
        let got_c = cs.map(|c| c.0);
        if got_c != want.map(|w| w.0) || cr.map(|c| c.0) != got_c {
            return Err(format!(
                "block {i}: commitment/verdict straight={cs:?} replay={cr:?} oracle={:?}",
                want.map(|w| hex::encode(w.0))
            ));
        }
        if crs.iter().any(|c| c.map(|c| c.0) != got_c) {
            return Err(format!("block {i}: restarted path verdict differs"));
        }
        let d = straight.digest().0;
        let od = match want {
            Some(w) => {
                head = (B256::from(w.0), w.1, w.2);
                w.3
            }
            None => d,
        };
        if d != od {
            return Err(format!(
                "block {i}: straight digest 0x{} vs oracle 0x{}",
                hex::encode(d),
                hex::encode(od)
            ));
        }
        if replay.digest().0 != d {
            return Err(format!(
                "block {i}: replay-path digest differs from straight"
            ));
        }
        for (cut, s) in &restarted {
            if s.digest().0 != d {
                return Err(format!(
                    "block {i}: state restarted from the snapshot at block {cut} differs"
                ));
            }
        }
    }
    Ok(cuts.len() as u64)
}
