//! The engine's data types: state, the per-block delta, the context a block
//! is processed under, and what processing returns. Nothing here does work
//! on the hot path — that is all in `engine.rs`.

use alloy_primitives::{keccak256, Address, B256};
use lean_native::{lane_domain, LeanTx};
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};

/// One account of the flat state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Acct {
    pub nonce: u64,
    pub balance: u128,
}

/// Number of [`AccountMap`] shards: shard = top `SHARD_BITS` bits of the
/// address's first byte, so shard order IS address order and a write set
/// sorted by address splits into one contiguous run per shard.
pub const SHARD_BITS: u32 = 6;
pub const SHARDS: usize = 1 << SHARD_BITS;

/// The account map of [`FlatState`]: `SHARDS` std `HashMap`s keyed by
/// address, sharded by the address's leading bits, each hashed with
/// [`AddressHasher`] (keyed, cheap) instead of SipHash. Sharding lets
/// [`crate::apply`] write disjoint shards on several threads (a write set is
/// sorted by address, so each shard's writes are one contiguous run), and a
/// growing state rehashes one 1/`SHARDS` slice at a time. The method set
/// mirrors the `HashMap` methods the node, bench and tests use. Iteration
/// order is unspecified (shard by shard; the digest sorts; snapshot order
/// was already hash-map order).
#[derive(Clone, Debug)]
pub struct AccountMap {
    shards: Box<[HashMap<Address, Acct, AddressHasherBuilder>]>,
}

/// The shard of address `a` (monotone in address order).
#[inline]
pub fn shard_of(a: &Address) -> usize {
    (a.0[0] >> (8 - SHARD_BITS)) as usize
}

impl Default for AccountMap {
    fn default() -> Self {
        Self::with_capacity(0)
    }
}

impl AccountMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Room for `n` accounts spread uniformly over the shards (+ slack).
    pub fn with_capacity(n: usize) -> Self {
        let per = Self::per_shard(n);
        Self {
            shards: (0..SHARDS)
                .map(|_| HashMap::with_capacity_and_hasher(per, AddressHasherBuilder::default()))
                .collect(),
        }
    }

    #[inline]
    fn per_shard(n: usize) -> usize {
        if n == 0 {
            0
        } else {
            n / SHARDS + n / SHARDS / 16 + 64
        }
    }

    /// Reserve room for `additional` more accounts (spread uniformly).
    pub fn reserve(&mut self, additional: usize) {
        let per = Self::per_shard(additional);
        for m in self.shards.iter_mut() {
            m.reserve(per);
        }
    }

    pub fn len(&self) -> usize {
        self.shards.iter().map(|m| m.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|m| m.is_empty())
    }

    #[inline]
    pub fn get(&self, a: &Address) -> Option<&Acct> {
        self.shards[shard_of(a)].get(a)
    }

    #[inline]
    pub fn get_mut(&mut self, a: &Address) -> Option<&mut Acct> {
        self.shards[shard_of(a)].get_mut(a)
    }

    #[inline]
    pub fn contains_key(&self, a: &Address) -> bool {
        self.shards[shard_of(a)].contains_key(a)
    }

    #[inline]
    pub fn insert(&mut self, a: Address, acct: Acct) -> Option<Acct> {
        self.shards[shard_of(&a)].insert(a, acct)
    }

    #[inline]
    pub fn remove(&mut self, a: &Address) -> Option<Acct> {
        self.shards[shard_of(a)].remove(a)
    }

    /// Every (address, account), shard by shard (unspecified order).
    pub fn iter(&self) -> impl Iterator<Item = (&Address, &Acct)> + '_ {
        self.shards.iter().flat_map(|m| m.iter())
    }

    /// The shards, in address order (for [`crate::apply`]).
    pub(crate) fn shards_mut(&mut self) -> &mut [HashMap<Address, Acct, AddressHasherBuilder>] {
        &mut self.shards
    }
}

impl<'a> IntoIterator for &'a AccountMap {
    type Item = (&'a Address, &'a Acct);
    type IntoIter =
        std::iter::Flatten<std::slice::Iter<'a, HashMap<Address, Acct, AddressHasherBuilder>>>;

    fn into_iter(self) -> Self::IntoIter {
        self.shards.iter().flatten()
    }
}

