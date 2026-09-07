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
        Self { parent, number, timestamp_ms, txs, commitment }
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

    /// Strict wire decode: malformed and trailing bytes rejected; the
    /// commitment is recomputed from content, never trusted.
    pub fn from_wire_bytes(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 52 {
            return Err("wire block too short".into());
        }
        let parent = B256::from_slice(&bytes[0..32]);
        let number = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
        let timestamp_ms = u64::from_le_bytes(bytes[40..48].try_into().unwrap());
        let n_tx = u32::from_le_bytes(bytes[48..52].try_into().unwrap()) as usize;
        let mut txs = Vec::with_capacity(n_tx.min(1 << 20));
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
            txs.push(bytes[at..at + len].to_vec());
            at += len;
        }
        if at != bytes.len() {
            return Err("trailing bytes after wire block".into());
        }
        Ok(Self::new(parent, number, timestamp_ms, txs))
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
}

impl BlockLog {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new().create(true).read(true).append(true).open(path)?;
        let bytes_written = file.metadata()?.len();
        Ok(Self { file, bytes_written, index: Default::default() })
    }

    pub fn append(&mut self, block: &LeanBlock) -> std::io::Result<()> {
        let offset = self.bytes_written;
        let payload = block.payload();
        self.index.insert(block.number, (offset, payload.len() as u32));
        let len = (payload.len() as u32).to_le_bytes();
        let mut rec = Vec::with_capacity(8 + payload.len());
        rec.extend_from_slice(&len);
        rec.extend_from_slice(&payload);
        rec.extend_from_slice(&len);
        self.file.write_all(&rec)?;
        self.file.sync_data()?;
        self.bytes_written += rec.len() as u64;
        Ok(())
    }

    /// Replay all intact records, invoking `f` for blocks with
    /// `number > after_number`; truncates the file past the last intact
    /// record. Returns the number of blocks replayed.
    pub fn replay(
        &mut self,
        after_number: u64,
        mut f: impl FnMut(&LeanBlock),
    ) -> std::io::Result<u64> {
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
            if block.number > after_number {
                f(&block);
                replayed += 1;
            }
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

    /// Read one block back from the log by number (None if unknown).
    pub fn read_block(&mut self, number: u64) -> std::io::Result<Option<LeanBlock>> {
        let Some(&(offset, len)) = self.index.get(&number) else { return Ok(None) };
        self.file.seek(SeekFrom::Start(offset + 4))?;
        let mut payload = vec![0u8; len as usize];
        self.file.read_exact(&mut payload)?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(LeanBlock::from_payload(&payload))
    }
}
