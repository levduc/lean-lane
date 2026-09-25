//! SPEC.md §1 — the fan-out transfer transaction (type `0x50`).

use k256::elliptic_curve::ops::Reduce;
use k256::elliptic_curve::point::DecompressPoint;
use k256::elliptic_curve::sec1::ToEncodedPoint;
use k256::elliptic_curve::subtle::Choice;
use k256::elliptic_curve::PrimeField;
use k256::{AffinePoint, FieldBytes, ProjectivePoint, Scalar, U256};

use crate::primitives::{
    keccak256, Address, Hash, AMOUNT_UNIT, GAS_BASE, GAS_PER_OUTPUT, GAS_PRICE, HEADER_LEN,
    LANE_DOMAIN_TAG, LEAN_TX_TYPE, MAX_OUTPUTS, OUTPUT_LEN, SECP256K1_N, SIG_LEN,
};

/// One output of a fan-out tx (§1.2): recipient and amount in `AMOUNT_UNIT` (gwei).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// §1.2 `to_i` — any address, including zero, duplicates and the sender itself.
    pub to: Address,
    /// §1.2 `amount_i` — u64 in AMOUNT_UNIT; wei value = amount * 1e9. Zero is permitted.
    pub amount: u64,
}

impl Output {
    /// §1.2: wei value of this output (`amount_i * 1e9`). Cannot overflow u128
    /// (u64::MAX * 1e9 < 2^94).
    pub fn wei(&self) -> u128 {
        u128::from(self.amount) * AMOUNT_UNIT
    }
}

/// A parsed fan-out transaction (§1.2). Holds exactly the canonical fields, so
/// [`Tx::encode`] reproduces the canonical bytes (for a prefix decode: the canonical
/// prefix, never the ignored trailing bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tx {
    /// §1.2 `nonce` — u32 on the wire.
    pub nonce: u32,
    /// §1.2 outputs, ordered, `1 <= len <= 10000`.
    pub outputs: Vec<Output>,
    /// §1.5 `r` — big-endian 32 bytes.
    pub r: [u8; 32],
    /// §1.5 `s` — big-endian 32 bytes.
    pub s: [u8; 32],
    /// §1.5 `v` — raw recovery id byte.
    pub v: u8,
}

/// Why a byte string is not a well-formed tx (§1.3), listed in check order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// §1.3 step 1: `len(b) >= 1` fails.
    Empty,
    /// §1.3 step 1: `b[0] != 0x50`.
    WrongType(u8),
    /// §1.3 step 2: fewer than 71 bytes after the type byte.
    TooShort,
    /// §1.3 step 3: `n_outputs` outside `1..=10000`.
    BadOutputCount(u16),
    /// §1.3 step 4: `len(b) < 72 + 28*N`.
    Truncated { needed: usize, got: usize },
    /// §1.3 strict decode only: `len(b) != 72 + 28*N`.
    TrailingBytes { expected: usize, got: usize },
}

/// Why sender recovery failed (§1.5). Any of these makes the tx **unrecoverable**.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unrecoverable {
    /// §1.5: `v >= 4` (includes Ethereum-style 27/28).
    BadRecoveryId(u8),
    /// §1.5: `r >= n`.
    RNotBelowOrder,
    /// §1.5: `s >= n`.
    SNotBelowOrder,
    /// §1.5: `r == 0`.
    RZero,
    /// §1.5: `s == 0`.
    SZero,
    /// §1.5: `v` bit 1 set and `r + n` does not fit in 256 bits.
    XOverflow,
    /// No curve point has x = (r or r+n) with the requested parity (includes x >= p).
    NoCurvePoint,
    /// The recovered public key is the point at infinity.
    PointAtInfinity,
}

/// §1.2: canonical length `72 + 28*N`.
pub fn canonical_len(n_outputs: usize) -> usize {
    HEADER_LEN + OUTPUT_LEN * n_outputs + SIG_LEN
}