/// Keyed hasher builder for [`AccountMap`]: four 64-bit keys drawn from
/// std's `RandomState` (OS randomness) per map instance. Account addresses
/// are attacker-chosen (any tx can credit any address, zero amounts
/// included), so the state map must not use an unkeyed hash (FxHash): with
/// secret per-node keys, colliding addresses cannot be precomputed. The
/// strength is foldhash's ("minimally DoS-resistant", folded 64x64->128
/// multiplies of key-whitened input) — not a cryptographic PRF, same as
/// every fast keyed hasher.
#[derive(Clone, Copy)]
pub struct AddressHasherBuilder {
    k: [u64; 4],
}

impl Default for AddressHasherBuilder {
    fn default() -> Self {
        let rs = std::collections::hash_map::RandomState::new();
        Self {
            k: [
                rs.hash_one(0u64),
                rs.hash_one(1u64),
                rs.hash_one(2u64),
                rs.hash_one(3u64),
            ],
        }
    }
}

impl std::fmt::Debug for AddressHasherBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddressHasherBuilder")
            .finish_non_exhaustive()
    }
}

impl BuildHasher for AddressHasherBuilder {
    type Hasher = AddressHasher;

    #[inline]
    fn build_hasher(&self) -> AddressHasher {
        AddressHasher { k: self.k, h: 0 }
    }
}

#[inline(always)]
fn folded_multiply(x: u64, y: u64) -> u64 {
    let p = x as u128 * y as u128;
    (p as u64) ^ ((p >> 64) as u64)
}

/// See [`AddressHasherBuilder`]. Correct (deterministic per instance) for
/// any input; fast for a 20-byte address: two folded multiplies plus one to
/// finish. `write_usize` (the slice length prefix `Hash for [u8; 20]`
/// emits) is ignored: the key type has a fixed length.
pub struct AddressHasher {
    k: [u64; 4],
    h: u64,
}

impl Hasher for AddressHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let k = &self.k;
        let mut acc = self.h;
        let mut rest = bytes;
        while rest.len() >= 16 {
            let lo = u64::from_le_bytes(rest[0..8].try_into().unwrap());
            let hi = u64::from_le_bytes(rest[8..16].try_into().unwrap());
            acc = folded_multiply(lo ^ k[0] ^ acc, hi ^ k[1]);
            rest = &rest[16..];
        }
        if !rest.is_empty() {
            let mut buf = [0u8; 16];
            buf[..rest.len()].copy_from_slice(rest);
            let lo = u64::from_le_bytes(buf[0..8].try_into().unwrap());
            let hi = u64::from_le_bytes(buf[8..16].try_into().unwrap());
            acc = folded_multiply(lo ^ k[2] ^ acc, hi ^ k[3] ^ rest.len() as u64);
        }
        self.h = acc;
    }

    #[inline]
    fn write_usize(&mut self, _len: usize) {}

    #[inline]
    fn finish(&self) -> u64 {
        folded_multiply(self.h ^ self.k[1], self.k[0] | 1)
    }
}

/// Flat account state: address -> (nonce, balance). No trie, no root — the
/// lane is BFT-final, the CL's signed block commitment is the source of truth.
#[derive(Clone, Debug, Default)]
pub struct FlatState {
    pub accounts: AccountMap,
}

impl FlatState {
    pub fn get(&self, a: &Address) -> Acct {
        self.accounts.get(a).copied().unwrap_or_default()
    }

    pub fn credit(&mut self, a: Address, wei: u128) {
        match self.accounts.get_mut(&a) {
            Some(acct) => acct.balance += wei,
            None => {
                self.accounts.insert(
                    a,
                    Acct {
                        nonce: 0,
                        balance: wei,
                    },
                );
            }
        }
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
}

/// Read access to the pre-block state. `Sync` so an engine may read it from
/// worker threads.
pub trait StateView: Sync {
    /// The account at `a` (default — zero nonce, zero balance — if absent).
    fn get(&self, a: &Address) -> Acct;
}

impl StateView for FlatState {
    #[inline]
    fn get(&self, a: &Address) -> Acct {
        FlatState::get(self, a)
    }
}

/// A decoded, sender-recovered transaction in block order:
/// (sender, tx, tx hash = keccak256 of its canonical bytes).
pub type RecoveredTx = (Address, LeanTx, B256);

/// A decoded transaction whose sender is not recovered yet.
#[derive(Clone, Debug)]
pub struct Decoded {
    pub tx: LeanTx,
    /// keccak256 of the canonical bytes (the pool's tx hash).
    pub hash: B256,
}

/// Counters of one executed block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockOutcome {
    pub applied: usize,
    pub noops: usize,
    pub outputs: usize,
    pub fees_to_beneficiary: u128,
}

