//! Counters the node keeps for operators, and the route an anchor took.
//!
//! Everything here is a relaxed atomic: read by the periodic `lean_stats`
//! line (`--stats`), written on the hot path without locks. Per-call timing
//! is not kept here; it goes out as `tracing` debug events
//! (`RUST_LOG=lean_lane_node=debug`).

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

/// Which route an `arc_newBlock` took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AnchorPath {
    /// Not classified (never recorded: every call ends in one of the others).
    Other = 0,
    /// Already canonical: answered from the commitment index / log.
    Index = 1,
    /// Promoted from a staged entry (the healthy validator's path).
    Staged = 2,
    /// Applied from the sync queue.
    Queued = 3,
    /// Bytes fetched from a peer by commitment.
    Peer = 4,
    /// Answered SYNCING.
    Syncing = 5,
    /// `arc_newBlock{blockBytes}` executed in full.
    Bytes = 6,
}

impl AnchorPath {
    const ALL: [AnchorPath; 7] = [
        AnchorPath::Other,
        AnchorPath::Index,
        AnchorPath::Staged,
        AnchorPath::Queued,
        AnchorPath::Peer,
        AnchorPath::Syncing,
        AnchorPath::Bytes,
    ];

    pub fn name(self) -> &'static str {
        match self {
            AnchorPath::Other => "other",
            AnchorPath::Index => "index",
            AnchorPath::Staged => "staged",
            AnchorPath::Queued => "queued",
            AnchorPath::Peer => "peer",
            AnchorPath::Syncing => "syncing",
            AnchorPath::Bytes => "bytes",
        }
    }
}

/// The route of ONE `arc_newBlock` call, shared by `&` across awaits (an
/// atomic because a `Cell` would make the futures `!Send`).
#[derive(Default)]
pub struct PathCell(AtomicU8);

impl PathCell {
    pub fn new(p: AnchorPath) -> Self {
        Self(AtomicU8::new(p as u8))
    }
    pub fn set(&self, p: AnchorPath) {
        self.0.store(p as u8, Ordering::Relaxed);
    }
    /// Only refine the default/bytes label — never overwrite an explicit
    /// source (peer/queued/index) with a later, less specific one.
    pub fn refine(&self, p: AnchorPath) {
        let cur = self.get();
        if cur == AnchorPath::Other || cur == AnchorPath::Bytes {
            self.set(p);
        }
    }
    pub fn get(&self) -> AnchorPath {
        AnchorPath::ALL[self.0.load(Ordering::Relaxed) as usize % AnchorPath::ALL.len()]
    }
}

#[derive(Default)]
pub struct Metrics {
    pub admitted: AtomicU64,
    pub rejected: AtomicU64,
    pub blocks: AtomicU64,
    pub txs: AtomicU64,
    pub noops: AtomicU64,
    pub outputs: AtomicU64,
    /// Blocks appended by promoting a staged entry (no re-execution).
    pub promoted: AtomicU64,
    pub announces_rx: AtomicU64,
    pub announces_known: AtomicU64,
    /// Announcements ignored because the block is already held (staged, or
    /// a stage/commit in flight): our own CL's anchor is milliseconds away.
    pub announce_skipped_staged: AtomicU64,
    /// Block-byte pulls from peers: the backfill's and the anchor's.
    pub block_pulls: AtomicU64,
    /// Block-byte pulls made by the backfill loop alone.
    pub backfill_pulls: AtomicU64,
    /// `arc_newBlock{commitment}` calls that found nothing locally, waited
    /// (`await_arrival`) and saw the block arrive: resolved without a peer.
    pub anchor_grace_hits: AtomicU64,
    /// Calls that waited and found nothing: they went on to the peer pull.
    pub anchor_grace_misses: AtomicU64,
    /// `arc_newBlock` calls per route, indexed by `AnchorPath as usize`.
    anchor_paths: [AtomicU64; 7],
}

impl Metrics {
    pub fn inc(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Ordering::Relaxed);
    }

    pub fn record_anchor(&self, p: AnchorPath) {
        Self::inc(&self.anchor_paths[p as usize]);
    }

    pub fn anchors(&self, p: AnchorPath) -> u64 {
        self.anchor_paths[p as usize].load(Ordering::Relaxed)
    }

    /// The counters half of the `lean_stats` line.
    pub fn line(&self) -> String {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let paths = [
            AnchorPath::Index,
            AnchorPath::Staged,
            AnchorPath::Queued,
            AnchorPath::Peer,
            AnchorPath::Syncing,
            AnchorPath::Bytes,
        ]
        .iter()
        .map(|p| format!("anchor_{}={}", p.name(), self.anchors(*p)))
        .collect::<Vec<_>>()
        .join(" ");
        format!(
            "blocks={} txs={} outputs={} noops={} promoted={} admitted={} rejected={} {paths} \
             anchor_grace_hits={} anchor_grace_misses={} block_pulls={} \
             backfill_pulls={} announces_rx={} announces_known={} announce_skipped_staged={}",
            g(&self.blocks),
            g(&self.txs),
            g(&self.outputs),
            g(&self.noops),
            g(&self.promoted),
            g(&self.admitted),
            g(&self.rejected),
            g(&self.anchor_grace_hits),
            g(&self.anchor_grace_misses),
            g(&self.block_pulls),
            g(&self.backfill_pulls),
            g(&self.announces_rx),
            g(&self.announces_known),
            g(&self.announce_skipped_staged),
        )
    }
}

/// Resident set size in MiB, from `/proc/self/statm` field 2 (resident pages).
/// 0 wherever the file is unreadable — a stats line must never fail.
pub fn rss_mb() -> u64 {
    const PAGE: u64 = 4096;
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or(0, |pages| pages * PAGE / (1024 * 1024))
}