/// §1.3 **prefix** decode: steps 1–4; bytes beyond `72 + 28*N` are ignored.
/// Used for block entries (§2.4).
pub fn decode_tx_prefix(b: &[u8]) -> Result<Tx, Malformed> {
    // Step 1: len(b) >= 1 and b[0] == 0x50.
    if b.is_empty() {
        return Err(Malformed::Empty);
    }
    if b[0] != LEAN_TX_TYPE {
        return Err(Malformed::WrongType(b[0]));
    }
    // Step 2: len(b) - 1 >= 71 (6 header bytes after the type + 65 sig bytes).
    if b.len() - 1 < 71 {
        return Err(Malformed::TooShort);
    }
    // Step 3: N = u16_le(b[5..7]), 1 <= N <= 10000.
    let n_raw = u16::from_le_bytes([b[5], b[6]]);
    let n = usize::from(n_raw);
    if !(1..=MAX_OUTPUTS).contains(&n) {
        return Err(Malformed::BadOutputCount(n_raw));
    }
    // Step 4: len(b) >= 72 + 28*N.
    let needed = canonical_len(n);
    if b.len() < needed {
        return Err(Malformed::Truncated {
            needed,
            got: b.len(),
        });
    }

    // Well-formed: parse the fields at the §1.2 offsets.
    let nonce = u32::from_le_bytes([b[1], b[2], b[3], b[4]]);
    let mut outputs = Vec::with_capacity(n);
    for i in 0..n {
        let off = HEADER_LEN + OUTPUT_LEN * i; // 7 + 28*i
        let mut to = [0u8; 20];
        to.copy_from_slice(&b[off..off + 20]);
        let mut amt = [0u8; 8];
        amt.copy_from_slice(&b[off + 20..off + 28]); // 27 + 28*i
        outputs.push(Output {
            to,
            amount: u64::from_le_bytes(amt),
        });
    }
    let sig_off = HEADER_LEN + OUTPUT_LEN * n; // 7 + 28*N
    let mut r = [0u8; 32];
    r.copy_from_slice(&b[sig_off..sig_off + 32]);
    let mut s = [0u8; 32];
    s.copy_from_slice(&b[sig_off + 32..sig_off + 64]);
    let v = b[sig_off + 64];
    Ok(Tx {
        nonce,
        outputs,
        r,
        s,
        v,
    })
}

/// §1.3 **strict** decode (the standalone form): steps 1–4 plus `len(b) == 72 + 28*N`.
pub fn decode_tx(b: &[u8]) -> Result<Tx, Malformed> {
    let tx = decode_tx_prefix(b)?;
    let expected = canonical_len(tx.outputs.len());
    if b.len() != expected {
        return Err(Malformed::TrailingBytes {
            expected,
            got: b.len(),
        });
    }
    Ok(tx)
}

impl Tx {
    /// §1.2 `body`: the first `7 + 28*N` canonical bytes (everything before `sig`).
    pub fn body(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + OUTPUT_LEN * self.outputs.len());
        out.push(LEAN_TX_TYPE);
        out.extend_from_slice(&self.nonce.to_le_bytes());
        // A Tx built by hand could exceed u16; that would not be a §1.2 tx at all.
        let n = u16::try_from(self.outputs.len()).expect("§1.2: n_outputs must fit u16");
        out.extend_from_slice(&n.to_le_bytes());
        for o in &self.outputs {
            out.extend_from_slice(&o.to);
            out.extend_from_slice(&o.amount.to_le_bytes());
        }
        out
    }

    /// §1.2 canonical bytes: `body ‖ r ‖ s ‖ v` (length `72 + 28*N`).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = self.body();
        out.extend_from_slice(&self.r);
        out.extend_from_slice(&self.s);
        out.push(self.v);
        out
    }

    /// §1.6 `lean_gas(N)` for this tx.
    pub fn gas(&self) -> u128 {
        lean_gas(self.outputs.len())
    }

    /// §1.6 `lean_fee(N)` for this tx.
    pub fn fee(&self) -> u128 {
        lean_fee(self.outputs.len())
    }
}

/// §1.4 `lane_domain(chain_id) = keccak256("ARC_LEAN_NATIVE_TRANSFER" ‖ u64_be(chain_id))`.
/// Note the chain id is **big-endian** here (§0).
pub fn lane_domain(chain_id: u64) -> Hash {
    let mut pre = Vec::with_capacity(32);
    pre.extend_from_slice(LANE_DOMAIN_TAG);
    pre.extend_from_slice(&chain_id.to_be_bytes());
    debug_assert_eq!(pre.len(), 32);
    keccak256(&pre)
}

