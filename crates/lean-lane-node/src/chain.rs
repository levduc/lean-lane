//! Minimal lane chain: a block is `{parent_commitment, number, timestamp_ms,
//! tx_list (2718 bytes verbatim)}` — no Ethereum header, no MPT root, no
//! receipts root, no bloom.
//!
//! ## Block commitment (the thing a CL would sign) — v2, FRAMING-BOUND
//!
//! ```text
//! txs_hash   = keccak256( [n_txs u32 LE] ‖ ( [len_i u32 LE] ‖ tx_i_bytes )* )
//!              -- i.e. keccak over EXACTLY the wire form's tx section --
//! commitment = keccak256( parent_commitment(32B)
//!                       ‖ number       (u64 LE)
//!                       ‖ timestamp_ms (u64 LE)
//!                       ‖ txs_hash     (32B) )
//! ```
//!
//! v1 hashed the bare concatenation of tx bodies, which does NOT bind the
//! framing: two different tx lists with the same concatenation (a moved
//! boundary) shared a commitment — an equivocation hole found by the CL-side
//! tests. v2's preimage includes the count and every length, so the
//! commitment binds the exact list structure (and equals a hash over the
//! wire bytes' tail, `wire[48..]`).
//!
//! `GENESIS_COMMITMENT = keccak256("ARC_LEAN_LANE_GENESIS" ‖ chain_id u64 BE)`.
//! Tx bytes are the canonical 2718 forms (self-delimiting), so `txs_hash`
//! commits to the exact list and order. Execution results (balances) are NOT
//! committed per block — BFT finality on the ordering makes state a pure
//! function of it; a state digest can be exchanged out-of-band every k blocks
//! (the snapshot's digest) if operators want cross-checks.
//!
//! ## Append-only log record
//!
//! ```text
//! record  = [u32 LE payload_len] payload [u32 LE payload_len]   (echo tail)
//! payload = [32B parent][8B number LE][8B timestamp_ms LE][32B commitment]
//!           [u32 LE n_tx] n_tx × ( [u32 LE len] tx_bytes )
//! ```
//! The trailing length echo detects torn tail writes: on recovery the log is
//! replayed until the first record whose bounds or echo don't check out, and
//! truncated there. (A production log would add a CRC per record; the echo
//! catches truncation — the crash mode fsync-per-block leaves us with.)

use alloy_primitives::{keccak256, B256};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

pub fn genesis_commitment(chain_id: u64) -> B256 {
    let mut buf = Vec::with_capacity(29);
    buf.extend_from_slice(b"ARC_LEAN_LANE_GENESIS");
    buf.extend_from_slice(&chain_id.to_be_bytes());
    keccak256(&buf)
}

/// A wire block's header fields plus its commitment, obtained without
/// materialising the transaction list (see `LeanBlock::commitment_of_wire`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WireHeader {
    pub parent: B256,
    pub number: u64,
    pub timestamp_ms: u64,
    pub commitment: B256,
}

#[derive(Clone, Debug)]
pub struct LeanBlock {
    pub parent: B256,
    pub number: u64,
    pub timestamp_ms: u64,
    /// Canonical 2718 tx bytes, verbatim.
    pub txs: Vec<Vec<u8>>,
    pub commitment: B256,
}

impl LeanBlock {
    pub fn new(parent: B256, number: u64, timestamp_ms: u64, txs: Vec<Vec<u8>>) -> Self {
        // FRAMED preimage: count + per-tx lengths bound into the hash
        // (identical bytes to the wire form's tail).
        let mut cat = Vec::with_capacity(4 + txs.iter().map(|t| 4 + t.len()).sum::<usize>());
        cat.extend_from_slice(&(txs.len() as u32).to_le_bytes());
        for t in &txs {
            cat.extend_from_slice(&(t.len() as u32).to_le_bytes());
            cat.extend_from_slice(t);
        }
        let txs_hash = keccak256(&cat);
        let mut pre = Vec::with_capacity(80);
        pre.extend_from_slice(parent.as_slice());
        pre.extend_from_slice(&number.to_le_bytes());
        pre.extend_from_slice(&timestamp_ms.to_le_bytes());
        pre.extend_from_slice(txs_hash.as_slice());
        let commitment = keccak256(&pre);
        Self {
            parent,
            number,
            timestamp_ms,
            txs,
            commitment,
        }
    }

