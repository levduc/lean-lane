//! Lean fan-out native transfer (tx type 0x50) — spammer-side encoder/signer.
//!
//! Wire (canonical = EIP-2718 bytes, fixed-width, no RLP):
//!   [0x50][nonce u32 LE][n_outputs u16 LE]([to 20B][amount u64 LE, gwei units])xN[sig 65B r||s||v]
//! Signing hash: keccak(domain || bytes-before-sig), where
//!   domain = keccak("ARC_LEAN_NATIVE_TRANSFER"(24B) || chain_id u64 BE).
//! v is the RAW recovery id (0/1). Tx hash = keccak(full canonical bytes).
//!
//! This mirrors ~/reth-fork/crates/lean-native (the source of truth). The
//! `pinned_vector_matches_fork` test reproduces the fork's `gen_vector` example
//! byte-for-byte — if either side changes the format, that test breaks FIRST.

use alloy_primitives::{keccak256, Address, B256};
use k256::ecdsa::SigningKey;

pub const LEAN_TX_TYPE: u8 = 0x50;

/// domain = keccak("ARC_LEAN_NATIVE_TRANSFER" || chain_id BE)
pub fn lane_domain(chain_id: u64) -> B256 {
    let mut buf = [0u8; 32];
    buf[..24].copy_from_slice(b"ARC_LEAN_NATIVE_TRANSFER");
    buf[24..].copy_from_slice(&chain_id.to_be_bytes());
    keccak256(buf)
}

fn body_bytes(nonce: u32, outputs: &[(Address, u64)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(7 + 28 * outputs.len());
    out.push(LEAN_TX_TYPE);
    out.extend_from_slice(&nonce.to_le_bytes());
    out.extend_from_slice(&(outputs.len() as u16).to_le_bytes());
    for (to, amount) in outputs {
        out.extend_from_slice(to.as_slice());
        out.extend_from_slice(&amount.to_le_bytes());
    }
    out
}

/// Build + sign a lean fan-out tx. Returns (canonical bytes, tx hash).
pub fn build_fanout_tx(
    nonce: u32,
    outputs: &[(Address, u64)],
    signer: &SigningKey,
    domain: &B256,
) -> (Vec<u8>, B256) {
    let mut bytes = body_bytes(nonce, outputs);
    let mut pre = Vec::with_capacity(32 + bytes.len());
    pre.extend_from_slice(domain.as_slice());
    pre.extend_from_slice(&bytes);
    let signing_hash = keccak256(&pre);
    let (sig, recid) = signer
        .sign_prehash_recoverable(signing_hash.as_slice())
        .expect("prehash signing cannot fail on a 32-byte digest");
    bytes.extend_from_slice(&sig.to_bytes());
    bytes.push(recid.to_byte());
    let hash = keccak256(&bytes);
    (bytes, hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte-for-byte against ~/reth-fork `cargo run -p lean-native --example
    /// gen_vector` (sk=0x..01, chain 1338, nonce 7, two outputs). Pins wire
    /// format, signing domain, RFC6979 determinism, and v convention across
    /// the two independent implementations.
    #[test]
    fn pinned_vector_matches_fork() {
        let mut sk = [0u8; 32];
        sk[31] = 1;
        let signer = SigningKey::from_slice(&sk).unwrap();
        let outputs = [
            (Address::repeat_byte(0x11), 5u64),
            (Address::repeat_byte(0x22), 0x0102030405060708u64),
        ];
        let (bytes, _hash) = build_fanout_tx(7, &outputs, &signer, &lane_domain(1338));
        let expected = "500700000002001111111111111111111111111111111111111111050000000000000022222222222222222222222222222222222222220807060504030201e492ebc6e4c59a8d6b57c10c4817bf89af46cd0d9a6022e030663286696a14d955fbc51627de27715e4d05f65bf65c42d3c6e1a38b42e099ff686004922f55b801";
        assert_eq!(alloy_primitives::hex::encode(&bytes), expected);
    }

    #[test]
    fn size_is_72_plus_28n() {
        let signer = SigningKey::from_slice(&B256::repeat_byte(0x42).0).unwrap();
        for n in [1usize, 10, 100] {
            let outputs: Vec<_> = (0..n).map(|i| (Address::repeat_byte(i as u8), 1u64)).collect();
            let (bytes, _) = build_fanout_tx(0, &outputs, &signer, &lane_domain(1338));
            assert_eq!(bytes.len(), 72 + 28 * n);
        }
    }
}
