//! THE HOT PATH: framing walk, tx decode, sender recovery, total-STF
//! execution and delta application. Everything between the EVOLVE markers
//! may be rewritten freely (an automated search edits only this region);
//! the public API, the types and the canonical commitment live outside it
//! (`lib.rs`, `types.rs`, `framing.rs`) and call in through these functions:
//!
//! ```text
//! scan(wire)                 -> (header, tx slices)       framing only, no hashing
//! decode(txs, ctx)           -> Vec<Decoded>              skip what does not decode
//! recover(decoded, ctx)      -> Vec<RecoveredTx>          skip what does not recover
//! execute(state, items, ctx) -> Delta                     pure, total STF
//! apply(state, delta)
//! process_block(state, wire, ctx) -> Processed            the whole pipeline
//! ```
//!
//! CONTRACT (consensus — every rewrite must keep it bit-for-bit; the tests in
//! `crates/lean-core/tests/` and the node's replay-digest check enforce it):
//!
//! * FRAMING: `wire = [parent 32][number u64][ts u64][n u32]([len u32][tx])*`,
//!   little-endian; a cut-off length/body or trailing bytes refuse the block
//!   with the matching `BlockError`, checked in tx order.
//! * DECODE: an entry is a lean tx iff its first byte is `0x50` and
//!   `LeanTx::parse_after_type` accepts the rest. Bytes after the parsed tx
//!   are IGNORED (EIP-2718 `decode_2718` semantics) and the tx hash is
//!   keccak256 of the consumed prefix only. Anything else is skipped.
//! * RECOVER: sender = `recover_sender(ctx.domain)`; a failure skips the tx.
//!   Output order = block order of the surviving entries, serial or parallel.
//! * EXECUTE: the total STF documented on `lean_native` — whole-tx atomic,
//!   nonce must equal, `fee + Σ amount·AMOUNT_UNIT` (checked add) must be
//!   covered by block-start balance minus own earlier debits, same-block
//!   credits merged after the debit phase, fees to the beneficiary as one
//!   credit at end. Write set = every sender (no-op senders too, at their
//!   unchanged value) ∪ recipients of applied txs ∪ beneficiary, sorted by
//!   address. `evict` = every item's hash in order.
//! * APPLY: insert every write.

// EVOLVE-BLOCK-START

use crate::framing::{self, WIRE_HEADER_LEN};
use crate::types::{
    Acct, BlockError, BlockHeader, BlockOutcome, Decoded, Delta, EngineCtx, FlatState, Processed,
    RecoveredTx, StateView,
};
use alloy_primitives::{keccak256, Address, B256};
use lean_native::{lean_fee, LeanTx, AMOUNT_UNIT, LEAN_TX_TYPE};
use rayon::prelude::*;
use std::collections::HashMap;

#[inline]
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

#[inline]
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// Validate the wire framing; return the header and the tx bodies borrowed
/// from `wire`. No hashing, no copies.
pub(crate) fn scan(wire: &[u8]) -> Result<(BlockHeader, Vec<&[u8]>), BlockError> {
    if wire.len() < WIRE_HEADER_LEN {
        return Err(BlockError::TooShort);
    }
    let header = BlockHeader {
        parent: B256::from_slice(&wire[0..32]),
        number: u64_at(wire, 32),
        timestamp_ms: u64_at(wire, 40),
    };
    let n_tx = u32_at(wire, 48) as usize;
    let mut txs = Vec::with_capacity(n_tx.min(wire.len() / 4));
    let mut at = WIRE_HEADER_LEN;
    for i in 0..n_tx {
        if wire.len() < at + 4 {
            return Err(BlockError::TruncatedLength(i));
        }
        let len = u32_at(wire, at) as usize;
        at += 4;
        if wire.len() < at + len {
            return Err(BlockError::TruncatedBody(i));
        }
        txs.push(&wire[at..at + len]);
        at += len;
    }
    if at != wire.len() {
        return Err(BlockError::TrailingBytes);
    }
    Ok((header, txs))
}

/// Decode one entry (see CONTRACT: DECODE).
#[inline]
fn decode_one(bytes: &[u8]) -> Option<Decoded> {
    if bytes.first() != Some(&LEAN_TX_TYPE) {
        return None;
    }
    let mut buf = &bytes[1..];
    let tx = LeanTx::parse_after_type(&mut buf).ok()?;
    let consumed = bytes.len() - buf.len();
    Some(Decoded {
        tx,
        hash: keccak256(&bytes[..consumed]),
    })
}

