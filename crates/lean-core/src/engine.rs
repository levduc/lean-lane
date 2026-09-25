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
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Sort key of an address: its 20 bytes as big-endian integers, so the
/// derived (lexicographic-by-field) order IS the byte order of the address
/// (SPEC §0) — cheaper to compare than a 20-byte memcmp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct AKey(u64, u64, u32);

impl AKey {
    #[inline]
    fn of(a: &Address) -> Self {
        let b = &a.0 .0;
        AKey(
            u64::from_be_bytes(b[0..8].try_into().unwrap()),
            u64::from_be_bytes(b[8..16].try_into().unwrap()),
            u32::from_be_bytes(b[16..20].try_into().unwrap()),
        )
    }

    #[inline]
    fn addr(self) -> Address {
        let mut b = [0u8; 20];
        b[0..8].copy_from_slice(&self.0.to_be_bytes());
        b[8..16].copy_from_slice(&self.1.to_be_bytes());
        b[16..20].copy_from_slice(&self.2.to_be_bytes());
        Address::new(b)
    }
}

/// One deferred credit: recipient key and amount in AMOUNT_UNIT.
#[derive(Clone, Copy)]
struct Credit {
    key: AKey,
    amount: u64,
}

/// The post-state of one sender after its txs of the block ran.
struct SenderPost {
    key: AKey,
    acct: Acct,
    applied: usize,
    outputs: usize,
    fees: u128,
}

