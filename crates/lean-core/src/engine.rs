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
    shard_of, Acct, AddressHasherBuilder, BlockError, BlockHeader, BlockOutcome, Decoded, Delta,
    EngineCtx, FlatState, Processed, RecoveredTx, StateView, SHARDS,
};
use alloy_primitives::{keccak256, Address, B256};
use lean_native::{
    lean_fee, recover_prehashed, signing_hash_of_body, LeanTx, AMOUNT_UNIT, LEAN_TX_TYPE, SIG_LEN,
};
use rayon::prelude::*;
use std::collections::HashMap;
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

/// Decode and recover one entry in a single pass over its bytes (CONTRACT:
/// DECODE + RECOVER). The signing hash is taken straight from the entry's
/// canonical body bytes — byte-identical to `LeanTx::signing_hash`'s
/// re-encode, since the parser reads every body field from exactly those
/// positions and `n` round-trips; the tx hash only for a recovered tx.
#[inline]
fn decode_recover_one(bytes: &[u8], domain: &B256) -> Option<RecoveredTx> {
    if bytes.first() != Some(&LEAN_TX_TYPE) {
        return None;
    }
    let mut buf = &bytes[1..];
    let tx = LeanTx::parse_after_type(&mut buf).ok()?;
    let canon = &bytes[..bytes.len() - buf.len()];
    let body = &canon[..canon.len() - SIG_LEN];
    let sender = recover_prehashed(&tx.sig, signing_hash_of_body(domain, body)).ok()?;
    Some((sender, tx, keccak256(canon)))
}

