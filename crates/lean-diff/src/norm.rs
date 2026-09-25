//! One normalized form for "what processing a wire block against a state
//! gives", produced independently by the engine and by the oracle, so the
//! two can be compared field by field.

use crate::util::{catch, ctx};
use crate::CHAIN_ID;
use alloy_primitives::Address;
use lean_core::{BlockError, FlatState};
use lean_oracle::{Account, State};

/// Why a block is not well-framed, in a shape both sides map onto.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameErr {
    TooShort,
    TruncatedLength(u64),
    TruncatedBody(u64),
    TrailingBytes,
}

/// A write as plain bytes.
pub type Write = ([u8; 20], u64, u128);

/// The result of processing one block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Norm {
    Rejected(FrameErr),
    Accepted(Box<Accepted>),
    /// The implementation panicked (always a failure).
    Panicked(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    pub parent: [u8; 32],
    pub number: u64,
    pub timestamp_ms: u64,
    pub commitment: [u8; 32],
    pub applied: u64,
    pub noops: u64,
    pub outputs: u64,
    pub fees: u128,
    /// The full write set, sorted by address.
    pub writes: Vec<Write>,
    /// The §2.4 item hashes in block order (the engine's pool-evict list).
    pub items: Vec<[u8; 32]>,
}

impl Norm {
    pub fn accepted(&self) -> Option<&Accepted> {
        match self {
            Norm::Accepted(a) => Some(a),
            _ => None,
        }
    }
}

fn frame_err_engine(e: BlockError) -> FrameErr {
    match e {
        BlockError::TooShort => FrameErr::TooShort,
        BlockError::TruncatedLength(i) => FrameErr::TruncatedLength(i as u64),
        BlockError::TruncatedBody(i) => FrameErr::TruncatedBody(i as u64),
        BlockError::TrailingBytes => FrameErr::TrailingBytes,
    }
}

fn frame_err_oracle(e: lean_oracle::BlockError) -> FrameErr {
    match e {
        lean_oracle::BlockError::TooShort { .. } => FrameErr::TooShort,
        lean_oracle::BlockError::LenPrefixTruncated { index } => {
            FrameErr::TruncatedLength(index as u64)
        }
        lean_oracle::BlockError::EntryTruncated { index, .. } => {
            FrameErr::TruncatedBody(index as u64)
        }
        lean_oracle::BlockError::TrailingBytes { .. } => FrameErr::TrailingBytes,
    }
}

/// The engine's result (pure: `state` is only read). Returns the delta too so
/// the caller can apply exactly what the engine computed.
pub fn engine_block(
    state: &FlatState,
    wire: &[u8],
    parallel: bool,
) -> (Norm, Option<lean_core::Delta>) {
    let c = ctx(parallel);
    match catch(|| lean_core::process_block(state, wire, &c)) {
        Err(p) => (Norm::Panicked(p), None),
        Ok(Err(e)) => (Norm::Rejected(frame_err_engine(e)), None),
        Ok(Ok(p)) => {
            let d = p.delta;
            let n = Norm::Accepted(Box::new(Accepted {
                parent: p.header.parent.0,
                number: p.header.number,
                timestamp_ms: p.header.timestamp_ms,
                commitment: p.commitment.0,
                applied: d.outcome.applied as u64,
                noops: d.outcome.noops as u64,
                outputs: d.outcome.outputs as u64,
                fees: d.outcome.fees_to_beneficiary,
                writes: d
                    .writes
                    .iter()
                    .map(|(a, acct)| (a.0 .0, acct.nonce, acct.balance))
                    .collect(),
                items: d.evict.iter().map(|h| h.0).collect(),
            }));
            (n, Some(d))
        }
    }
}

/// The oracle's write set.
pub type OracleWrites = Vec<([u8; 20], Account)>;

/// The oracle's result (pure). Returns its writes too.
pub fn oracle_block(state: &State, wire: &[u8]) -> (Norm, Option<OracleWrites>) {
    let r = catch(|| {
        let block = match lean_oracle::decode_block(wire) {
            Err(e) => return Err(frame_err_oracle(e)),
            Ok(b) => b,
        };
        let (items, _skipped) = lean_oracle::stf::classify_entries(&block.entries, CHAIN_ID);
        let (writes, out) = lean_oracle::execute(state, &items);
        Ok((block, items, writes, out))
    });
    match r {
        Err(p) => (Norm::Panicked(p), None),
        Ok(Err(e)) => (Norm::Rejected(e), None),
        Ok(Ok((block, items, writes, out))) => {
            let n = Norm::Accepted(Box::new(Accepted {
                parent: block.header.parent,
                number: block.header.number,
                timestamp_ms: block.header.timestamp_ms,
                commitment: block.commitment(),
                applied: out.applied,
                noops: out.noops,
                outputs: out.outputs,
                fees: out.fees_to_beneficiary,
                writes: writes
                    .iter()
                    .map(|(a, acct)| (*a, acct.nonce, acct.balance))
                    .collect(),
                items: items.iter().map(|i| i.tx_hash).collect(),
            }));
            (n, Some(writes))
        }
    }
}