/// §1.4 `signing_hash = keccak256(lane_domain(chain_id) ‖ body)`.
pub fn signing_hash(tx: &Tx, chain_id: u64) -> Hash {
    let mut pre = lane_domain(chain_id).to_vec();
    pre.extend_from_slice(&tx.body());
    keccak256(&pre)
}

/// §1.6 `tx_hash = keccak256(canonical bytes)`. The argument must be the canonical
/// bytes (for a block entry with trailing bytes, the canonical prefix — see §2.4; use
/// `tx_hash(&tx.encode())` or [`crate::stf::classify_entry`]).
pub fn tx_hash(canonical_bytes: &[u8]) -> Hash {
    keccak256(canonical_bytes)
}

/// §1.6 `lean_gas(N) = 21000 + 5000 * N`.
pub fn lean_gas(n_outputs: usize) -> u128 {
    GAS_BASE + GAS_PER_OUTPUT * n_outputs as u128
}

/// §1.6 `lean_fee(N) = lean_gas(N) * GAS_PRICE` (wei).
pub fn lean_fee(n_outputs: usize) -> u128 {
    lean_gas(n_outputs) * GAS_PRICE
}

/// §1.5 sender recovery for `tx` under `chain_id`.
pub fn recover_sender(tx: &Tx, chain_id: u64) -> Result<Address, Unrecoverable> {
    let msg = signing_hash(tx, chain_id);
    recover_address(&msg, &tx.r, &tx.s, tx.v)
}

/// §1.5: textbook ECDSA public-key recovery over the 32-byte message `msg` (no further
/// hashing), written out step by step on top of k256's field/group arithmetic.
///
/// Deliberately NOT `k256::ecdsa::VerifyingKey::recover_from_prehash`: that function
/// re-verifies the signature and k256's verifier rejects high-s, while §1.5 says
/// "High-s is accepted".
///
/// Given `(r, s, v)`: `R` = the curve point with `x = r (+ n if v bit 1)` and y parity
/// `v bit 0`; `z = msg mod n`; `Q = r⁻¹ · (s·R − z·G)`; sender = keccak(Q_uncompressed[1..65])[12..].
pub fn recover_address(
    msg: &Hash,
    r: &[u8; 32],
    s: &[u8; 32],
    v: u8,
) -> Result<Address, Unrecoverable> {
    // §1.5: v is a raw recovery id; values 0..=3 accepted, v >= 4 fails (27/28 included).
    if v >= 4 {
        return Err(Unrecoverable::BadRecoveryId(v));
    }
    // §1.5: r >= n or s >= n fails. `Scalar::from_repr` returns None exactly when the
    // big-endian value is >= n.
    let r_scalar: Scalar = Option::from(Scalar::from_repr(FieldBytes::from(*r)))
        .ok_or(Unrecoverable::RNotBelowOrder)?;
    let s_scalar: Scalar = Option::from(Scalar::from_repr(FieldBytes::from(*s)))
        .ok_or(Unrecoverable::SNotBelowOrder)?;
    // §1.5: r == 0 or s == 0 fails at recovery.
    if bool::from(r_scalar.is_zero()) {
        return Err(Unrecoverable::RZero);
    }
    if bool::from(s_scalar.is_zero()) {
        return Err(Unrecoverable::SZero);
    }
    // §1.5: bit 1 of v => x = r + n (only possible if it stays a field element).
    let x = if v & 2 != 0 {
        add_be_256(r, &SECP256K1_N).ok_or(Unrecoverable::XOverflow)?
    } else {
        *r
    };
    // §1.5: bit 0 of v = y parity. `decompress` rejects x >= p and non-residues.
    let y_is_odd = Choice::from(v & 1);
    let big_r: AffinePoint = Option::from(AffinePoint::decompress(&FieldBytes::from(x), y_is_odd))
        .ok_or(Unrecoverable::NoCurvePoint)?;
    // Standard ECDSA: the 32-byte message is interpreted big-endian and reduced mod n.
    // (See SPEC-QUESTIONS.md Q1.)
    let z = <Scalar as Reduce<U256>>::reduce_bytes(&FieldBytes::from(*msg));
    // r != 0 was checked above, so the inverse exists.
    let r_inv: Scalar = Option::from(r_scalar.invert()).expect("r != 0 checked");
    let q = (ProjectivePoint::from(big_r) * s_scalar - ProjectivePoint::GENERATOR * z) * r_inv;
    if q == ProjectivePoint::IDENTITY {
        return Err(Unrecoverable::PointAtInfinity);
    }
    let encoded = q.to_affine().to_encoded_point(false); // 0x04 ‖ X(32) ‖ Y(32)
    let bytes = encoded.as_bytes();
    debug_assert_eq!(bytes.len(), 65);
    Ok(pubkey_to_address(&bytes[1..65]))
}

