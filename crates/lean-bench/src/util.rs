//! Small helpers: a version-stable RNG, key/address derivation, RSS, stats.

use alloy_primitives::Address;
use secp256k1::SecretKey;

/// splitmix64: tiny, fast, and — unlike `rand`'s generators — guaranteed
/// stable across crate versions, so cached corpora never silently change.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (n > 0; the modulo bias is irrelevant here).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// Deterministic secret key of sender `i` (same scheme as the node's tests).
pub fn sender_key(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).expect("valid key")
}

pub fn sender_address(i: u64) -> Address {
    let pk = sender_key(i).public_key(secp256k1::SECP256K1);
    lean_native::pubkey_to_address(&pk)
}

/// A synthetic (keyless) account address for state index `i`: 20 bytes of
/// splitmix output — uniformly spread, so hash-map behaviour is realistic.
pub fn synthetic_address(i: u64) -> Address {
    let mut r = Rng::new(i ^ 0x5eed_0000_a11c_e000);
    let mut b = [0u8; 20];
    b[..8].copy_from_slice(&r.next_u64().to_le_bytes());
    b[8..16].copy_from_slice(&r.next_u64().to_le_bytes());
    b[16..].copy_from_slice(&r.next_u64().to_le_bytes()[..4]);
    Address::from(b)
}

fn status_kb(field: &str) -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find(|l| l.starts_with(field))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Peak resident set (VmHWM) in MB.
pub fn peak_rss_mb() -> f64 {
    status_kb("VmHWM:").unwrap_or(0) as f64 / 1024.0
}

/// Current resident set (VmRSS) in MB.
pub fn rss_mb() -> f64 {
    status_kb("VmRSS:").unwrap_or(0) as f64 / 1024.0
}

/// Reset VmHWM to the current RSS (Linux `clear_refs` 5), so the next
/// `peak_rss_mb` is the peak of what follows. Best effort.
pub fn reset_peak_rss() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

pub fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let m = v.len() / 2;
    if v.len() % 2 == 1 {
        v[m]
    } else {
        (v[m - 1] + v[m]) / 2.0
    }
}

/// Where cached corpora live: `$CARGO_TARGET_DIR/lean-bench-cache`, else the
/// workspace's `target/lean-bench-cache`.
pub fn default_cache_dir() -> std::path::PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target")
                .to_path_buf()
        });
    target.join("lean-bench-cache")
}
