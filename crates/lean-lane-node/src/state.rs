//! Flat account state: address -> (nonce, balance). No trie, no root — the
//! lane is BFT-final, the CL's signed block commitment is the source of truth.
//! Snapshots are full serializations of the map tagged with the block
//! commitment they correspond to; recovery = newest snapshot + log replay.

use alloy_primitives::{keccak256, Address, B256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Acct {
    pub nonce: u64,
    pub balance: u128,
}

#[derive(Clone, Debug, Default)]
pub struct FlatState {
    pub accounts: HashMap<Address, Acct>,
}

impl FlatState {
    pub fn get(&self, a: &Address) -> Acct {
        self.accounts.get(a).copied().unwrap_or_default()
    }

    pub fn credit(&mut self, a: Address, wei: u128) {
        self.accounts.entry(a).or_default().balance += wei;
    }

    /// Deterministic digest over sorted entries — for crash/recovery equality
    /// checks (NOT a consensus commitment).
    pub fn digest(&self) -> B256 {
        let mut entries: Vec<(&Address, &Acct)> = self.accounts.iter().collect();
        entries.sort_by_key(|(a, _)| **a);
        let mut buf = Vec::with_capacity(entries.len() * 44);
        for (a, acct) in entries {
            buf.extend_from_slice(a.as_slice());
            buf.extend_from_slice(&acct.nonce.to_le_bytes());
            buf.extend_from_slice(&acct.balance.to_le_bytes());
        }
        keccak256(&buf)
    }

    /// Serialize the full map with the commitment + number it corresponds to.
    /// Written to a tmp file then renamed (atomic on the same filesystem).
    pub fn write_snapshot(
        &self,
        dir: &Path,
        commitment: B256,
        number: u64,
        timestamp_ms: u64,
    ) -> std::io::Result<PathBuf> {
        let mut buf = Vec::with_capacity(56 + self.accounts.len() * 44);
        buf.extend_from_slice(commitment.as_slice());
        buf.extend_from_slice(&number.to_le_bytes());
        buf.extend_from_slice(&timestamp_ms.to_le_bytes());
        buf.extend_from_slice(&(self.accounts.len() as u64).to_le_bytes());
        for (a, acct) in &self.accounts {
            buf.extend_from_slice(a.as_slice());
            buf.extend_from_slice(&acct.nonce.to_le_bytes());
            buf.extend_from_slice(&acct.balance.to_le_bytes());
        }
        let tmp = dir.join(format!(".snapshot-{number}.tmp"));
        let fin = dir.join(format!("snapshot-{number}.bin"));
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&buf)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &fin)?;
        Ok(fin)
    }

    /// Load the newest complete snapshot in `dir`, returning
    /// (state, commitment, block number). None if no snapshot exists.
    /// Returns (state, commitment, number, timestamp_ms).
    pub fn load_newest_snapshot(dir: &Path) -> std::io::Result<Option<(Self, B256, u64, u64)>> {
        let mut best: Option<(u64, PathBuf)> = None;
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(numstr) =
                name.strip_prefix("snapshot-").and_then(|s| s.strip_suffix(".bin"))
            {
                if let Ok(n) = numstr.parse::<u64>() {
                    if best.as_ref().map_or(true, |(b, _)| n > *b) {
                        best = Some((n, e.path()));
                    }
                }
            }
        }
        let Some((_, path)) = best else { return Ok(None) };
        let mut bytes = Vec::new();
        std::fs::File::open(&path)?.read_to_end(&mut bytes)?;
        if bytes.len() < 56 {
            return Ok(None);
        }
        let commitment = B256::from_slice(&bytes[0..32]);
        let number = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
        let timestamp_ms = u64::from_le_bytes(bytes[40..48].try_into().unwrap());
        let count = u64::from_le_bytes(bytes[48..56].try_into().unwrap()) as usize;
        if bytes.len() != 56 + count * 44 {
            return Ok(None); // torn snapshot (tmp+rename should prevent this)
        }
        let mut accounts = HashMap::with_capacity(count);
        let mut at = 56;
        for _ in 0..count {
            let a = Address::from_slice(&bytes[at..at + 20]);
            let nonce = u64::from_le_bytes(bytes[at + 20..at + 28].try_into().unwrap());
            let balance = u128::from_le_bytes(bytes[at + 28..at + 44].try_into().unwrap());
            accounts.insert(a, Acct { nonce, balance });
            at += 44;
        }
        Ok(Some((Self { accounts }, commitment, number, timestamp_ms)))
    }
}