/// §1.5: `sender = keccak256(uncompressed_pubkey[1..65])[12..32]`.
pub fn pubkey_to_address(xy: &[u8]) -> Address {
    let h = keccak256(xy);
    let mut a = [0u8; 20];
    a.copy_from_slice(&h[12..32]);
    a
}

/// Big-endian 256-bit addition; `None` on carry out of 256 bits.
fn add_be_256(a: &[u8; 32], b: &[u8; 32]) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    let mut carry = 0u16;
    for i in (0..32).rev() {
        let sum = u16::from(a[i]) + u16::from(b[i]) + carry;
        out[i] = (sum & 0xff) as u8;
        carry = sum >> 8;
    }
    if carry != 0 {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal(n: u16) -> Vec<u8> {
        let mut b = vec![LEAN_TX_TYPE, 0, 0, 0, 0];
        b.extend_from_slice(&n.to_le_bytes());
        b.resize(canonical_len(usize::from(n)), 0);
        b
    }

    #[test]
    fn decode_check_order() {
        assert_eq!(decode_tx(&[]), Err(Malformed::Empty));
        assert_eq!(decode_tx(&[0x02, 1]), Err(Malformed::WrongType(0x02)));
        // 71 bytes total = 70 after the type: step 2 fails.
        let mut short = vec![LEAN_TX_TYPE];
        short.resize(71, 0);
        assert_eq!(decode_tx(&short), Err(Malformed::TooShort));
        // Exactly 72 bytes with N = 0: step 3 fails.
        assert_eq!(decode_tx(&minimal(0)), Err(Malformed::BadOutputCount(0)));
        let mut too_many = minimal(1);
        too_many[5..7].copy_from_slice(&10_001u16.to_le_bytes());
        assert_eq!(decode_tx(&too_many), Err(Malformed::BadOutputCount(10_001)));
        // N = 2 but only room for 1: step 4 fails.
        let mut trunc = minimal(1);
        trunc[5..7].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(
            decode_tx(&trunc),
            Err(Malformed::Truncated {
                needed: 128,
                got: 100
            })
        );
    }

    #[test]
    fn max_outputs_decodes() {
        let b = minimal(10_000);
        assert_eq!(decode_tx(&b).unwrap().outputs.len(), 10_000);
    }

    #[test]
    fn strict_vs_prefix() {
        let mut b = minimal(1);
        b.push(0xaa);
        assert_eq!(
            decode_tx(&b),
            Err(Malformed::TrailingBytes {
                expected: 100,
                got: 101
            })
        );
        let tx = decode_tx_prefix(&b).unwrap();
        assert_eq!(tx.encode(), b[..100].to_vec());
    }

    #[test]
    fn fee_examples_from_section_1_6() {
        assert_eq!(lean_fee(1), 26_000_000_000_000);
        assert_eq!(lean_fee(2), 31_000_000_000_000);
        assert_eq!(lean_fee(3), 36_000_000_000_000);
    }

    #[test]
    fn scalar_bounds_rejected() {
        let msg = [7u8; 32];
        let one = {
            let mut o = [0u8; 32];
            o[31] = 1;
            o
        };
        assert_eq!(
            recover_address(&msg, &SECP256K1_N, &one, 0),
            Err(Unrecoverable::RNotBelowOrder)
        );
        assert_eq!(
            recover_address(&msg, &one, &SECP256K1_N, 0),
            Err(Unrecoverable::SNotBelowOrder)
        );
        assert_eq!(
            recover_address(&msg, &[0u8; 32], &one, 0),
            Err(Unrecoverable::RZero)
        );
        assert_eq!(
            recover_address(&msg, &one, &[0u8; 32], 0),
            Err(Unrecoverable::SZero)
        );
        assert_eq!(
            recover_address(&msg, &one, &one, 27),
            Err(Unrecoverable::BadRecoveryId(27))
        );
    }
}