    /// Canonical CONSENSUS wire form (the SSZ-carried bytes; commitment NOT
    /// included — receivers recompute it):
    /// `[parent 32][number u64 LE][timestamp_ms u64 LE][n_txs u32 LE]([len u32 LE][tx])*`
    pub fn to_wire_bytes(&self) -> Vec<u8> {
        let tx_bytes: usize = self.txs.iter().map(|t| 4 + t.len()).sum();
        let mut w = Vec::with_capacity(52 + tx_bytes);
        w.extend_from_slice(self.parent.as_slice());
        w.extend_from_slice(&self.number.to_le_bytes());
        w.extend_from_slice(&self.timestamp_ms.to_le_bytes());
        w.extend_from_slice(&(self.txs.len() as u32).to_le_bytes());
        for t in &self.txs {
            w.extend_from_slice(&(t.len() as u32).to_le_bytes());
            w.extend_from_slice(t);
        }
        w
    }

    /// Validate the wire FRAMING and return the header fields plus the tx
    /// count. Walks the length table without copying a single tx body, so it
    /// is the cheap half of a decode.
    fn scan_wire(bytes: &[u8]) -> Result<(B256, u64, u64, usize), String> {
        if bytes.len() < 52 {
            return Err("wire block too short".into());
        }
        let parent = B256::from_slice(&bytes[0..32]);
        let number = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
        let timestamp_ms = u64::from_le_bytes(bytes[40..48].try_into().unwrap());
        let n_tx = u32::from_le_bytes(bytes[48..52].try_into().unwrap()) as usize;
        let mut at = 52usize;
        for i in 0..n_tx {
            if bytes.len() < at + 4 {
                return Err(format!("wire block truncated at tx {i} length"));
            }
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            at += 4;
            if bytes.len() < at + len {
                return Err(format!("wire block truncated at tx {i} body"));
            }
            at += len;
        }
        if at != bytes.len() {
            return Err("trailing bytes after wire block".into());
        }
        Ok((parent, number, timestamp_ms, n_tx))
    }

    /// The commitment (and header) of a wire block WITHOUT materialising its
    /// transactions.
    ///
    /// The wire tail `bytes[48..]` is BYTE-IDENTICAL to the framed tx preimage
    /// `cat` that `new` builds (`[n u32 LE]([len u32 LE][tx])*`), so the
    /// commitment is two keccaks over borrowed slices: no per-tx `Vec`, no
    /// re-serialisation of the block. That matters on the anchor race —
    /// `stage_block` must publish the commitment it is working on BEFORE it
    /// pays for the decode, or an `arc_newBlock` arriving 15 ms later misses
    /// the in-progress mark and buys a 200 ms peer pull instead.
    ///
    /// Framing is validated exactly as `from_wire_bytes` validates it, so a
    /// malformed body can never be marked in progress.
    pub fn commitment_of_wire(bytes: &[u8]) -> Result<WireHeader, String> {
        let (parent, number, timestamp_ms, _) = Self::scan_wire(bytes)?;
        let txs_hash = keccak256(&bytes[48..]);
        let mut pre = Vec::with_capacity(80);
        pre.extend_from_slice(parent.as_slice());
        pre.extend_from_slice(&number.to_le_bytes());
        pre.extend_from_slice(&timestamp_ms.to_le_bytes());
        pre.extend_from_slice(txs_hash.as_slice());
        Ok(WireHeader {
            parent,
            number,
            timestamp_ms,
            commitment: keccak256(&pre),
        })
    }

    /// Strict wire decode: malformed and trailing bytes rejected; the
    /// commitment is recomputed from content, never trusted.
    pub fn from_wire_bytes(bytes: &[u8]) -> Result<Self, String> {
        let h = Self::commitment_of_wire(bytes)?;
        Self::from_wire_bytes_with_header(bytes, h)
    }

    /// Finish a decode whose header/commitment `commitment_of_wire` already
    /// produced over THESE bytes: copy the tx bodies out and keep that
    /// commitment rather than rebuilding and re-hashing the preimage.
    ///
    /// TRUST BOUNDARY — read before adding a caller. Only the three header
    /// fields are re-derived from the bytes and compared; `h.commitment` is
    /// TAKEN ON FAITH. It binds `keccak256(bytes[48..])`, and recomputing that
    /// is the whole cost this function exists to avoid (it is what
    /// `commitment_of_wire` already paid over these same bytes). So the
    /// caller's obligation is: pass the `WireHeader` that `commitment_of_wire`
    /// returned for the SAME slice, never one computed over a different body —
    /// otherwise the block is appendable under a commitment that does not
    /// match its content, `from_payload` rejects that record on the next
    /// replay, and the log is silently truncated there. `pub(crate)` for that
    /// reason; both callers (`from_wire_bytes` and `node::stage_inner`) hash
    /// and decode the one slice. Debug builds recompute and reject a mismatch,
    /// so a violating caller fails in tests rather than on the fleet.
    pub(crate) fn from_wire_bytes_with_header(bytes: &[u8], h: WireHeader) -> Result<Self, String> {
        let (parent, number, timestamp_ms, n_tx) = Self::scan_wire(bytes)?;
        if parent != h.parent || number != h.number || timestamp_ms != h.timestamp_ms {
            return Err("wire header does not match the bytes".into());
        }
        if cfg!(debug_assertions) && Self::commitment_of_wire(bytes)?.commitment != h.commitment {
            return Err("wire header commitment does not match the bytes".into());
        }
        let mut txs = Vec::with_capacity(n_tx.min(1 << 20));
        let mut at = 52usize;
        for _ in 0..n_tx {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            at += 4;
            txs.push(bytes[at..at + len].to_vec());
            at += len;
        }
        Ok(Self {
            parent,
            number,
            timestamp_ms,
            txs,
            commitment: h.commitment,
        })
    }

