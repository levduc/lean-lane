//! Deterministic synthetic workloads: a pre-funded in-memory state and a
//! corpus of pre-signed, budget-full wire blocks, generated once and cached
//! on disk keyed by every parameter (signing is never timed).
//!
//! Layout. Account index `i < SENDERS` is sender `i` (a real key, see
//! `util::sender_key`); every other index is a keyless synthetic address.
//! Block `b` carries one tx from each of senders `0..T_b` at that sender's
//! next nonce, so the blocks of a corpus apply in sequence with zero no-ops.
//! Recipients are uniform over ALL `accounts` indices (existing accounts — the lookup cost
//! the state size is there to exercise); amounts 1..=1000 gwei.

use crate::util::{sender_address, sender_key, synthetic_address, Rng};
use alloy_primitives::{Address, B256};
use lean_core::framing::{commitment_of_list, to_wire};
use lean_core::{Acct, BlockHeader, FlatState};
use lean_native::{lane_domain, lean_gas, LeanTx, Output};
use rayon::prelude::*;
use std::io::{Read, Write};
use std::path::Path;

/// Keyed senders in every state (the largest corpus, N=1 at 1G gas, needs
/// 38,461 txs per block).
pub const SENDERS: u64 = 40_000;
pub const CHAIN_ID: u64 = 1338;
/// Every account's genesis balance, in wei (1e6 ETH: no corpus drains it).
pub const FUND: u128 = 1_000_000 * 1_000_000_000_000_000_000;

pub fn beneficiary() -> Address {
    Address::with_last_byte(0xbe)
}

/// Outputs per tx: a fixed N, or a weighted mix (weights by tx count).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fanout {
    Fixed(usize),
    Mixed(Vec<(usize, u32)>),
}

impl Fanout {
    /// The roadmap mix: equal-by-tx over {1, 5, 10, 50, 100}.
    pub fn roadmap_mix() -> Self {
        Self::Mixed(vec![(1, 20), (5, 20), (10, 20), (50, 20), (100, 20)])
    }

    /// Short label used in scenario names.
    pub fn label(&self) -> String {
        match self {
            Self::Fixed(n) => n.to_string(),
            Self::Mixed(_) => "mix".into(),
        }
    }

    /// Full description (cache key, reports).
    pub fn spec(&self) -> String {
        match self {
            Self::Fixed(n) => n.to_string(),
            Self::Mixed(w) => w
                .iter()
                .map(|(n, w)| format!("{n}:{w}"))
                .collect::<Vec<_>>()
                .join(","),
        }
    }

    pub(crate) fn pick(&self, rng: &mut Rng) -> usize {
        match self {
            Self::Fixed(n) => *n,
            Self::Mixed(w) => {
                let total: u64 = w.iter().map(|(_, w)| *w as u64).sum();
                let mut x = rng.below(total);
                for (n, wt) in w {
                    if x < *wt as u64 {
                        return *n;
                    }
                    x -= *wt as u64;
                }
                w.last().unwrap().0
            }
        }
    }

    /// Sizes available to top a block up once the drawn one no longer fits.
    fn sizes_desc(&self) -> Vec<usize> {
        let mut v: Vec<usize> = match self {
            Self::Fixed(n) => vec![*n],
            Self::Mixed(w) => w.iter().map(|(n, _)| *n).collect(),
        };
        v.sort_unstable_by(|a, b| b.cmp(a));
        v
    }
}

#[derive(Clone, Debug)]
pub struct CorpusSpec {
    pub accounts: u64,
    pub fanout: Fanout,
    pub budget: u64,
    pub blocks: usize,
    pub seed: u64,
}

impl CorpusSpec {
    pub fn key(&self) -> String {
        format!(
            "v2-a{}-n{}-g{}-b{}-s{}",
            self.accounts,
            self.fanout.spec().replace([':', ','], "_"),
            self.budget,
            self.blocks,
            self.seed
        )
    }
}

/// Per-block counts, derived from the wire bytes.
#[derive(Clone, Copy, Debug, Default)]
pub struct BlockStats {
    pub txs: u64,
    pub payments: u64,
    pub gas: u64,
    pub bytes: u64,
}

pub struct Corpus {
    pub blocks: Vec<Vec<u8>>,
    pub stats: Vec<BlockStats>,
}

/// The sender addresses (index-aligned with `util::sender_key`).
pub fn senders() -> Vec<Address> {
    (0..SENDERS).into_par_iter().map(sender_address).collect()
}

pub fn account_address(idx: u64, senders: &[Address]) -> Address {
    if idx < senders.len() as u64 {
        senders[idx as usize]
    } else {
        synthetic_address(idx)
    }
}

/// The pre-funded state: `accounts` accounts, senders first.
pub fn build_state(accounts: u64, senders: &[Address]) -> FlatState {
    assert!(
        accounts >= senders.len() as u64,
        "state smaller than the sender set"
    );
    let mut s = FlatState::default();
    s.accounts.reserve(accounts as usize + 1);
    for i in 0..accounts {
        s.accounts.insert(
            account_address(i, senders),
            Acct {
                nonce: 0,
                balance: FUND,
            },
        );
    }
    s
}