/// Decode a tx list in order, skipping what does not decode.
pub(crate) fn decode<T: AsRef<[u8]> + Sync>(txs: &[T], ctx: &EngineCtx) -> Vec<Decoded> {
    if ctx.parallel_recovery {
        txs.par_iter()
            .map(|t| decode_one(t.as_ref()))
            .collect::<Vec<_>>()
            .into_iter()
            .flatten()
            .collect()
    } else {
        txs.iter().filter_map(|t| decode_one(t.as_ref())).collect()
    }
}

/// Recover senders in order, skipping what does not recover. Parallel mode
/// runs on the rayon pool; the indexed collect keeps block order.
pub(crate) fn recover(decoded: Vec<Decoded>, ctx: &EngineCtx) -> Vec<RecoveredTx> {
    let domain = ctx.domain;
    let one = move |d: Decoded| -> Option<RecoveredTx> {
        let sender = d.tx.recover_sender(&domain).ok()?;
        Some((sender, d.tx, d.hash))
    };
    if ctx.parallel_recovery {
        decoded
            .into_par_iter()
            .map(one)
            .collect::<Vec<_>>()
            .into_iter()
            .flatten()
            .collect()
    } else {
        decoded.into_iter().filter_map(one).collect()
    }
}

/// THE state transition function: execute `items` (block order) against
/// `state` and return the delta. Pure — `state` is only read.
pub(crate) fn execute<S: StateView + ?Sized>(
    state: &S,
    items: &[RecoveredTx],
    ctx: &EngineCtx,
) -> Delta {
    let beneficiary = ctx.beneficiary;
    // Post view: block-start values, own debits accumulate, incoming
    // same-block credits invisible to the debit checks (deterministic-parallel
    // rule) — they are only merged in after the debit phase.
    let mut post: HashMap<Address, Acct> = HashMap::new();
    let mut credits: HashMap<Address, u128> = HashMap::new();
    let mut fees = 0u128;
    let mut applied = 0usize;
    let mut outputs = 0usize;

    for (sender, tx, _) in items {
        let entry = post.entry(*sender).or_insert_with(|| state.get(sender));
        let fee = lean_fee(tx.outputs.len());
        let mut total = fee;
        let mut ok = tx.nonce as u64 == entry.nonce;
        if ok {
            for o in &tx.outputs {
                match total.checked_add(o.amount as u128 * AMOUNT_UNIT) {
                    Some(t) => total = t,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
        }
        if !ok || entry.balance < total {
            continue;
        }
        entry.balance -= total;
        entry.nonce += 1;
        for o in &tx.outputs {
            *credits.entry(o.to).or_default() += o.amount as u128 * AMOUNT_UNIT;
        }
        outputs += tx.outputs.len();
        fees += fee;
        applied += 1;
    }

    for (addr, wei) in credits {
        post.entry(addr).or_insert_with(|| state.get(&addr)).balance += wei;
    }
    post.entry(beneficiary)
        .or_insert_with(|| state.get(&beneficiary))
        .balance += fees;

    let mut writes: Vec<(Address, Acct)> = post.into_iter().collect();
    writes.sort_unstable_by_key(|(a, _)| *a);
    Delta {
        writes,
        evict: items.iter().map(|(_, _, h)| *h).collect(),
        outcome: BlockOutcome {
            applied,
            noops: items.len() - applied,
            outputs,
            fees_to_beneficiary: fees,
        },
    }
}

/// Write a delta into the state.
pub(crate) fn apply(state: &mut FlatState, delta: &Delta) {
    for (a, acct) in &delta.writes {
        state.accounts.insert(*a, *acct);
    }
}

/// The whole pipeline over one wire block.
pub(crate) fn process_block<S: StateView + ?Sized>(
    state: &S,
    wire: &[u8],
    ctx: &EngineCtx,
) -> Result<Processed, BlockError> {
    let (header, txs) = scan(wire)?;
    let commitment = framing::commitment_of_wire(&header, wire);
    let items = recover(decode(&txs, ctx), ctx);
    let delta = execute(state, &items, ctx);
    Ok(Processed {
        header,
        commitment,
        delta,
    })
}

// EVOLVE-BLOCK-END
