//! Signing support for tests and differential harnesses. NOT normative: SPEC.md defines
//! how signatures are checked (§1.5), not how they are produced. Appendix A says the
//! vectors use RFC 6979 deterministic nonces with no extra entropy (libsecp256k1 default),
//! which is also what k256 does, so these reproduce the vector bytes.

use k256::ecdsa::SigningKey;
use k256::elliptic_curve::sec1::ToEncodedPoint;

use crate::primitives::Address;
use crate::tx::{pubkey_to_address, signing_hash, Output, Tx};

/// A 32-byte big-endian secret whose last byte is `k` (Appendix A's `k1`, `k2`, …).
pub fn small_secret(k: u8) -> [u8; 32] {
    let mut s = [0u8; 32];
    s[31] = k;
    s
}

/// §1.5 address of a secret key: keccak of the uncompressed public key, last 20 bytes.
pub fn address_of(secret: &[u8; 32]) -> Address {
    let sk = SigningKey::from_bytes(secret.into()).expect("valid secret");
    let point = sk.verifying_key().as_affine().to_encoded_point(false);
    pubkey_to_address(&point.as_bytes()[1..65])
}

/// Build and sign a tx: §1.4 signing hash, RFC 6979 low-s signature, raw recovery id as `v`.
pub fn sign_tx(secret: &[u8; 32], nonce: u32, outputs: Vec<Output>, chain_id: u64) -> Tx {
    let mut tx = Tx {
        nonce,
        outputs,
        r: [0u8; 32],
        s: [0u8; 32],
        v: 0,
    };
    let msg = signing_hash(&tx, chain_id);
    let sk = SigningKey::from_bytes(secret.into()).expect("valid secret");
    let (sig, recid) = sk
        .sign_prehash_recoverable(&msg)
        .expect("signing a 32-byte prehash");
    let bytes = sig.to_bytes();
    tx.r.copy_from_slice(&bytes[0..32]);
    tx.s.copy_from_slice(&bytes[32..64]);
    tx.v = recid.to_byte();
    tx
}
