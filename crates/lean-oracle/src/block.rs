//! SPEC.md §2 — block wire form, framing, commitment and genesis.

use crate::primitives::{keccak256, Hash, GENESIS_TAG};

/// §2.1 fixed header fields (the first 48 bytes of the wire form).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// §2.1 `parent`: commitment of the parent block.
    pub parent: Hash,
    /// §2.1 `number`: u64 LE.
    pub number: u64,
    /// §2.1 `timestamp_ms`: u64 LE. Affects only the commitment (§2.5).
    pub timestamp_ms: u64,
}

/// A well-framed wire block (§2.1). Entries are raw, unvalidated bytes (§2.1: "Entry
/// contents are NOT validated by framing").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub header: Header,
    pub entries: Vec<Vec<u8>>,
}

/// Why a byte string is not well-framed (§2.1). Such a block is rejected as a whole and
/// has no commitment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// `len < 52`.
    TooShort { got: usize },
    /// Entry `index`'s 4-byte `len_i` prefix does not fit in the buffer.
    LenPrefixTruncated { index: u32 },
    /// Entry `index`'s body (`len_i` bytes) does not fit in the buffer.
    EntryTruncated { index: u32, len: u32 },
    /// The last entry does not end exactly at the end of the buffer.
    TrailingBytes { extra: usize },
}

/// §2.1 wire header length: parent(32) + number(8) + timestamp_ms(8) + n_txs(4).
pub const BLOCK_HEADER_LEN: usize = 52;

fn read_u32_le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn read_u64_le(b: &[u8], off: usize) -> u64 {
    let mut x = [0u8; 8];
    x.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(x)
}

/// §2.1: parse and check framing. Well-framed iff `len >= 52`, every `len_i` prefix and
/// body fits, and the last entry ends exactly at the end of the buffer.
pub fn decode_block(wire: &[u8]) -> Result<Block, BlockError> {
    if wire.len() < BLOCK_HEADER_LEN {
        return Err(BlockError::TooShort { got: wire.len() });
    }
    let mut parent = [0u8; 32];
    parent.copy_from_slice(&wire[0..32]);
    let number = read_u64_le(wire, 32);
    let timestamp_ms = read_u64_le(wire, 40);
    let n_txs = read_u32_le(wire, 48);

    // No preallocation from n_txs: it is attacker-controlled (up to 2^32 - 1).
    let mut entries = Vec::new();
    let mut off = BLOCK_HEADER_LEN;
    for index in 0..n_txs {
        // len_i prefix must fit.
        if wire.len() - off < 4 {
            return Err(BlockError::LenPrefixTruncated { index });
        }
        let len = read_u32_le(wire, off);
        off += 4;
        // entry body must fit.
        let len_usize = len as usize;
        if wire.len() - off < len_usize {
            return Err(BlockError::EntryTruncated { index, len });
        }
        entries.push(wire[off..off + len_usize].to_vec());
        off += len_usize;
    }
    // Last entry must end exactly at the end of the buffer (no trailing bytes).
    if off != wire.len() {
        return Err(BlockError::TrailingBytes {
            extra: wire.len() - off,
        });
    }
    Ok(Block {
        header: Header {
            parent,
            number,
            timestamp_ms,
        },
        entries,
    })
}

/// The bytes `wire[48..]`: `u32_le(n_txs) ‖ (u32_le(len_i) ‖ entry_i)*` (§2.1 / §2.2).
fn encode_entries(entries: &[Vec<u8>]) -> Vec<u8> {
    let n = u32::try_from(entries.len()).expect("§2.1: n_txs is u32");
    let mut out = Vec::new();
    out.extend_from_slice(&n.to_le_bytes());
    for e in entries {
        let len = u32::try_from(e.len()).expect("§2.1: len_i is u32");
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(e);
    }
    out
}

/// §2.1: encode a block to its wire form (used by tests and harnesses).
pub fn encode_block(header: &Header, entries: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&header.parent);
    out.extend_from_slice(&header.number.to_le_bytes());
    out.extend_from_slice(&header.timestamp_ms.to_le_bytes());
    out.extend_from_slice(&encode_entries(entries));
    out
}