/// What executing one block does to the state, computed WITHOUT mutating it:
/// the post-value of every account the block writes, plus what the node needs
/// to finish the append (pool evictions, counters).
///
/// The write set is exactly the total STF's: every sender (a no-op's too — its
/// entry is created at the value it already had), every recipient of an
/// APPLIED tx, and the beneficiary. The outputs of a no-op tx are never
/// written. `FlatState::digest` covers the account count, so this set must be
/// the same on every path that executes a block — and it is, because the
/// engine's `execute` is the only code that computes it.
#[derive(Clone, Debug, Default)]
pub struct Delta {
    /// (address, post-state), sorted by address.
    pub writes: Vec<(Address, Acct)>,
    /// hashes of every tx in the block (applied or no-op): pool evictions
    pub evict: Vec<B256>,
    pub outcome: BlockOutcome,
}

impl Delta {
    /// Write the delta into `state` (same as [`crate::apply`]).
    pub fn apply_to(&self, state: &mut FlatState) {
        crate::apply(state, self)
    }

    /// Number of txs in the block (applied + no-op).
    pub fn txs(&self) -> usize {
        self.outcome.applied + self.outcome.noops
    }
}

/// The header fields of a wire block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockHeader {
    pub parent: B256,
    pub number: u64,
    pub timestamp_ms: u64,
}

/// A wire block after framing: header, commitment, and the tx bodies as
/// slices borrowed from the wire (nothing copied).
#[derive(Clone, Debug)]
pub struct Framed<'a> {
    pub header: BlockHeader,
    pub commitment: B256,
    pub txs: Vec<&'a [u8]>,
}

/// Everything a block is processed under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineCtx {
    pub chain_id: u64,
    /// `lane_domain(chain_id)`: the signing domain senders are recovered in.
    pub domain: B256,
    /// Receives every applied tx's fee, as one credit at end of block.
    pub beneficiary: Address,
    /// Recover senders on the rayon pool (order-preserving) instead of on the
    /// calling thread. Consensus-neutral: both give the identical result.
    pub parallel_recovery: bool,
}

impl EngineCtx {
    /// Context for lane `chain_id`, parallel recovery on.
    pub fn new(chain_id: u64, beneficiary: Address) -> Self {
        Self {
            chain_id,
            domain: lane_domain(chain_id),
            beneficiary,
            parallel_recovery: true,
        }
    }

    pub fn with_parallel_recovery(mut self, on: bool) -> Self {
        self.parallel_recovery = on;
        self
    }
}

/// The result of processing one block.
#[derive(Clone, Debug)]
pub struct Processed {
    pub header: BlockHeader,
    pub commitment: B256,
    pub delta: Delta,
}

/// Why a wire block was refused. Only FRAMING can fail a block: a tx that
/// does not decode or recover is skipped, and an invalid tx is a no-op
/// (total STF). The messages are the node's historical error strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// Shorter than the 52-byte header.
    TooShort,
    /// The length prefix of tx `i` is cut off.
    TruncatedLength(usize),
    /// The body of tx `i` is cut off.
    TruncatedBody(usize),
    /// Bytes remain after the last tx.
    TrailingBytes,
}

impl std::fmt::Display for BlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "wire block too short"),
            Self::TruncatedLength(i) => write!(f, "wire block truncated at tx {i} length"),
            Self::TruncatedBody(i) => write!(f, "wire block truncated at tx {i} body"),
            Self::TrailingBytes => write!(f, "trailing bytes after wire block"),
        }
    }
}

impl std::error::Error for BlockError {}

impl From<BlockError> for String {
    fn from(e: BlockError) -> Self {
        e.to_string()
    }
}
