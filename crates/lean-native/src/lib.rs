//! Lean fan-out native transfer for the Arc payment lane: one signature,
//! N recipients, fixed-width encoding (no RLP).
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
//! Block execution semantics (implemented by the node, `lean-lane-node`'s
//! `exec::execute`). The state transition is TOTAL:
//! * Whole-tx atomic: any invalidity (nonce mismatch, output-sum overflow,
//!   insufficient funds) makes the ENTIRE tx a no-op; state untouched. The
//!   BLOCK stays valid — an invalid tx never rejects the block that carries
//!   it, so a byzantine proposer can waste bytes but cannot halt or fork the
//!   lane. (Malformed bytes that do not decode or recover are skipped.)
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
//! * No receipts or logs are produced or committed.

pub mod envelope;
pub mod pool;

use alloy_primitives::{keccak256, Address, B256};
use secp256k1::ecdsa::{RecoverableSignature, RecoveryId};
use secp256k1::{Message, PublicKey, SecretKey, SECP256K1};

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
/// (not a flat fee): it keeps reth's generic pool cost
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
        Ok(LeanTx {
            nonce,
            outputs,
            sig,
        })
    }

    pub fn sign(nonce: u32, outputs: Vec<Output>, sk: &SecretKey, domain: &B256) -> Self {
        let mut tx = LeanTx {
            nonce,
            outputs,
            sig: [0u8; SIG_LEN],
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sk(i: u64) -> SecretKey {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&(i + 1).to_be_bytes());
        b[31] = 1;
        SecretKey::from_slice(&b).unwrap()
    }

    fn dom() -> B256 {
        lane_domain(1338)
    }

    #[test]
    fn roundtrip_various_sizes() {
        let d = dom();
        for n in [1usize, 2, 10, 100, MAX_OUTPUTS] {
            let outputs: Vec<Output> = (0..n)
                .map(|i| Output {
                    to: Address::with_last_byte((i % 250) as u8),
                    amount: i as u64,
                })
                .collect();
            let tx = LeanTx::sign(7, outputs, &sk(1), &d);
            let bytes = tx.encode();
            assert_eq!(bytes.len(), 72 + 28 * n);
            assert_eq!(bytes[0], LEAN_TX_TYPE);
            let back = LeanTx::decode(&bytes).unwrap();
            assert_eq!(back, tx);
            assert_eq!(
                back.recover_sender(&d).unwrap(),
                tx.recover_sender(&d).unwrap()
            );
        }
    }

    #[test]
    fn decode_rejects_malformed() {
        let d = dom();
        let tx = LeanTx::sign(
            0,
            vec![Output {
                to: Address::ZERO,
                amount: 5,
            }],
            &sk(1),
            &d,
        );
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
        assert!(matches!(
            LeanTx::decode(&bad),
            Err(DecodeError::BadType(0x51))
        ));
        // zero outputs
        let mut z = good.clone();
        z[5] = 0;
        z[6] = 0;
        assert!(matches!(
            LeanTx::decode(&z),
            Err(DecodeError::BadOutputCount(0)) | Err(DecodeError::TrailingBytes)
        ));
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
        let tx = LeanTx::sign(
            0,
            vec![Output {
                to: Address::ZERO,
                amount: 5,
            }],
            &sk(3),
            &d1,
        );
        assert_ne!(
            tx.recover_sender(&d1).unwrap(),
            tx.recover_sender(&d2).unwrap()
        );
    }
}
