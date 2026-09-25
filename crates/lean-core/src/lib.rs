//! The lean payment lane's block-processing engine: pure, synchronous,
//! I/O-free. No tokio, no reth, no filesystem — bytes and a state view in, a
//! delta out. The node (`lean-lane-node`) runs every execution path through
//! it (stage, build, commit/promote, replay); `lean-bench` measures it.
//!
//! Pipeline: framing parse -> tx decode -> sender recovery (rayon,
//! order-preserving) -> total-STF execute. The hot path is one file,
//! `engine.rs`, between `EVOLVE-BLOCK-START/END` markers; this file holds only
//! the public signatures, which delegate to it.
//!
//! Wire format and signing are `lean_native`'s; the commitment definition is
//! [`framing`]'s. Neither is inside the optimizable region.

mod engine;
pub mod framing;
mod types;

pub use framing::genesis_commitment;
pub use types::{
    AccountMap, Acct, AddressHasher, AddressHasherBuilder, BlockError, BlockHeader, BlockOutcome,
    Decoded, Delta, EngineCtx, FlatState, Framed, Processed, RecoveredTx, StateView,
};

use alloy_primitives::B256;

/// Process one wire block against `state`: validate the framing, compute the
/// commitment, decode and recover every tx, execute. `state` is only read;
/// apply the returned delta with [`apply`].
pub fn process_block(
    state: &impl StateView,
    block: &[u8],
    ctx: &EngineCtx,
) -> Result<Processed, BlockError> {
    engine::process_block(state, block, ctx)
}

/// Validate a wire block's framing: header plus borrowed tx bodies. No
/// hashing.
pub fn scan(block: &[u8]) -> Result<(BlockHeader, Vec<&[u8]>), BlockError> {
    engine::scan(block)
}

/// [`scan`] plus the commitment.
pub fn frame(block: &[u8]) -> Result<Framed<'_>, BlockError> {
    let (header, txs) = engine::scan(block)?;
    Ok(Framed {
        header,
        commitment: framing::commitment_of_wire(&header, block),
        txs,
    })
}

/// The header and commitment of a wire block (framing validated, tx bodies
/// not materialised).
pub fn header_and_commitment(block: &[u8]) -> Result<(BlockHeader, B256), BlockError> {
    let f = frame(block)?;
    Ok((f.header, f.commitment))
}

/// Decode a tx list in block order; entries that do not decode are skipped.
pub fn decode_txs<T: AsRef<[u8]> + Sync>(txs: &[T], ctx: &EngineCtx) -> Vec<Decoded> {
    engine::decode(txs, ctx)
}

/// Recover the senders of decoded txs in block order; failures are skipped.
pub fn recover(decoded: Vec<Decoded>, ctx: &EngineCtx) -> Vec<RecoveredTx> {
    engine::recover(decoded, ctx)
}

/// [`decode_txs`] + [`recover`]: a block's raw tx list to recovered items.
pub fn recover_txs<T: AsRef<[u8]> + Sync>(txs: &[T], ctx: &EngineCtx) -> Vec<RecoveredTx> {
    engine::recover(engine::decode(txs, ctx), ctx)
}

/// THE state transition function over already-recovered items (block
/// order). Pure: `state` is only read.
pub fn execute(state: &impl StateView, items: &[RecoveredTx], ctx: &EngineCtx) -> Delta {
    engine::execute(state, items, ctx)
}

/// Write a delta into the state.
pub fn apply(state: &mut FlatState, delta: &Delta) {
    engine::apply(state, delta)
}

/// [`execute`] + [`apply`] (replay and tests).
pub fn apply_block(state: &mut FlatState, items: &[RecoveredTx], ctx: &EngineCtx) -> BlockOutcome {
    let delta = engine::execute(&*state, items, ctx);
    engine::apply(state, &delta);
    delta.outcome
}