/// §2.2 `txs_hash = keccak256(u32_le(n_txs) ‖ (u32_le(len_i) ‖ entry_i)*)`, i.e.
/// `keccak256(wire[48..])` of a well-framed block. Binds the exact entry bytes,
/// including bytes a prefix decode ignores.
pub fn txs_hash(entries: &[Vec<u8>]) -> Hash {
    keccak256(&encode_entries(entries))
}

/// §2.2 `commitment = keccak256(parent ‖ u64_le(number) ‖ u64_le(timestamp_ms) ‖ txs_hash)`
/// — an 80-byte preimage.
pub fn commitment(header: &Header, txs_hash: &Hash) -> Hash {
    let mut pre = Vec::with_capacity(80);
    pre.extend_from_slice(&header.parent);
    pre.extend_from_slice(&header.number.to_le_bytes());
    pre.extend_from_slice(&header.timestamp_ms.to_le_bytes());
    pre.extend_from_slice(txs_hash);
    debug_assert_eq!(pre.len(), 80);
    keccak256(&pre)
}

impl Block {
    /// §2.2 `txs_hash` of this block.
    pub fn txs_hash(&self) -> Hash {
        txs_hash(&self.entries)
    }

    /// §2.2 commitment of this block.
    pub fn commitment(&self) -> Hash {
        commitment(&self.header, &self.txs_hash())
    }
}

/// §2.3 `GENESIS_COMMITMENT(chain_id) = keccak256("ARC_LEAN_LANE_GENESIS" ‖ u64_be(chain_id))`.
/// Chain id is **big-endian** here (§0).
pub fn genesis_commitment(chain_id: u64) -> Hash {
    let mut pre = Vec::with_capacity(29);
    pre.extend_from_slice(GENESIS_TAG);
    pre.extend_from_slice(&chain_id.to_be_bytes());
    keccak256(&pre)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr() -> Header {
        Header {
            parent: [9u8; 32],
            number: 3,
            timestamp_ms: 4,
        }
    }

    #[test]
    fn roundtrip_and_hash_equals_keccak_of_tail() {
        let entries = vec![vec![], vec![1, 2, 3], vec![0x50]];
        let wire = encode_block(&hdr(), &entries);
        let b = decode_block(&wire).unwrap();
        assert_eq!(b.entries, entries);
        assert_eq!(b.header, hdr());
        assert_eq!(b.txs_hash(), keccak256(&wire[48..]));
    }

    #[test]
    fn framing_errors() {
        assert_eq!(
            decode_block(&[0u8; 51]),
            Err(BlockError::TooShort { got: 51 })
        );
        let mut wire = encode_block(&hdr(), &[vec![1, 2, 3]]);
        // Trailing byte.
        wire.push(0);
        assert_eq!(
            decode_block(&wire),
            Err(BlockError::TrailingBytes { extra: 1 })
        );
        wire.pop();
        // Body truncated.
        wire.pop();
        assert_eq!(
            decode_block(&wire),
            Err(BlockError::EntryTruncated { index: 0, len: 3 })
        );
        // n_txs says 2 but only one entry: second len prefix missing.
        let mut wire = encode_block(&hdr(), &[vec![1]]);
        wire[48..52].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(
            decode_block(&wire),
            Err(BlockError::LenPrefixTruncated { index: 1 })
        );
        // Huge n_txs on a tiny buffer: rejected, not allocated.
        let mut wire = encode_block(&hdr(), &[]);
        wire[48..52].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            decode_block(&wire),
            Err(BlockError::LenPrefixTruncated { index: 0 })
        );
    }

    #[test]
    fn exactly_52_bytes_zero_txs_is_well_framed() {
        let wire = encode_block(&hdr(), &[]);
        assert_eq!(wire.len(), 52);
        assert!(decode_block(&wire).unwrap().entries.is_empty());
    }
}