/// `decode` + `recover` fused into one pass (one rayon pass in parallel
/// mode), block order kept.
pub(crate) fn decode_recover<T: AsRef<[u8]> + Sync>(
    txs: &[T],
    ctx: &EngineCtx,
) -> Vec<RecoveredTx> {
    let domain = ctx.domain;
    if ctx.parallel_recovery {
        txs.par_iter()
            .map(|t| decode_recover_one(t.as_ref(), &domain))
            .collect::<Vec<_>>()
            .into_iter()
            .flatten()
            .collect()
    } else {
        txs.iter()
            .filter_map(|t| decode_recover_one(t.as_ref(), &domain))
            .collect()
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

    // 3. credits of applied txs, sorted by recipient: a counting sort on the
    //    address's first byte (256 buckets, bucket order = address order),
    //    then each bucket sorted on its own (in parallel)
    let mut counts = [0usize; 256];
    for (i, (_, tx, _)) in items.iter().enumerate() {
        if applied_flags[i].load(Ordering::Relaxed) {
            for o in &tx.outputs {
                counts[o.to.0[0] as usize] += 1;
            }
        }
    }
    let mut next = [0usize; 256];
    let mut sum = 0;
    for (b, c) in counts.iter().enumerate() {
        next[b] = sum;
        sum += c;
    }
    let mut credits: Vec<Credit> = vec![
        Credit {
            key: AKey(0, 0, 0),
            amount: 0
        };
        sum
    ];
    for (i, (_, tx, _)) in items.iter().enumerate() {
        if applied_flags[i].load(Ordering::Relaxed) {
            for o in &tx.outputs {
                let b = o.to.0[0] as usize;
                credits[next[b]] = Credit {
                    key: AKey::of(&o.to),
                    amount: o.amount,
                };
                next[b] += 1;
            }
        }
    }
    let mut buckets: Vec<&mut [Credit]> = Vec::with_capacity(256);
    let mut rest: &mut [Credit] = &mut credits;
    for c in counts {
        let (bucket, tail) = std::mem::take(&mut rest).split_at_mut(c);
        buckets.push(bucket);
        rest = tail;
    }
    if par {
        buckets
            .into_par_iter()
            .for_each(|b| b.sort_unstable_by_key(|c| c.key));
    } else {
        buckets
            .into_iter()
            .for_each(|b| b.sort_unstable_by_key(|c| c.key));
    }
    // 4. one merge, in address order, of senders ∪ recipient runs (summed)
    //    ∪ the beneficiary (credited `fees`, always written). An account that
    //    is not a sender starts from its block-start value, read in step 5;
    //    until then it holds just its credit and is flagged in `from_state`.
    let ben = AKey::of(&beneficiary);
    let mut writes: Vec<(Address, Acct)> = Vec::with_capacity(senders.len() + credits.len() + 1);
    let mut from_state: Vec<bool> = Vec::with_capacity(senders.len() + credits.len() + 1);
    let mut ben_pending = true;
    let (mut si, mut ci) = (0, 0);
    loop {
        let mut k = match (senders.get(si), credits.get(ci)) {
            (Some(p), Some(c)) => p.key.min(c.key),
            (Some(p), None) => p.key,
            (None, Some(c)) => c.key,
            (None, None) if ben_pending => ben,
            (None, None) => break,
        };
        if ben_pending && ben < k {
            k = ben;
        }
        let mut wei = 0u128;
        while ci < credits.len() && credits[ci].key == k {
            wei += credits[ci].amount as u128 * AMOUNT_UNIT;
            ci += 1;
        }
        if ben_pending && k == ben {
            wei += fees;
            ben_pending = false;
        }
        if si < senders.len() && senders[si].key == k {
            let mut acct = senders[si].acct;
            acct.balance += wei;
            writes.push((k.addr(), acct));
            from_state.push(false);
            si += 1;
        } else {
            writes.push((
                k.addr(),
                Acct {
                    nonce: 0,
                    balance: wei,
                },
            ));
            from_state.push(true);
        }
    }
    drop(credits);

    // 5. block-start values of the flagged accounts: balance = state + credit
    let read = |(w, &flag): (&mut (Address, Acct), &bool)| {
        if flag {
            let mut acct = state.get(&w.0);
            acct.balance += w.1.balance;
            w.1 = acct;
        }
    };
    if par {
        writes
            .par_iter_mut()
            .zip(from_state.par_iter())
            .with_min_len(512)
            .for_each(read);
    } else {
        writes.iter_mut().zip(from_state.iter()).for_each(read);
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
///
/// Most writes hit an existing account: overwrite it in place (`get_mut`) and
/// insert only when absent — the same map afterwards as `insert` for every
/// write, but an update skips the insert path's slot search/reserve.
///
/// A large write set sorted by shard (every `execute` delta: writes are
/// sorted by address and shards are address-ordered) is applied shard-
/// parallel: each shard's writes are one contiguous run and shards are
/// disjoint maps. Within a shard the writes keep their order, so even a
/// hand-built delta with a repeated address ends with its last value, as
/// the serial loop does. Anything else runs serially.
pub(crate) fn apply(state: &mut FlatState, delta: &Delta) {
    fn write(m: &mut HashMap<Address, Acct, AddressHasherBuilder>, w: &[(Address, Acct)]) {
        for (a, acct) in w {
            match m.get_mut(a) {
                Some(v) => *v = *acct,
                None => {
                    m.insert(*a, *acct);
                }
            }
        }
    }
    let w = &delta.writes;
    if w.len() >= PAR_APPLY_MIN && w.windows(2).all(|p| shard_of(&p[0].0) <= shard_of(&p[1].0)) {
        let mut runs: Vec<&[(Address, Acct)]> = Vec::with_capacity(SHARDS);
        let mut rest = &w[..];
        for s in 0..SHARDS {
            let n = rest.partition_point(|(a, _)| shard_of(a) <= s);
            let (run, tail) = rest.split_at(n);
            runs.push(run);
            rest = tail;
        }
        state
            .accounts
            .shards_mut()
            .par_iter_mut()
            .zip(runs)
            .for_each(|(m, run)| write(m, run));
    } else {
        for (a, acct) in w {
            match state.accounts.get_mut(a) {
                Some(v) => *v = *acct,
                None => {
                    state.accounts.insert(*a, *acct);
                }
            }
        }
    }
}

/// Below this many writes `apply` stays on the calling thread.
const PAR_APPLY_MIN: usize = 2048;

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
            || decode_recover(&txs, ctx),
        )
    } else {
        (
            framing::commitment_of_wire(&header, wire),
            decode_recover(&txs, ctx),
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
