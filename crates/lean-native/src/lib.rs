//! Lean fan-out native transfer for the Arc payment lane — increment 1 (offline).
//!
//! One signature, N recipients, fixed-width encoding (no RLP), deterministic
//! parallel execution. See `LEAN-NATIVE.md` at the fork root for the design doc,
//! byte tables and the reth touchpoints for increment 2 (engine integration).
//!
//! Canonical wire format (little-endian integers, fixed width, no RLP). The
//! SAME bytes serve every role: signing preimage (minus sig), the EIP-2718
//! envelope payload (the leading byte IS the 2718 type byte), the raw form for
//! `eth_sendRawTransaction`, and storage — encoded once at signing, decoded
//! once at admission, passed through verbatim everywhere else.
//! ```text
//! type      u8   = 0x50 (the EIP-2718 tx type byte, part of the canon bytes)
//! nonce     u32
//! n_outputs u16  (1..=MAX_OUTPUTS)
//! outputs   n_outputs x { address 20B, amount u64 }    (28B each)
//! sig       65B  (r 32 || s 32 || v 1), secp256k1 recoverable, over
//!                keccak256(domain(32B) || canon-bytes-before-sig)
//! ```
//! Total size = 7 + 28*N + 65 = 72 + 28*N bytes. Amounts are in AMOUNT_UNIT
//! (1 gwei = 1e9 wei) base units: per-output precision $1e-9, per-output cap
//! ~1.8e28 wei — both fine for a payment lane; documented trade, saves
//! 8B/output vs u128 wei.
//!
//! Block execution semantics (BOTH executors implement exactly this — the rule
//! set is the spec, the serial executor is its reference implementation):
//! * Whole-tx atomic: any invalidity (bad version, N=0, N>MAX, nonce mismatch,
//!   output-sum overflow, insufficient funds) rejects the ENTIRE tx; state
//!   untouched. A lane block containing an invalid lean tx is itself invalid
//!   (builder never packs one; validators reject the block).
//! * Same-block incoming credits are NOT spendable: a sender's spendable
//!   balance during a block is its block-start balance minus its own earlier
//!   debits in that block. This is what makes sender-partitioned parallel
//!   execution deterministic without Block-STM machinery, and it is a lane
//!   design choice, documented, not an implementation accident.
//! * Recipient credits are commutative deltas, merged after the debit phase.
//!   Duplicate recipients inside one tx simply sum. Self-transfers credit the
//!   sender as an ordinary delta (not spendable until next block, per above).
//! * The fixed per-tx fee accumulates to the beneficiary as one deferred
//!   credit at end of block (Arc credits the beneficiary every tx; deferral is
//!   the standard trick that removes the all-tx write conflict — same result).
//! * One receipt per TX (not per output): status + one transfer log per
//!   NON-ZERO output (zero-value outputs emit no log — consensus bug #3 in the
//!   project ledger was exactly an extra log on a zero-value transfer).

pub mod envelope;
pub mod pool;

use alloy_primitives::{keccak256, Address, B256};
use rayon::prelude::*;
use secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
use secp256k1::{Message, PublicKey, SecretKey, SECP256K1};
use std::collections::HashMap;

/// EIP-2718 transaction type byte — the FIRST byte of the canonical encoding.
pub const LEAN_TX_TYPE: u8 = 0x50;
pub const MAX_OUTPUTS: usize = 10_000;
pub const HEADER_LEN: usize = 1 + 4 + 2;
pub const OUTPUT_LEN: usize = 20 + 8;
pub const SIG_LEN: usize = 65;
/// Base unit of output amounts on the wire: 1 gwei (1e9 wei).
pub const AMOUNT_UNIT: u128 = 1_000_000_000;