/// First differing field between two results, human-readable; `None` if equal.
pub fn describe_diff(a_name: &str, a: &Norm, b_name: &str, b: &Norm) -> Option<String> {
    if a == b {
        return None;
    }
    let (x, y) = match (a, b) {
        (Norm::Accepted(x), Norm::Accepted(y)) => (x, y),
        _ => {
            return Some(format!(
                "framing/panic verdict: {a_name}={} vs {b_name}={}",
                short(a),
                short(b)
            ))
        }
    };
    let mut out = Vec::new();
    if (x.parent, x.number, x.timestamp_ms) != (y.parent, y.number, y.timestamp_ms) {
        out.push(format!(
            "header: {a_name}=({}, {}, {}) {b_name}=({}, {}, {})",
            hex::encode(x.parent),
            x.number,
            x.timestamp_ms,
            hex::encode(y.parent),
            y.number,
            y.timestamp_ms
        ));
    }
    if x.commitment != y.commitment {
        out.push(format!(
            "commitment: {a_name}=0x{} {b_name}=0x{}",
            hex::encode(x.commitment),
            hex::encode(y.commitment)
        ));
    }
    let cx = (x.applied, x.noops, x.outputs, x.fees);
    let cy = (y.applied, y.noops, y.outputs, y.fees);
    if cx != cy {
        out.push(format!(
            "counters (applied, noops, outputs, fees): {a_name}={cx:?} {b_name}={cy:?}"
        ));
    }
    if x.items != y.items {
        let first = x
            .items
            .iter()
            .zip(&y.items)
            .position(|(p, q)| p != q)
            .unwrap_or(x.items.len().min(y.items.len()));
        out.push(format!(
            "items: {a_name} has {} {b_name} has {}; first difference at item {first}: \
             {a_name}={} {b_name}={}",
            x.items.len(),
            y.items.len(),
            x.items
                .get(first)
                .map(|h| format!("0x{}", hex::encode(h)))
                .unwrap_or("-".into()),
            y.items
                .get(first)
                .map(|h| format!("0x{}", hex::encode(h)))
                .unwrap_or("-".into()),
        ));
    }
    if x.writes != y.writes {
        out.push(format!(
            "write set: {a_name} writes {} accounts, {b_name} writes {}; {}",
            x.writes.len(),
            y.writes.len(),
            write_set_diff(a_name, &x.writes, b_name, &y.writes)
        ));
    }
    Some(out.join("\n    "))
}

fn short(n: &Norm) -> String {
    match n {
        Norm::Rejected(e) => format!("rejected({e:?})"),
        Norm::Accepted(a) => format!("accepted(commitment 0x{})", hex::encode(a.commitment)),
        Norm::Panicked(p) => format!("PANIC({p})"),
    }
}

fn write_set_diff(a_name: &str, a: &[Write], b_name: &str, b: &[Write]) -> String {
    use std::collections::BTreeMap;
    let ma: BTreeMap<_, _> = a.iter().map(|(k, n, bal)| (*k, (*n, *bal))).collect();
    let mb: BTreeMap<_, _> = b.iter().map(|(k, n, bal)| (*k, (*n, *bal))).collect();
    let mut lines = Vec::new();
    for k in ma
        .keys()
        .chain(mb.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        let (va, vb) = (ma.get(k), mb.get(k));
        if va != vb {
            lines.push(format!(
                "0x{}: {a_name}={} {b_name}={}",
                hex::encode(k),
                va.map(|(n, b)| format!("(nonce {n}, balance {b})"))
                    .unwrap_or("absent".into()),
                vb.map(|(n, b)| format!("(nonce {n}, balance {b})"))
                    .unwrap_or("absent".into()),
            ));
        }
        if lines.len() >= 8 {
            lines.push("…".into());
            break;
        }
    }
    lines.join("; ")
}

/// Engine and oracle pre-states for a reproducer: the accounts named by a
/// write set, as `address nonce balance` lines.
pub fn account_lines(state: &State, addrs: &[[u8; 20]]) -> Vec<String> {
    addrs
        .iter()
        .map(|a| {
            let present = state.contains(a);
            let acct = state.get(a);
            format!(
                "0x{} {}",
                hex::encode(a),
                if present {
                    format!("nonce {} balance {}", acct.nonce, acct.balance)
                } else {
                    "absent".to_string()
                }
            )
        })
        .collect()
}

/// Convert an engine address to oracle bytes.
pub fn addr_bytes(a: &Address) -> [u8; 20] {
    a.0 .0
}
