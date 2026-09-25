//! The canonical block framing and commitment — the consensus definitions,
//! kept OUT of the optimizable engine so no engine change can alter them.
//!
//! ```text
//! wire       = [parent 32][number u64 LE][timestamp_ms u64 LE]
//!              [n_txs u32 LE] ( [len u32 LE] tx_bytes )*
//! txs_hash   = keccak256( [n_txs u32 LE] ‖ ( [len_i u32 LE] ‖ tx_i_bytes )* )
//!              -- exactly the wire form's tail, wire[48..] --
//! commitment = keccak256( parent ‖ number u64 LE ‖ timestamp_ms u64 LE ‖ txs_hash )
//! GENESIS    = keccak256( "ARC_LEAN_LANE_GENESIS" ‖ chain_id u64 BE )
//! ```

use crate::types::BlockHeader;
use alloy_primitives::{keccak256, B256};

/// Bytes before the first tx length: parent, number, timestamp, n_txs.
pub const WIRE_HEADER_LEN: usize = 52;
/// Offset of the tx section (`n_txs` onward) that `txs_hash` covers.
pub const TXS_SECTION_AT: usize = 48;

pub fn genesis_commitment(chain_id: u64) -> B256 {
    let mut buf = Vec::with_capacity(29);
    buf.extend_from_slice(b"ARC_LEAN_LANE_GENESIS");
    buf.extend_from_slice(&chain_id.to_be_bytes());
    keccak256(&buf)
}

/// The commitment over a header and its `txs_hash`.
pub fn commitment(h: &BlockHeader, txs_hash: B256) -> B256 {
    let mut pre = [0u8; 80];
    pre[0..32].copy_from_slice(h.parent.as_slice());
    pre[32..40].copy_from_slice(&h.number.to_le_bytes());
    pre[40..48].copy_from_slice(&h.timestamp_ms.to_le_bytes());
    pre[48..80].copy_from_slice(txs_hash.as_slice());
    keccak256(pre)
}

/// The commitment of a wire block whose framing is already validated: two
/// keccaks over borrowed slices, no re-serialisation.
pub fn commitment_of_wire(h: &BlockHeader, wire: &[u8]) -> B256 {
    commitment(h, keccak256(&wire[TXS_SECTION_AT..]))
}

/// The framed tx section (`[n u32 LE]([len u32 LE][tx])*`) of a tx list.
pub fn txs_section<T: AsRef<[u8]>>(txs: &[T]) -> Vec<u8> {
    let size = 4 + txs.iter().map(|t| 4 + t.as_ref().len()).sum::<usize>();
    let mut cat = Vec::with_capacity(size);
    cat.extend_from_slice(&(txs.len() as u32).to_le_bytes());
    for t in txs {
        let t = t.as_ref();
        cat.extend_from_slice(&(t.len() as u32).to_le_bytes());
        cat.extend_from_slice(t);
    }
    cat
}

/// The commitment of a block given as a header and a tx list.
pub fn commitment_of_list<T: AsRef<[u8]>>(h: &BlockHeader, txs: &[T]) -> B256 {
    commitment(h, keccak256(txs_section(txs)))
}

/// The canonical wire bytes of a block.
pub fn to_wire<T: AsRef<[u8]>>(h: &BlockHeader, txs: &[T]) -> Vec<u8> {
    let tx_bytes: usize = txs.iter().map(|t| 4 + t.as_ref().len()).sum();
    let mut w = Vec::with_capacity(WIRE_HEADER_LEN + tx_bytes);
    w.extend_from_slice(h.parent.as_slice());
    w.extend_from_slice(&h.number.to_le_bytes());
    w.extend_from_slice(&h.timestamp_ms.to_le_bytes());
    w.extend_from_slice(&(txs.len() as u32).to_le_bytes());
    for t in txs {
        let t = t.as_ref();
        w.extend_from_slice(&(t.len() as u32).to_le_bytes());
        w.extend_from_slice(t);
    }
    w
}