/// Gas accounting for the lean type: `gas(tx) = GAS_BASE + GAS_PER_OUTPUT * N`.
/// Gas is the PACKING WEIGHT (block budgeting + pool ordering) and, multiplied
/// by the lane's protocol-fixed gas price, the fee. The fee is gas-proportional
/// (changed from increment 1's flat fee): it keeps reth's generic pool cost
/// arithmetic (`max_fee_per_gas * gas_limit + value`) exactly right with no
/// overrides — and is economically saner for large fan-outs anyway.
/// `GAS_PER_OUTPUT = 5000` is a cold-account-credit-shaped weight; a lane can
/// retune it without touching the wire format (gas is not on the wire).
pub const GAS_BASE: u64 = 21_000;
pub const GAS_PER_OUTPUT: u64 = 5_000;
/// The lane's protocol-fixed gas price in wei (fee-market machinery bypassed).
pub const GAS_PRICE: u128 = 1_000_000_000; // 1 gwei

/// Gas weight of a lean tx with `n` outputs.
pub const fn lean_gas(n: usize) -> u64 {
    GAS_BASE + GAS_PER_OUTPUT * n as u64
}

/// Protocol-fixed fee of a lean tx with `n` outputs, in wei.
pub const fn lean_fee(n: usize) -> u128 {
    lean_gas(n) as u128 * GAS_PRICE
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Output {
    pub to: Address,
    /// Amount in AMOUNT_UNIT (gwei) base units; wei value = amount * AMOUNT_UNIT.
    pub amount: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LeanTx {
    pub nonce: u32,
    pub outputs: Vec<Output>,
    /// r||s||v recoverable signature over `signing_hash`.
    pub sig: [u8; SIG_LEN],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    TooShort,
    BadType(u8),
    BadOutputCount(u16),
    TrailingBytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxError {
    BadSignature,
    NonceMismatch { expected: u32, got: u32 },
    OutputSumOverflow,
    InsufficientFunds,
    UnknownSender,
}

/// Lane domain separator: commits the signature to one specific lane so a lean
/// tx can never be replayed on another chain id (chain id is otherwise implied,
/// not carried on the wire).
pub fn lane_domain(chain_id: u64) -> B256 {
    let mut buf = [0u8; 32];
    buf[..24].copy_from_slice(b"ARC_LEAN_NATIVE_TRANSFER");
    buf[24..].copy_from_slice(&chain_id.to_be_bytes());
    keccak256(buf)
}

impl LeanTx {
    pub fn size(&self) -> usize {
        HEADER_LEN + OUTPUT_LEN * self.outputs.len() + SIG_LEN
    }

    /// Canonical bytes BEFORE the signature: [type][nonce][n][outputs].
    fn body_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + OUTPUT_LEN * self.outputs.len());
        out.push(LEAN_TX_TYPE);
        out.extend_from_slice(&self.nonce.to_le_bytes());
        out.extend_from_slice(&(self.outputs.len() as u16).to_le_bytes());
        for o in &self.outputs {
            out.extend_from_slice(o.to.as_slice());
            out.extend_from_slice(&o.amount.to_le_bytes());
        }
        out
    }

    pub fn signing_hash(&self, domain: &B256) -> B256 {
        let pre = self.body_bytes();
        let mut buf = Vec::with_capacity(32 + pre.len());
        buf.extend_from_slice(domain.as_slice());
        buf.extend_from_slice(&pre);
        keccak256(&buf)
    }

    /// Full canonical encoding, INCLUDING the leading 2718 type byte.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = self.body_bytes();
        out.extend_from_slice(&self.sig);
        out
    }

    /// Decode the FULL canonical form (leading type byte included).
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.is_empty() {
            return Err(DecodeError::TooShort);
        }
        if bytes[0] != LEAN_TX_TYPE {
            return Err(DecodeError::BadType(bytes[0]));
        }
        let mut buf = &bytes[1..];
        let tx = Self::parse_after_type(&mut buf)?;
        if !buf.is_empty() {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(tx)
    }

    /// Parse one tx from the FRONT of `buf`, which is positioned just AFTER
    /// the 2718 type byte (as `Decodable2718::typed_decode` hands it over);
    /// advances the buffer past the consumed bytes (self-delimiting via
    /// `n_outputs`).
    pub fn parse_after_type(buf: &mut &[u8]) -> Result<Self, DecodeError> {
        let bytes = *buf;
        const HDR: usize = HEADER_LEN - 1; // type byte already consumed
        if bytes.len() < HDR + SIG_LEN {
            return Err(DecodeError::TooShort);
        }
        let nonce = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let n = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
        if n == 0 || n as usize > MAX_OUTPUTS {
            return Err(DecodeError::BadOutputCount(n));
        }
        let need = HDR + OUTPUT_LEN * n as usize + SIG_LEN;
        if bytes.len() < need {
            return Err(DecodeError::TooShort);
        }
        let mut outputs = Vec::with_capacity(n as usize);
        let mut at = HDR;
        for _ in 0..n {
            let to = Address::from_slice(&bytes[at..at + 20]);
            let amount = u64::from_le_bytes(bytes[at + 20..at + 28].try_into().unwrap());
            outputs.push(Output { to, amount });
            at += OUTPUT_LEN;
        }
        let mut sig = [0u8; SIG_LEN];
        sig.copy_from_slice(&bytes[at..at + SIG_LEN]);
        *buf = &bytes[need..];
        Ok(LeanTx { nonce, outputs, sig })
    }

    pub fn sign(nonce: u32, outputs: Vec<Output>, sk: &SecretKey, domain: &B256) -> Self {
        let mut tx = LeanTx { nonce, outputs, sig: [0u8; SIG_LEN] };
        let msg = Message::from_digest(tx.signing_hash(domain).0);
        let sig = SECP256K1.sign_ecdsa_recoverable(&msg, sk);
        let (rec, data) = sig.serialize_compact();
        tx.sig[..64].copy_from_slice(&data);
        tx.sig[64] = i32::from(rec) as u8;
        tx
    }

    /// ecrecover — the dominant per-tx cost being amortized over N outputs.
    pub fn recover_sender(&self, domain: &B256) -> Result<Address, TxError> {
        let rec = RecoveryId::try_from(self.sig[64] as i32).map_err(|_| TxError::BadSignature)?;
        let sig = RecoverableSignature::from_compact(&self.sig[..64], rec)
            .map_err(|_| TxError::BadSignature)?;
        let msg = Message::from_digest(self.signing_hash(domain).0);
        let pk: PublicKey = SECP256K1
            .recover_ecdsa(&msg, &sig)
            .map_err(|_| TxError::BadSignature)?;
        Ok(pubkey_to_address(&pk))
    }
}

