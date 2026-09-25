//! SPEC.md §0 — conventions and primitives, plus the fixed constants of §1.1 and §3.3.

use sha3::{Digest, Keccak256};

/// §0: "Address = 20 raw bytes". Ordering of `[u8; 20]` in Rust is lexicographic on the
/// bytes, which is exactly the §0 sort order.
pub type Address = [u8; 20];

/// A 32-byte keccak output.
pub type Hash = [u8; 32];

/// §0: Keccak-256 as used by Ethereum (pre-NIST padding). `sha3::Keccak256` is that
/// function (NOT `Sha3_256`).
pub fn keccak256(data: &[u8]) -> Hash {
    let mut hasher = Keccak256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// §0: secp256k1 group order `n`, big-endian.
pub const SECP256K1_N: [u8; 32] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE,
    0xBA, 0xAE, 0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C, 0xD0, 0x36, 0x41, 0x41,
];

// ---- §1.1 constants -------------------------------------------------------------------

/// §1.1 `LEAN_TX_TYPE`: first byte of every canonical tx.
pub const LEAN_TX_TYPE: u8 = 0x50;
/// §1.1 `MAX_OUTPUTS`.
pub const MAX_OUTPUTS: usize = 10_000;
/// §1.1 `HEADER_LEN` = type(1) + nonce(4) + n_outputs(2).
pub const HEADER_LEN: usize = 7;
/// §1.1 `OUTPUT_LEN` = address(20) + amount(8).
pub const OUTPUT_LEN: usize = 28;
/// §1.1 `SIG_LEN` = r(32) + s(32) + v(1).
pub const SIG_LEN: usize = 65;
/// §1.1 `AMOUNT_UNIT`: wei per wire amount unit (1 gwei).
pub const AMOUNT_UNIT: u128 = 1_000_000_000;
/// §1.1 `GAS_BASE`.
pub const GAS_BASE: u128 = 21_000;
/// §1.1 `GAS_PER_OUTPUT`.
pub const GAS_PER_OUTPUT: u128 = 5_000;
/// §1.1 `GAS_PRICE` (wei), protocol-fixed.
pub const GAS_PRICE: u128 = 1_000_000_000;

/// §1.4 domain tag (24 ASCII bytes).
pub const LANE_DOMAIN_TAG: &[u8] = b"ARC_LEAN_NATIVE_TRANSFER";
/// §2.3 genesis tag (21 ASCII bytes).
pub const GENESIS_TAG: &[u8] = b"ARC_LEAN_LANE_GENESIS";

/// §1.4: default chain id.
pub const DEFAULT_CHAIN_ID: u64 = 1338;

/// §3.2: default `--fund-balance` in wei (1e15).
pub const DEFAULT_FUND_BALANCE: u128 = 1_000_000_000_000_000;

/// §3.3: `BENEFICIARY = 0x00000000000000000000000000000000000000be`.
pub const BENEFICIARY: Address = {
    let mut a = [0u8; 20];
    a[19] = 0xbe;
    a
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keccak_of_empty_matches_section_0() {
        assert_eq!(
            hex::encode(keccak256(b"")),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
    }

    #[test]
    fn tag_lengths_match_spec() {
        assert_eq!(LANE_DOMAIN_TAG.len(), 24); // §1.4
        assert_eq!(GENESIS_TAG.len(), 21); // §2.3
    }
}