/// THE state transition function: execute `items` (block order) against
/// `state` and return the delta. Pure — `state` is only read.
///
/// Sender-partitioned: a sender's in-block balance depends only on its own
/// earlier txs (same-block credits are not spendable), so the items are
/// grouped by sender (sort by (sender, block index)), each group runs in
/// block order from the block-start value, groups are independent. The
/// credits of applied txs are then sorted by recipient and summed per run,
/// and senders ∪ recipients ∪ beneficiary are merged in address order — the
/// write set comes out sorted without a map. `+=` on balances keeps the
/// reference's overflow semantics; wrapping addition is commutative and
/// associative, so the summation order cannot change a result. The same
/// algorithm runs on the rayon pool or on the calling thread
/// (`ctx.parallel_recovery`); only the executor differs.
pub(crate) fn execute<S: StateView + ?Sized>(
    state: &S,
    items: &[RecoveredTx],
    ctx: &EngineCtx,
) -> Delta {
    let par = ctx.parallel_recovery;
    let beneficiary = ctx.beneficiary;

    // 1. group by sender, block order inside a group ((key, index) is unique,
    //    so the unstable sort is deterministic)
    let mut order: Vec<(AKey, usize)> = items
        .iter()
        .enumerate()
        .map(|(i, (s, _, _))| (AKey::of(s), i))
        .collect();
    if par {
        order.par_sort_unstable();
    } else {
        order.sort_unstable();
    }
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let mut at = 0;
    while at < order.len() {
        let k = order[at].0;
        let mut end = at + 1;
        while end < order.len() && order[end].0 == k {
            end += 1;
        }
        groups.push((at, end));
        at = end;
    }

    // 2. run every sender's txs in block order from its block-start value
    let applied_flags: Vec<AtomicBool> = (0..items.len()).map(|_| AtomicBool::new(false)).collect();
    let run_group = |&(lo, hi): &(usize, usize)| -> SenderPost {
        let key = order[lo].0;
        let (sender, _, _) = &items[order[lo].1];
        let mut acct = state.get(sender);
        let (mut applied, mut outputs, mut fees) = (0usize, 0usize, 0u128);
        for &(_, i) in &order[lo..hi] {
            let tx = &items[i].1;
            let fee = lean_fee(tx.outputs.len());
            let mut total = fee;
            let mut ok = tx.nonce as u64 == acct.nonce;
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
            if !ok || acct.balance < total {
                continue;
            }
            acct.balance -= total;
            acct.nonce += 1;
            applied_flags[i].store(true, Ordering::Relaxed);
            outputs += tx.outputs.len();
            fees += fee;
            applied += 1;
        }
        SenderPost {
            key,
            acct,
            applied,
            outputs,
            fees,
        }
    };
    let senders: Vec<SenderPost> = if par {
        groups.par_iter().map(run_group).collect()
    } else {
        groups.iter().map(run_group).collect()
    };
    let (mut applied, mut outputs, mut fees) = (0usize, 0usize, 0u128);
    for s in &senders {
        applied += s.applied;
        outputs += s.outputs;
        fees += s.fees;
    }

    // 3. credits of applied txs, sorted by recipient, summed per run
    let mut credits: Vec<Credit> = Vec::with_capacity(outputs);
    for (i, (_, tx, _)) in items.iter().enumerate() {
        if applied_flags[i].load(Ordering::Relaxed) {
            credits.extend(tx.outputs.iter().map(|o| Credit {
                key: AKey::of(&o.to),
                amount: o.amount,
            }));
        }
    }
    if par {
        credits.par_sort_unstable_by_key(|c| c.key);
    } else {
        credits.sort_unstable_by_key(|c| c.key);
    }
    // distinct recipients (+ the beneficiary, credited `fees`, always
    // present), ascending: (key, wei)
    let ben = AKey::of(&beneficiary);
    let mut recv: Vec<(AKey, u128)> = Vec::with_capacity(credits.len().min(outputs) + 1);
    let mut ben_done = false;
    let mut at = 0;
    while at < credits.len() {
        let k = credits[at].key;
        if !ben_done && ben < k {
            recv.push((ben, fees));
            ben_done = true;
        }
        let mut wei = 0u128;
        while at < credits.len() && credits[at].key == k {
            wei += credits[at].amount as u128 * AMOUNT_UNIT;
            at += 1;
        }
        if k == ben {
            wei += fees;
            ben_done = true;
        }
        recv.push((k, wei));
    }
    if !ben_done {
        recv.push((ben, fees));
    }
    drop(credits);

    // 4. merge senders and recipients in address order; a recipient that is
    //    not a sender starts from its block-start value (read in step 5)
    let mut writes: Vec<(Address, Acct)> = Vec::with_capacity(senders.len() + recv.len());
    let mut fill: Vec<usize> = Vec::with_capacity(recv.len());
    let (mut si, mut ri) = (0, 0);
    while si < senders.len() || ri < recv.len() {
        let take_s = ri == recv.len() || (si < senders.len() && senders[si].key <= recv[ri].0);
        if take_s {
            let s = &senders[si];
            let mut acct = s.acct;
            if ri < recv.len() && recv[ri].0 == s.key {
                acct.balance += recv[ri].1;
                ri += 1;
            }
            writes.push((s.key.addr(), acct));
            si += 1;
        } else {
            let (k, wei) = recv[ri];
            fill.push(writes.len());
            writes.push((
                k.addr(),
                Acct {
                    nonce: 0,
                    balance: wei,
                },
            ));
            ri += 1;
        }
    }

    // 5. block-start values of the recipient-only accounts: balance = state + credit
    let read = |w: &mut (Address, Acct)| {
        let base = state.get(&w.0);
        w.1 = Acct {
            nonce: base.nonce,
            balance: {
                let mut b = base.balance;
                b += w.1.balance;
                b
            },
        };
    };
    if par {
        // `fill` is ascending, so the writes it names are disjoint
        let mut rest: &mut [(Address, Acct)] = &mut writes;
        let mut taken = 0;
        let mut refs: Vec<&mut (Address, Acct)> = Vec::with_capacity(fill.len());
        for &i in &fill {
            let (_, tail) = std::mem::take(&mut rest).split_at_mut(i - taken);
            let (one, tail) = tail.split_first_mut().unwrap();
            refs.push(one);
            rest = tail;
            taken = i + 1;
        }
        refs.into_par_iter().for_each(|w| read(w));
    } else {
        for &i in &fill {
            read(&mut writes[i]);
        }
    }

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
    // The commitment is one serial keccak over the whole block: overlap it
    // with the (parallel) decode + recovery instead of running it first.
    let (commitment, items) = if ctx.parallel_recovery {
        rayon::join(
            || framing::commitment_of_wire(&header, wire),
            || recover(decode(&txs, ctx), ctx),
        )
    } else {
        (
            framing::commitment_of_wire(&header, wire),
            recover(decode(&txs, ctx), ctx),
        )
    };
    let delta = execute(state, &items, ctx);
    Ok(Processed {
        header,
        commitment,
        delta,
    })
}

// EVOLVE-BLOCK-END