pub fn pubkey_to_address(pk: &PublicKey) -> Address {
    let uncompressed = pk.serialize_uncompressed();
    Address::from_slice(&keccak256(&uncompressed[1..]).0[12..])
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Account {
    pub nonce: u32,
    pub balance: u128,
}

pub type State = HashMap<Address, Account>;

/// EIP-7708-shaped transfer log (from, to, amount). Zero-value outputs emit none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferLog {
    pub from: Address,
    pub to: Address,
    pub amount: u128,
}

/// One receipt per TX (not per output) — derived, never persisted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub sender: Address,
    pub n_outputs: usize,
    pub logs: Vec<TransferLog>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct BlockResult {
    pub receipts: Vec<Receipt>,
    pub fees_to_beneficiary: u128,
}

/// A tx whose sender is already recovered (recovery is a separate, perfectly
/// parallel phase in both executors so the phase costs can be reported apart).
#[derive(Clone, Debug)]
pub struct RecoveredTx {
    pub sender: Address,
    pub tx: LeanTx,
}

pub fn recover_all_serial(txs: &[LeanTx], domain: &B256) -> Result<Vec<RecoveredTx>, TxError> {
    txs.iter()
        .map(|tx| Ok(RecoveredTx { sender: tx.recover_sender(domain)?, tx: tx.clone() }))
        .collect()
}

pub fn recover_all_parallel(txs: &[LeanTx], domain: &B256) -> Result<Vec<RecoveredTx>, TxError> {
    txs.par_iter()
        .map(|tx| Ok(RecoveredTx { sender: tx.recover_sender(domain)?, tx: tx.clone() }))
        .collect()
}