fn stats_of(wire: &[u8]) -> BlockStats {
    let (_, txs) = lean_core::scan(wire).expect("corpus block framing");
    let mut st = BlockStats {
        txs: txs.len() as u64,
        bytes: wire.len() as u64,
        ..Default::default()
    };
    for t in txs {
        let n = u16::from_le_bytes([t[5], t[6]]) as usize;
        st.payments += n as u64;
        st.gas += lean_gas(n);
    }
    st
}

/// The tx sizes of block `b`: draw until the budget is full, then top up with
/// the largest size that still fits (a block is FULL when not even the
/// smallest available tx fits).
fn block_layout(spec: &CorpusSpec, b: usize) -> Vec<usize> {
    let mut rng = Rng::new(spec.seed ^ 0xb10c_0000 ^ ((b as u64) << 32));
    let sizes = spec.fanout.sizes_desc();
    let mut left = spec.budget;
    let mut out = Vec::new();
    loop {
        let n = spec.fanout.pick(&mut rng);
        if lean_gas(n) <= left {
            left -= lean_gas(n);
            out.push(n);
            continue;
        }
        match sizes.iter().find(|&&n| lean_gas(n) <= left) {
            Some(&n) => {
                left -= lean_gas(n);
                out.push(n);
            }
            None => break,
        }
    }
    out
}

fn generate(spec: &CorpusSpec, senders: &[Address]) -> Vec<Vec<u8>> {
    let domain = lane_domain(CHAIN_ID);
    let mut parent: B256 = lean_core::genesis_commitment(CHAIN_ID);
    let mut blocks = Vec::with_capacity(spec.blocks);
    // next nonce of each sender: a mixed layout's tx count varies per block,
    // so sender `t` is not in every block
    let mut nonces = vec![0u32; SENDERS as usize];
    for b in 0..spec.blocks {
        let layout = block_layout(spec, b);
        assert!(
            layout.len() as u64 <= SENDERS,
            "{} txs/block exceeds the {SENDERS} keyed senders",
            layout.len()
        );
        let txs: Vec<Vec<u8>> = layout
            .par_iter()
            .enumerate()
            .map(|(t, &n)| {
                let mut rng = Rng::new(spec.seed ^ ((b as u64) << 40) ^ ((t as u64) << 1));
                let outputs = (0..n)
                    .map(|_| Output {
                        to: account_address(rng.below(spec.accounts), senders),
                        amount: 1 + rng.below(1000),
                    })
                    .collect::<Vec<_>>();
                LeanTx::sign(nonces[t], outputs, &sender_key(t as u64), &domain).encode()
            })
            .collect();
        for n in &mut nonces[..layout.len()] {
            *n += 1;
        }
        let h = BlockHeader {
            parent,
            number: b as u64 + 1,
            timestamp_ms: 1_000 * (b as u64 + 1),
        };
        parent = commitment_of_list(&h, &txs);
        blocks.push(to_wire(&h, &txs));
    }
    blocks
}

const MAGIC: &[u8; 4] = b"LBC1";

pub(crate) fn read_blob(path: &Path, key: &str) -> Option<Vec<Vec<u8>>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .read_to_end(&mut bytes)
        .ok()?;
    let mut at = 0usize;
    let take = |at: &mut usize, n: usize| -> Option<&[u8]> {
        let s = bytes.get(*at..*at + n)?;
        *at += n;
        Some(s)
    };
    if take(&mut at, 4)? != MAGIC {
        return None;
    }
    let klen = u32::from_le_bytes(take(&mut at, 4)?.try_into().ok()?) as usize;
    if take(&mut at, klen)? != key.as_bytes() {
        return None;
    }
    let n = u32::from_le_bytes(take(&mut at, 4)?.try_into().ok()?) as usize;
    let mut blocks = Vec::with_capacity(n);
    for _ in 0..n {
        let len = u64::from_le_bytes(take(&mut at, 8)?.try_into().ok()?) as usize;
        blocks.push(take(&mut at, len)?.to_vec());
    }
    (at == bytes.len()).then_some(blocks)
}

pub(crate) fn write_blob(path: &Path, key: &str, blocks: &[Vec<u8>]) -> std::io::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("tmp");
    let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    f.write_all(MAGIC)?;
    f.write_all(&(key.len() as u32).to_le_bytes())?;
    f.write_all(key.as_bytes())?;
    f.write_all(&(blocks.len() as u32).to_le_bytes())?;
    for b in blocks {
        f.write_all(&(b.len() as u64).to_le_bytes())?;
        f.write_all(b)?;
    }
    f.into_inner()?.sync_all()?;
    std::fs::rename(tmp, path)
}

/// Load the corpus for `spec` from `cache_dir`, generating (and caching) it
/// on a miss. Returns the corpus and whether it came from the cache.
pub fn load_or_build(spec: &CorpusSpec, senders: &[Address], cache_dir: &Path) -> (Corpus, bool) {
    let key = spec.key();
    let path = cache_dir.join(format!("{key}.bin"));
    let (blocks, hit) = match read_blob(&path, &key) {
        Some(b) if b.len() == spec.blocks => (b, true),
        _ => {
            let b = generate(spec, senders);
            if let Err(e) = write_blob(&path, &key, &b) {
                eprintln!("lean-bench: could not cache {}: {e}", path.display());
            }
            (b, false)
        }
    };
    let stats = blocks.iter().map(|b| stats_of(b)).collect();
    (Corpus { blocks, stats }, hit)
}