    fn payload(&self) -> Vec<u8> {
        let tx_bytes: usize = self.txs.iter().map(|t| 4 + t.len()).sum();
        let mut p = Vec::with_capacity(84 + tx_bytes);
        p.extend_from_slice(self.parent.as_slice());
        p.extend_from_slice(&self.number.to_le_bytes());
        p.extend_from_slice(&self.timestamp_ms.to_le_bytes());
        p.extend_from_slice(self.commitment.as_slice());
        p.extend_from_slice(&(self.txs.len() as u32).to_le_bytes());
        for t in &self.txs {
            p.extend_from_slice(&(t.len() as u32).to_le_bytes());
            p.extend_from_slice(t);
        }
        p
    }

    fn from_payload(p: &[u8]) -> Option<Self> {
        if p.len() < 84 {
            return None;
        }
        let parent = B256::from_slice(&p[0..32]);
        let number = u64::from_le_bytes(p[32..40].try_into().unwrap());
        let timestamp_ms = u64::from_le_bytes(p[40..48].try_into().unwrap());
        let commitment = B256::from_slice(&p[48..80]);
        let n_tx = u32::from_le_bytes(p[80..84].try_into().unwrap()) as usize;
        let mut txs = Vec::with_capacity(n_tx);
        let mut at = 84;
        for _ in 0..n_tx {
            if p.len() < at + 4 {
                return None;
            }
            let len = u32::from_le_bytes(p[at..at + 4].try_into().unwrap()) as usize;
            at += 4;
            if p.len() < at + len {
                return None;
            }
            txs.push(p[at..at + len].to_vec());
            at += len;
        }
        if at != p.len() {
            return None;
        }
        let rebuilt = Self::new(parent, number, timestamp_ms, txs);
        // Commitment integrity: a record whose stored commitment does not
        // match its content is treated as torn.
        (rebuilt.commitment == commitment).then_some(rebuilt)
    }
}

/// Append-only block log with fsync-per-append, torn-tail recovery, and an
/// in-memory `number -> (offset, payload_len)` index for sync serving.
pub struct BlockLog {
    file: std::fs::File,
    pub bytes_written: u64,
    index: std::collections::HashMap<u64, (u64, u32)>,
    /// fsync after every append (see `append`).
    fsync: bool,
}

