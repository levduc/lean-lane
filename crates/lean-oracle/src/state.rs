//! SPEC.md §3 — flat state, genesis funding, digest.

use std::collections::BTreeMap;

use crate::primitives::{keccak256, Address, Hash};

/// §3.1 `Acct { nonce: u64, balance: u128 (wei) }`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Account {
    pub nonce: u64,
    pub balance: u128,
}

/// §3.1 `state: Address → Acct`. A `BTreeMap` so iteration is already in §0 address order.
///
/// Presence matters (§3.1): an entry explicitly holding `(0, 0)` is distinct from absence
/// for the digest. Entries are never deleted (§4.3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub accounts: BTreeMap<Address, Account>,
}

impl State {
    /// Empty state (no fund file, §3.2).
    pub fn new() -> Self {
        Self::default()
    }

    /// §3.1 read rule: absent address reads as `(0, 0)`.
    pub fn get(&self, a: &Address) -> Account {
        self.accounts.get(a).copied().unwrap_or_default()
    }

    /// §4.1 `apply`: set `state[a] = acct` (set, not add).
    pub fn set(&mut self, a: Address, acct: Account) {
        self.accounts.insert(a, acct);
    }

    /// Whether the address has an explicit entry (digest-relevant, §3.1 / §4.3).
    pub fn contains(&self, a: &Address) -> bool {
        self.accounts.contains_key(a)
    }

    /// Number of explicit entries.
    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    /// Whether the state has no explicit entries.
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }
}

/// §3.4 `digest = keccak256(‖ over entries sorted by address of (address(20) ‖
/// u64_le(nonce) ‖ u128_le(balance)))` — 44 bytes per entry, no count, no separator.
/// Empty state ⇒ `keccak256("")`.
pub fn state_digest(state: &State) -> Hash {
    let mut pre = Vec::with_capacity(44 * state.accounts.len());
    // BTreeMap iterates in ascending key order = ascending lexicographic address order.
    for (addr, acct) in &state.accounts {
        pre.extend_from_slice(addr);
        pre.extend_from_slice(&acct.nonce.to_le_bytes());
        pre.extend_from_slice(&acct.balance.to_le_bytes());
    }
    keccak256(&pre)
}

/// A fatal fund-file error (§3.2): a non-blank line that is not a 20-byte hex address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FundFileError {
    /// 1-based line number.
    pub line: usize,
    /// The trimmed line content.
    pub content: String,
}

/// §3.2 genesis state from the `--fund-file` contents and `--fund-balance` (wei).
///
/// - one address per line; each line whitespace-trimmed; blank lines skipped;
/// - every other line MUST be a 20-byte hex address: `0x` optional, hex digits
///   case-insensitive, checksum not enforced; anything else (incl. `#` comments) is fatal;
/// - each listed address is **set** to `(nonce 0, balance = fund_balance)`; duplicates collapse.
///
/// "No file" is represented by the empty string (⇒ empty genesis state).
pub fn genesis_state(fund_file_contents: &str, fund_balance: u128) -> Result<State, FundFileError> {
    let mut state = State::new();
    for (i, raw_line) in fund_file_contents.lines().enumerate() {
        // Rust `str::trim` = Unicode White_Space (SPEC-QUESTIONS.md Q4).
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let addr = parse_address(line).ok_or_else(|| FundFileError {
            line: i + 1,
            content: line.to_string(),
        })?;
        state.set(
            addr,
            Account {
                nonce: 0,
                balance: fund_balance,
            },
        );
    }
    Ok(state)
}

/// §3.2 address syntax: optional `0x` prefix, then exactly 40 hex digits of either case.
/// (Uppercase `0X` prefix: see SPEC-QUESTIONS.md Q3.)
fn parse_address(s: &str) -> Option<Address> {
    let digits = s.strip_prefix("0x").unwrap_or(s);
    if digits.len() != 40 || !digits.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let bytes = hex::decode(digits).ok()?;
    let mut a = [0u8; 20];
    a.copy_from_slice(&bytes);
    Some(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "7E5F4552091A69125d5DfCb7b8C2659029395Bdf";

    #[test]
    fn empty_digest() {
        assert_eq!(
            hex::encode(state_digest(&State::new())),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
    }

    #[test]
    fn fund_file_syntax() {
        let file = format!(
            "\n  0x{A}  \n{}\n\t0x{}\n\n",
            A.to_lowercase(),
            A.to_uppercase()
        );
        let s = genesis_state(&file, 42).unwrap();
        // All three spellings are the same address: duplicates collapse.
        assert_eq!(s.len(), 1);
        let addr: Address = hex::decode(A).unwrap().try_into().unwrap();
        assert_eq!(
            s.get(&addr),
            Account {
                nonce: 0,
                balance: 42
            }
        );
    }

    #[test]
    fn fund_file_fatal_lines() {
        assert_eq!(
            genesis_state(&format!("0x{A}\n# comment\n"), 1),
            Err(FundFileError {
                line: 2,
                content: "# comment".into()
            })
        );
        assert!(genesis_state("0x1234", 1).is_err()); // too short
        assert!(genesis_state(&format!("0x{A}00"), 1).is_err()); // too long
        assert!(genesis_state(&format!("0x{}zz", &A[..38]), 1).is_err()); // non-hex
        assert!(genesis_state(&format!("0x{A} 0x{A}"), 1).is_err()); // two per line
    }

    #[test]
    fn no_file_is_empty_state() {
        assert!(genesis_state("", 5).unwrap().is_empty());
    }

    #[test]
    fn digest_is_presence_sensitive() {
        let mut s = State::new();
        let d0 = state_digest(&s);
        s.set([1u8; 20], Account::default());
        assert_ne!(state_digest(&s), d0);
    }
}
