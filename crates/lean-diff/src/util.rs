//! Shared helpers: signing keys, the engine-side genesis loader, 256-bit
//! scalar arithmetic on big-endian bytes, panic capture, hex display.

use crate::CHAIN_ID;
use alloy_primitives::{Address, B256};
use lean_core::{Acct, EngineCtx, FlatState};
use lean_native::{lane_domain, pubkey_to_address, LeanTx, Output, SIG_LEN};
use secp256k1::{PublicKey, SecretKey, SECP256K1};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::OnceLock;

/// SPEC.md §3.3 beneficiary.
pub fn beneficiary() -> Address {
    Address::with_last_byte(0xbe)
}

/// Engine context, parallel recovery on or off.
pub fn ctx(parallel: bool) -> EngineCtx {
    EngineCtx::new(CHAIN_ID, beneficiary()).with_parallel_recovery(parallel)
}

pub fn domain() -> B256 {
    lane_domain(CHAIN_ID)
}

/// secp256k1 group order n, big-endian.
pub const N: [u8; 32] = lean_oracle::primitives::SECP256K1_N;

/// A signing key and its address.
pub struct Key {
    pub sk: SecretKey,
    pub addr: Address,
}

impl Key {
    pub fn from_secret(b: [u8; 32]) -> Option<Key> {
        let sk = SecretKey::from_slice(&b).ok()?;
        let addr = pubkey_to_address(&PublicKey::from_secret_key(SECP256K1, &sk));
        Some(Key { sk, addr })
    }

    /// Sign a fan-out tx (lean-native's signer: RFC 6979, low-s, raw recid).
    pub fn sign(&self, nonce: u32, outputs: Vec<Output>) -> LeanTx {
        LeanTx::sign(nonce, outputs, &self.sk, &domain())
    }
}

/// Number of keys in the shared pool.
pub const KEY_POOL: usize = 96;

/// The shared, lazily derived key pool (secret i = keccak("lean-diff key" ‖ i)).
pub fn keys() -> &'static [Key] {
    static KEYS: OnceLock<Vec<Key>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let mut out = Vec::with_capacity(KEY_POOL);
        let mut i = 0u64;
        while out.len() < KEY_POOL {
            let mut pre = b"lean-diff key ".to_vec();
            pre.extend_from_slice(&i.to_be_bytes());
            if let Some(k) = Key::from_secret(alloy_primitives::keccak256(&pre).0) {
                out.push(k);
            }
            i += 1;
        }
        out
    })
}

/// The engine side's genesis: a line-for-line copy of `lean-lane-node`'s
/// `--fund-file` loader (`src/main.rs`, which is not a library function):
/// `lines()`, `trim()`, skip blank, `Address::from_str`, set (not add) to
/// `(0, fund_balance)`.
pub fn engine_genesis(fund_file: &str, fund_balance: u128) -> Result<FlatState, String> {
    let mut state = FlatState::default();
    for line in fund_file.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let a: Address = line
            .parse()
            .map_err(|e| format!("fund-file '{line}': {e}"))?;
        state.accounts.insert(
            a,
            Acct {
                nonce: 0,
                balance: fund_balance,
            },
        );
    }
    Ok(state)
}

/// Big-endian 256-bit `a - b` (wrapping).
pub fn sub_be(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let mut d = a[i] as i16 - b[i] as i16 - borrow;
        borrow = 0;
        if d < 0 {
            d += 256;
            borrow = 1;
        }
        out[i] = d as u8;
    }
    out
}

/// Big-endian 256-bit `a + b` (wrapping).
pub fn add_be(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut carry = 0u16;
    for i in (0..32).rev() {
        let s = a[i] as u16 + b[i] as u16 + carry;
        out[i] = s as u8;
        carry = s >> 8;
    }
    out
}

pub fn be32_from_u64(x: u64) -> [u8; 32] {
    let mut o = [0u8; 32];
    o[24..].copy_from_slice(&x.to_be_bytes());
    o
}

/// The high-s twin of a signature: `(r, n - s, v ^ 1)`.
pub fn high_s_twin(sig: &[u8; SIG_LEN]) -> [u8; SIG_LEN] {
    let mut s = [0u8; 32];
    s.copy_from_slice(&sig[32..64]);
    let mut out = *sig;
    out[32..64].copy_from_slice(&sub_be(&N, &s));
    out[64] ^= 1;
    out
}

/// Run `f`, turning a panic into `Err(message)`.
pub fn catch<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|p| {
        if let Some(s) = p.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = p.downcast_ref::<String>() {
            s.clone()
        } else {
            "non-string panic payload".to_string()
        }
    })
}

/// Hex of `b`, elided in the middle past `max` bytes (with the full length).
pub fn hex_short(b: &[u8], max: usize) -> String {
    if b.len() <= max {
        format!("0x{}", hex::encode(b))
    } else {
        format!(
            "0x{}…{} ({} bytes)",
            hex::encode(&b[..max / 2]),
            hex::encode(&b[b.len() - max / 2..]),
            b.len()
        )
    }
}

/// Worker thread count: `LEAN_DIFF_THREADS`, else available parallelism.
pub fn threads() -> usize {
    crate::env_count(
        "LEAN_DIFF_THREADS",
        std::thread::available_parallelism()
            .map(|n| n.get() as u64)
            .unwrap_or(4),
    )
    .max(1) as usize
}