fn validate_and_debit(
    sender: Address,
    tx: &LeanTx,
    spendable: &mut Account,
) -> Result<(u128, Vec<TransferLog>), TxError> {
    if tx.nonce != spendable.nonce {
        return Err(TxError::NonceMismatch { expected: spendable.nonce, got: tx.nonce });
    }
    let fee = lean_fee(tx.outputs.len());
    let mut total: u128 = fee;
    for o in &tx.outputs {
        total = total
            .checked_add(o.amount as u128 * AMOUNT_UNIT)
            .ok_or(TxError::OutputSumOverflow)?;
    }
    if spendable.balance < total {
        return Err(TxError::InsufficientFunds);
    }
    spendable.balance -= total;
    spendable.nonce += 1;
    let logs = tx
        .outputs
        .iter()
        .filter(|o| o.amount != 0)
        .map(|o| TransferLog { from: sender, to: o.to, amount: o.amount as u128 * AMOUNT_UNIT })
        .collect();
    Ok((total, logs))
}

/// Serial reference implementation of the block semantics documented above.
/// Whole-block atomicity: the first invalid tx fails the whole block (the
/// builder never packs invalid txs, so a block containing one is invalid).
pub fn execute_block_serial(
    state: &mut State,
    txs: &[RecoveredTx],
    beneficiary: Address,
) -> Result<BlockResult, (usize, TxError)> {
    // Debit phase against block-start balances (own debits accumulate).
    let mut debit_view: HashMap<Address, Account> = HashMap::new();
    let mut receipts = Vec::with_capacity(txs.len());
    let mut credits: HashMap<Address, u128> = HashMap::new();
    let mut fees: u128 = 0;
    for (i, rtx) in txs.iter().enumerate() {
        let entry = debit_view
            .entry(rtx.sender)
            .or_insert_with(|| state.get(&rtx.sender).copied().unwrap_or_default());
        let (_, logs) =
            validate_and_debit(rtx.sender, &rtx.tx, entry).map_err(|e| (i, e))?;
        for o in &rtx.tx.outputs {
            *credits.entry(o.to).or_default() += o.amount as u128 * AMOUNT_UNIT;
        }
        fees += lean_fee(rtx.tx.outputs.len());
        receipts.push(Receipt { sender: rtx.sender, n_outputs: rtx.tx.outputs.len(), logs });
    }
    // Commit debits, then credits, then the deferred beneficiary fee.
    for (addr, acct) in debit_view {
        state.insert(addr, acct);
    }
    for (addr, amount) in credits {
        state.entry(addr).or_default().balance += amount;
    }
    state.entry(beneficiary).or_default().balance += fees;
    Ok(BlockResult { receipts, fees_to_beneficiary: fees })
}