impl BlockLog {
    pub fn open(path: &Path, fsync: bool) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        let bytes_written = file.metadata()?.len();
        Ok(Self {
            file,
            bytes_written,
            index: Default::default(),
            fsync,
        })
    }

    /// Append one record. With `fsync` off the append does NOT reach the disk
    /// before returning, so a crash can lose the last block(s). That is a
    /// LIVENESS cost only, never a safety one: the log is append-only and
    /// torn-tail-truncating, the commitment chain is recomputed from content
    /// on replay, the backfill re-pulls everything above the head, and the CL
    /// certificate binds the block bytes.
    ///
    /// The one asymmetry: state snapshots ARE fsync'd whatever this is set to,
    /// so a crash can leave a durable snapshot at height N with the log intact
    /// only to L < N. `LaneNode::open` therefore restarts from the newest
    /// snapshot at or below L and replays from there, so state and log always
    /// end at the same block (rolling back only the head would not do: the
    /// state would still be N's, and re-applying L+1..=N against it is not a
    /// no-op).
    pub fn append(&mut self, block: &LeanBlock) -> std::io::Result<()> {
        let offset = self.bytes_written;
        let payload = block.payload();
        self.index
            .insert(block.number, (offset, payload.len() as u32));
        let len = (payload.len() as u32).to_le_bytes();
        let mut rec = Vec::with_capacity(8 + payload.len());
        rec.extend_from_slice(&len);
        rec.extend_from_slice(&payload);
        rec.extend_from_slice(&len);
        self.file.write_all(&rec)?;
        if self.fsync {
            self.file.sync_data()?;
        }
        self.bytes_written += rec.len() as u64;
        Ok(())
    }

    /// Replay all intact records in order, invoking `f` for every one;
    /// truncates the file past the last intact record. Returns the number of
    /// intact records.
    pub fn replay(&mut self, mut f: impl FnMut(&LeanBlock)) -> std::io::Result<u64> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes)?;
        let mut at = 0usize;
        let mut good_end = 0usize;
        let mut replayed = 0u64;
        while bytes.len() >= at + 4 {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let end = at + 4 + len + 4;
            if bytes.len() < end {
                break; // torn tail
            }
            let echo = u32::from_le_bytes(bytes[end - 4..end].try_into().unwrap()) as usize;
            if echo != len {
                break;
            }
            let Some(block) = LeanBlock::from_payload(&bytes[at + 4..at + 4 + len]) else {
                break;
            };
            self.index.insert(block.number, (at as u64, len as u32));
            f(&block);
            replayed += 1;
            good_end = end;
            at = end;
        }
        if good_end < bytes.len() {
            self.file.set_len(good_end as u64)?;
            self.file.sync_data()?;
        }
        self.bytes_written = good_end as u64;
        self.file.seek(SeekFrom::End(0))?;
        Ok(replayed)
    }

    /// The highest block number the log holds, or `None` if it is empty.
    /// Only meaningful after `replay` (which is what populates the index and
    /// truncates a torn tail); `open` uses it to decide which snapshot the log
    /// can be continued from.
    pub fn last_number(&self) -> Option<u64> {
        self.index.keys().copied().max()
    }

    /// Read one block back from the log by number (None if unknown).
    pub fn read_block(&mut self, number: u64) -> std::io::Result<Option<LeanBlock>> {
        let Some(&(offset, len)) = self.index.get(&number) else {
            return Ok(None);
        };
        self.file.seek(SeekFrom::Start(offset + 4))?;
        let mut payload = vec![0u8; len as usize];
        self.file.read_exact(&mut payload)?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(LeanBlock::from_payload(&payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// I1: `from_wire_bytes_with_header` trusts `h.commitment` in release. The
    /// guard that keeps that trust honest is the debug-build recompute — a
    /// header paired with a DIFFERENT body must be refused, or the decode
    /// mints a block whose commitment does not bind its content (appendable,
    /// then torn on the next replay).
    ///
    /// Debug-only, because the guard is: the recompute is a full keccak over
    /// the body (2.5 MB on the fleet, ~1 per stage on the consensus critical
    /// path), which is precisely the cost `from_wire_bytes_with_header` exists
    /// to avoid — so it stays `cfg!(debug_assertions)` and this test compiles
    /// out of `cargo test --release` rather than failing there.
    #[cfg(debug_assertions)]
    #[test]
    fn a_header_from_a_different_body_is_rejected() {
        let parent = B256::repeat_byte(0x11);
        let a = LeanBlock::new(parent, 7, 1_000, vec![vec![0xaa; 16]]);
        let b = LeanBlock::new(parent, 7, 1_000, vec![vec![0xbb; 16]]);
        // same header fields, different txs
        assert_ne!(a.commitment, b.commitment);
        let ha = LeanBlock::commitment_of_wire(&a.to_wire_bytes()).unwrap();
        let hb = LeanBlock::commitment_of_wire(&b.to_wire_bytes()).unwrap();
        // the three re-derived fields agree, so only the commitment check can
        // catch this pairing
        assert_eq!(ha.parent, hb.parent);
        assert_eq!((ha.number, ha.timestamp_ms), (hb.number, hb.timestamp_ms));
        let err = LeanBlock::from_wire_bytes_with_header(&b.to_wire_bytes(), ha).unwrap_err();
        assert!(err.contains("commitment"), "{err}");
    }

    /// The matched pairing every in-crate caller passes still decodes, and to
    /// exactly what a full `from_wire_bytes` produces.
    #[test]
    fn a_matched_header_decodes_to_the_full_block() {
        let b = LeanBlock::new(B256::repeat_byte(0x22), 3, 9, vec![vec![1, 2, 3], vec![4]]);
        let wire = b.to_wire_bytes();
        let h = LeanBlock::commitment_of_wire(&wire).unwrap();
        let got = LeanBlock::from_wire_bytes_with_header(&wire, h).unwrap();
        assert_eq!(got.commitment, b.commitment);
        assert_eq!(got.txs, b.txs);
        let full = LeanBlock::from_wire_bytes(&wire).unwrap();
        assert_eq!(full.commitment, b.commitment);
    }
}