/// Deterministic parallel implementation: partition by sender (nonce/balance
/// have one owner; in-order within a partition), recipient credits aggregated
/// per worker chunk and merged once (per-worker aggregation is what took the
/// measured scheme from 2.2x to 4.25x). No aborts, no retries.
pub fn execute_block_parallel(
    state: &mut State,
    txs: &[RecoveredTx],
    beneficiary: Address,
) -> Result<BlockResult, (usize, TxError)> {
    // Partition tx indices by sender, preserving in-block order per sender.
    let mut partitions: HashMap<Address, Vec<usize>> = HashMap::new();
    for (i, rtx) in txs.iter().enumerate() {
        partitions.entry(rtx.sender).or_default().push(i);
    }
    let parts: Vec<(Address, Vec<usize>)> = partitions.into_iter().collect();
    let ro_state: &State = state;

    struct PartOut {
        sender: Address,
        post: Account,
        credits: HashMap<Address, u128>,
        receipts: Vec<(usize, Receipt)>,
        fees: u128,
    }

    let chunk = (parts.len() / (rayon::current_num_threads() * 4)).max(1);
    let results: Vec<Result<Vec<PartOut>, (usize, TxError)>> = parts
        .par_chunks(chunk)
        .map(|chunk_parts| {
            let mut outs = Vec::with_capacity(chunk_parts.len());
            for (sender, idxs) in chunk_parts {
                let mut acct = ro_state.get(sender).copied().unwrap_or_default();
                let mut credits: HashMap<Address, u128> = HashMap::new();
                let mut receipts = Vec::with_capacity(idxs.len());
                let mut fees = 0u128;
                for &i in idxs {
                    let rtx = &txs[i];
                    let (_, logs) =
                        validate_and_debit(*sender, &rtx.tx, &mut acct).map_err(|e| (i, e))?;
                    for o in &rtx.tx.outputs {
                        *credits.entry(o.to).or_default() += o.amount as u128 * AMOUNT_UNIT;
                    }
                    fees += lean_fee(rtx.tx.outputs.len());
                    receipts.push((
                        i,
                        Receipt { sender: *sender, n_outputs: rtx.tx.outputs.len(), logs },
                    ));
                }
                outs.push(PartOut { sender: *sender, post: acct, credits, receipts, fees });
            }
            Ok(outs)
        })
        .collect();

    // Merge phase (single-threaded, deltas only).
    let mut receipts_slots: Vec<Option<Receipt>> = vec![None; txs.len()];
    let mut fees_total = 0u128;
    let mut merged: Vec<PartOut> = Vec::with_capacity(parts.len());
    let mut first_err: Option<(usize, TxError)> = None;
    for r in results {
        match r {
            Ok(outs) => merged.extend(outs),
            Err(e) => {
                // Whole-block failure must report the FIRST invalid tx index,
                // matching the serial reference regardless of thread timing.
                if first_err.map_or(true, |(fi, _)| e.0 < fi) {
                    first_err = Some(e);
                }
            }
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    for out in merged {
        state.insert(out.sender, out.post);
        for (addr, amount) in out.credits {
            state.entry(addr).or_default().balance += amount;
        }
        fees_total += out.fees;
        for (i, rec) in out.receipts {
            receipts_slots[i] = Some(rec);
        }
    }
    state.entry(beneficiary).or_default().balance += fees_total;
    let receipts = receipts_slots.into_iter().map(|r| r.expect("all txs accounted")).collect();
    Ok(BlockResult { receipts, fees_to_beneficiary: fees_total })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sk(i: u64) -> SecretKey {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&(i + 1).to_be_bytes());
        b[31] = 1;
        SecretKey::from_slice(&b).unwrap()
    }

    fn addr_of(i: u64, domain: &B256) -> Address {
        let tx = LeanTx::sign(0, vec![Output { to: Address::ZERO, amount: 1 }], &sk(i), domain);
        tx.recover_sender(domain).unwrap()
    }

    fn dom() -> B256 {
        lane_domain(1338)
    }

    fn fund(state: &mut State, a: Address, balance: u128) {
        state.insert(a, Account { nonce: 0, balance });
    }

    #[test]
    fn roundtrip_various_sizes() {
        let d = dom();
        for n in [1usize, 2, 10, 100, MAX_OUTPUTS] {
            let outputs: Vec<Output> = (0..n)
                .map(|i| Output { to: Address::with_last_byte((i % 250) as u8), amount: i as u64 })
                .collect();
            let tx = LeanTx::sign(7, outputs, &sk(1), &d);
            let bytes = tx.encode();
            assert_eq!(bytes.len(), 72 + 28 * n);
            assert_eq!(bytes[0], LEAN_TX_TYPE);
            let back = LeanTx::decode(&bytes).unwrap();
            assert_eq!(back, tx);
            assert_eq!(back.recover_sender(&d).unwrap(), tx.recover_sender(&d).unwrap());
        }
    }

    #[test]
    fn decode_rejects_malformed() {
        let d = dom();
        let tx = LeanTx::sign(0, vec![Output { to: Address::ZERO, amount: 5 }], &sk(1), &d);
        let good = tx.encode();
        // too short at every truncation point
        for cut in 0..good.len() {
            assert!(LeanTx::decode(&good[..cut]).is_err(), "cut={cut}");
        }
        // trailing garbage
        let mut long = good.clone();
        long.push(0);
        assert_eq!(LeanTx::decode(&long), Err(DecodeError::TrailingBytes));
        // wrong type byte
        let mut bad = good.clone();
        bad[0] = 0x51;
        assert!(matches!(LeanTx::decode(&bad), Err(DecodeError::BadType(0x51))));
        // zero outputs
        let mut z = good.clone();
        z[5] = 0;
        z[6] = 0;
        assert!(matches!(LeanTx::decode(&z), Err(DecodeError::BadOutputCount(0)) | Err(DecodeError::TrailingBytes)));
        // tampered output amount breaks signature recovery to the same sender
        let mut t = good.clone();
        let amt_off = HEADER_LEN + 20;
        t[amt_off] ^= 1;
        let tampered = LeanTx::decode(&t).unwrap();
        assert_ne!(
            tampered.recover_sender(&d).ok(),
            tx.recover_sender(&d).ok(),
            "tamper must not recover to the original sender"
        );
    }

    #[test]
    fn domain_separates_lanes() {
        let d1 = lane_domain(1338);
        let d2 = lane_domain(1337);
        let tx = LeanTx::sign(0, vec![Output { to: Address::ZERO, amount: 5 }], &sk(3), &d1);
        assert_ne!(tx.recover_sender(&d1).unwrap(), tx.recover_sender(&d2).unwrap());
    }

    #[test]
    fn semantics_fanout_atomic_and_logs() {
        let d = dom();
        let sender = addr_of(1, &d);
        let ben = Address::with_last_byte(0xbe);
        let r1 = Address::with_last_byte(1);
        let r2 = Address::with_last_byte(2);
        let mut state = State::new();
        fund(&mut state, sender, 1_000_000 * AMOUNT_UNIT + lean_fee(3));
        // zero-value output emits NO log; duplicate recipient sums.
        let tx = LeanTx::sign(
            0,
            vec![
                Output { to: r1, amount: 400_000 },
                Output { to: r2, amount: 0 },
                Output { to: r1, amount: 600_000 },
            ],
            &sk(1),
            &d,
        );
        let rec = recover_all_serial(&[tx], &d).unwrap();
        let out = execute_block_serial(&mut state, &rec, ben).unwrap();
        assert_eq!(out.receipts[0].logs.len(), 2, "zero-value output must not log");
        assert_eq!(state[&r1].balance, 1_000_000 * AMOUNT_UNIT);
        assert!(state.get(&r2).map_or(true, |a| a.balance == 0));
        assert_eq!(state[&sender].balance, 0);
        assert_eq!(state[&ben].balance, lean_fee(3));
        assert_eq!(state[&sender].nonce, 1);
    }

    #[test]
    fn semantics_insufficient_funds_is_whole_tx() {
        let d = dom();
        let sender = addr_of(2, &d);
        let mut state = State::new();
        fund(&mut state, sender, lean_fee(2) + 99); // can afford first output alone, not both
        let tx = LeanTx::sign(
            0,
            vec![
                Output { to: Address::with_last_byte(1), amount: 50 },
                Output { to: Address::with_last_byte(2), amount: 50 },
            ],
            &sk(2),
            &d,
        );
        let rec = recover_all_serial(&[tx], &d).unwrap();
        let snapshot = state.clone();
        let err = execute_block_serial(&mut state, &rec, Address::ZERO).unwrap_err();
        assert_eq!(err, (0, TxError::InsufficientFunds));
        assert_eq!(state, snapshot, "whole-tx atomic: no partial application");
    }

    #[test]
    fn semantics_same_block_credit_not_spendable() {
        let d = dom();
        let a = addr_of(3, &d);
        let b = addr_of(4, &d);
        let mut state = State::new();
        fund(&mut state, a, 1_000 * AMOUNT_UNIT + lean_fee(1));
        fund(&mut state, b, lean_fee(1)); // b can pay its fee but not forward a's credit
        let t1 = LeanTx::sign(0, vec![Output { to: b, amount: 1_000 }], &sk(3), &d);
        let t2 = LeanTx::sign(0, vec![Output { to: Address::with_last_byte(9), amount: 500 }], &sk(4), &d);
        let rec = recover_all_serial(&[t1, t2], &d).unwrap();
        let err = execute_block_serial(&mut state.clone(), &rec, Address::ZERO).unwrap_err();
        assert_eq!(err, (1, TxError::InsufficientFunds), "same-block credit must not fund t2");
        let errp = execute_block_parallel(&mut state, &rec, Address::ZERO).unwrap_err();
        assert_eq!(err, errp);
    }

    fn differential(workload: Vec<(u64, u32, Vec<Output>)>, balances: Vec<(u64, u128)>) {
        let d = dom();
        let ben = Address::with_last_byte(0xbe);
        let mut state = State::new();
        for (key, bal) in &balances {
            fund(&mut state, addr_of(*key, &d), *bal);
        }
        let txs: Vec<LeanTx> = workload
            .iter()
            .map(|(key, nonce, outs)| LeanTx::sign(*nonce, outs.clone(), &sk(*key), &d))
            .collect();
        let rec_s = recover_all_serial(&txs, &d).unwrap();
        let rec_p = recover_all_parallel(&txs, &d).unwrap();
        assert_eq!(rec_s.iter().map(|r| r.sender).collect::<Vec<_>>(),
                   rec_p.iter().map(|r| r.sender).collect::<Vec<_>>());
        let mut s1 = state.clone();
        let mut s2 = state;
        let o1 = execute_block_serial(&mut s1, &rec_s, ben);
        let o2 = execute_block_parallel(&mut s2, &rec_p, ben);
        match (&o1, &o2) {
            (Ok(a), Ok(b)) => {
                assert_eq!(a, b);
                assert_eq!(s1, s2);
            }
            (Err(a), Err(b)) => assert_eq!(a, b),
            _ => panic!("serial/parallel disagree on validity: {o1:?} vs {o2:?}"),
        }
    }

    #[test]
    fn differential_pooled() {
        let hot = Address::with_last_byte(0xff);
        let mut work = Vec::new();
        let mut balances = Vec::new();
        for k in 0..40u64 {
            balances.push((k, 10 * lean_fee(3) + 100_000 * AMOUNT_UNIT));
            // two txs per sender, in nonce order, mixed fan-out incl. hot recipient + self dups
            work.push((k, 0u32, vec![
                Output { to: hot, amount: 7 },
                Output { to: Address::with_last_byte((k % 200) as u8), amount: 11 },
                Output { to: hot, amount: 3 },
            ]));
            work.push((k, 1u32, vec![Output { to: hot, amount: 1 }]));
        }
        differential(work, balances);
    }

    #[test]
    fn differential_self_transfer_and_failure() {
        let d = dom();
        let a3 = addr_of(6, &d);
        // sender 6: self-transfer; sender 7: nonce gap -> whole block invalid, same index both impls
        differential(
            vec![
                (6, 0, vec![Output { to: a3, amount: 500 }]),  // 500 gwei
                (7, 5, vec![Output { to: Address::with_last_byte(1), amount: 1 }]),
            ],
            vec![(6, lean_fee(1) + 1_000 * AMOUNT_UNIT), (7, lean_fee(1) + 1_000 * AMOUNT_UNIT)],
        );
    }

    #[test]
    fn differential_many_senders_random_shape() {
        let mut work = Vec::new();
        let mut balances = Vec::new();
        for k in 0..200u64 {
            balances.push((k, 3 * lean_fee(13) + 1_000_000 * AMOUNT_UNIT));
            let n = 1 + (k % 13) as usize;
            let outs: Vec<Output> = (0..n)
                .map(|i| Output {
                    to: Address::with_last_byte(((k * 31 + i as u64 * 7) % 251) as u8),
                    amount: (k * 1000 + i as u64) % 900,
                })
                .collect();
            work.push((k, 0u32, outs));
        }
        differential(work, balances);
    }
}
