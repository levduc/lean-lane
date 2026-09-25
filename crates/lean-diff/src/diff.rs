//! The differential property test: engine (parallel AND serial recovery) vs
//! oracle over seeded adversarial chains, with delta-debugging shrinking.

use crate::gen::{ChainGen, GenBlock, Rates, KINDS};
use crate::norm::{account_lines, describe_diff, engine_block, oracle_block, Norm};
use crate::rng::mix;
use crate::util::{engine_genesis, hex_short, threads};
use crate::CHAIN_ID;
use lean_core::FlatState;
use lean_oracle::{encode_block, genesis_state, state_digest, Header, State};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct DiffCfg {
    pub seed: u64,
    pub chains: u64,
    pub blocks: u64,
    pub rates: Rates,
    /// Run only this chain index (reproduction).
    pub only_chain: Option<u64>,
    pub threads: usize,
}

impl DiffCfg {
    pub fn new(seed: u64, chains: u64, blocks: u64) -> Self {
        DiffCfg {
            seed,
            chains,
            blocks,
            rates: Rates::default(),
            only_chain: None,
            threads: threads(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DiffStats {
    pub chains: u64,
    pub blocks: u64,
    pub blocks_rejected: u64,
    pub blocks_empty: u64,
    pub entries: u64,
    pub items: u64,
    pub applied: u64,
    pub noops: u64,
    pub outputs: u64,
    pub entries_by_kind: std::collections::BTreeMap<String, u64>,
    pub seconds: f64,
}

impl DiffStats {
    fn add(&mut self, o: &DiffStats) {
        self.chains += o.chains;
        self.blocks += o.blocks;
        self.blocks_rejected += o.blocks_rejected;
        self.blocks_empty += o.blocks_empty;
        self.entries += o.entries;
        self.items += o.items;
        self.applied += o.applied;
        self.noops += o.noops;
        self.outputs += o.outputs;
        for (k, v) in &o.entries_by_kind {
            *self.entries_by_kind.entry(k.clone()).or_default() += v;
        }
    }
}

/// A divergence, already shrunk, with a printable reproducer.
#[derive(Clone, Debug)]
pub struct Mismatch {
    pub seed: u64,
    pub chain: u64,
    pub block: u64,
    pub summary: String,
    pub report: String,
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.report)
    }
}

/// Run the suite. Chains are independent (`chain seed = mix(seed, index)`)
/// and spread over worker threads; the first mismatch stops everything.
pub fn run(cfg: &DiffCfg) -> Result<DiffStats, Box<Mismatch>> {
    let t0 = Instant::now();
    let indices: Vec<u64> = match cfg.only_chain {
        Some(i) => vec![i],
        None => (0..cfg.chains).collect(),
    };
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let total = Mutex::new(DiffStats::default());
    let failure: Mutex<Option<Box<Mismatch>>> = Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..cfg.threads.min(indices.len()).max(1) {
            s.spawn(|| loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(&chain) = indices.get(i) else {
                    return;
                };
                match run_chain(cfg, chain) {
                    Ok(st) => total.lock().unwrap().add(&st),
                    Err(m) => {
                        stop.store(true, Ordering::Relaxed);
                        let mut f = failure.lock().unwrap();
                        // Deterministic report: keep the lowest chain index.
                        if f.as_ref().is_none_or(|x| m.chain < x.chain) {
                            *f = Some(m);
                        }
                        return;
                    }
                }
            });
        }
    });
    if let Some(m) = failure.into_inner().unwrap() {
        return Err(m);
    }
    let mut st = total.into_inner().unwrap();
    st.seconds = t0.elapsed().as_secs_f64();
    Ok(st)
}

/// The genesis both sides start from, checked equal.
pub fn genesis_pair(gen: &ChainGen) -> Result<(State, FlatState), String> {
    let o = genesis_state(&gen.fund_file, gen.fund_balance)
        .map_err(|e| format!("oracle rejected the fund file: {e:?}"))?;
    let e = engine_genesis(&gen.fund_file, gen.fund_balance)?;
    if state_digest(&o) != e.digest().0 {
        return Err(format!(
            "genesis digest: engine-side loader 0x{} vs oracle 0x{} (fund file {:?}, balance {})",
            hex::encode(e.digest()),
            hex::encode(state_digest(&o)),
            gen.fund_file,
            gen.fund_balance
        ));
    }
    Ok((o, e))
}

/// Differential run of one chain.
pub fn run_chain(cfg: &DiffCfg, chain: u64) -> Result<DiffStats, Box<Mismatch>> {
    let mut gen = ChainGen::new(mix(cfg.seed, chain), cfg.rates.clone());
    let fail = |block: u64, summary: String, report: String| {
        Box::new(Mismatch {
            seed: cfg.seed,
            chain,
            block,
            summary: summary.clone(),
            report: format!(
                "MISMATCH seed={} chain={} block={}: {}\n{}\n  reproduce: lean-diff diff --seed {} \
                 --only-chain {} --blocks {}{}",
                cfg.seed,
                chain,
                block,
                summary,
                report,
                cfg.seed,
                chain,
                block + 1,
                rates_hint()
            ),
        })
    };
    let (mut ostate, mut estate) =
        genesis_pair(&gen).map_err(|e| fail(0, "genesis".into(), format!("  {e}")))?;
    if lean_core::genesis_commitment(CHAIN_ID).0 != lean_oracle::genesis_commitment(CHAIN_ID) {
        return Err(fail(0, "genesis commitment".into(), String::new()));
    }
    let mut head = (lean_oracle::genesis_commitment(CHAIN_ID), 0u64);
    let mut st = DiffStats {
        chains: 1,
        ..Default::default()
    };
    for bi in 0..cfg.blocks {
        let blk = gen.next_block(&ostate, head.0, head.1 + 1);
        st.blocks += 1;
        st.entries += blk.entries.len() as u64;
        if blk.entries.is_empty() {
            st.blocks_empty += 1;
        }
        for k in &blk.kinds {
            *st.entries_by_kind.entry(k.to_string()).or_default() += 1;
        }
        let (on, owrites) = oracle_block(&ostate, &blk.wire);
        let (ep, edelta) = engine_block(&estate, &blk.wire, true);
        let (es, _) = engine_block(&estate, &blk.wire, false);
        let problem = describe_diff("engine(parallel)", &ep, "oracle", &on)
            .or_else(|| describe_diff("engine(serial)", &es, "oracle", &on));
        if let Some(what) = problem {
            let report = reproducer(&blk, &ostate, &estate, &what);
            return Err(fail(bi, first_line(&what), report));
        }
        match (&on, owrites, edelta) {
            (Norm::Accepted(a), Some(ow), Some(ed)) => {
                lean_oracle::stf::apply_writes(&mut ostate, &ow);
                lean_core::apply(&mut estate, &ed);
                let (od, edg) = (state_digest(&ostate), estate.digest().0);
                if od != edg {
                    return Err(fail(
                        bi,
                        "post-state digest".into(),
                        format!(
                            "  engine 0x{} oracle 0x{} (block results were identical: the \
                             divergence is in apply or digest)",
                            hex::encode(edg),
                            hex::encode(od)
                        ),
                    ));
                }
                head = (a.commitment, a.number);
                st.items += a.items.len() as u64;
                st.applied += a.applied;
                st.noops += a.noops;
                st.outputs += a.outputs;
            }
            (Norm::Rejected(_), _, _) => st.blocks_rejected += 1,
            _ => unreachable!("results compared equal and not panicked"),
        }
    }
    Ok(st)
}

fn rates_hint() -> String {
    match std::env::var("LEAN_DIFF_RATES") {
        Ok(r) => format!(" --rates '{r}'"),
        Err(_) => String::new(),
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

/// Does this entry list (on `header`, against the given pre-states) still
/// make the implementations disagree?
fn diverges(header: &Header, entries: &[Vec<u8>], o: &State, e: &FlatState) -> Option<String> {
    let wire = encode_block(header, entries);
    let (on, _) = oracle_block(o, &wire);
    let (ep, _) = engine_block(e, &wire, true);
    let (es, _) = engine_block(e, &wire, false);
    describe_diff("engine(parallel)", &ep, "oracle", &on)
        .or_else(|| describe_diff("engine(serial)", &es, "oracle", &on))
}

/// Delta debugging (ddmin) over a list, then a 1-minimal pass: the smallest
/// sub-list (by removal) for which `fails` still holds. `fails(xs)` must be
/// true for the input.
pub fn ddmin<T: Clone>(xs: &[T], fails: impl Fn(&[T]) -> bool) -> Vec<T> {
    let mut cur = xs.to_vec();
    let mut n = 2usize;
    while cur.len() >= 2 {
        let chunk = cur.len().div_ceil(n);
        let mut reduced = false;
        for start in (0..cur.len()).step_by(chunk) {
            let complement: Vec<T> = cur[..start]
                .iter()
                .chain(&cur[(start + chunk).min(cur.len())..])
                .cloned()
                .collect();
            if fails(&complement) {
                cur = complement;
                n = (n - 1).max(2);
                reduced = true;
                break;
            }
        }
        if !reduced {
            if n >= cur.len() {
                break;
            }
            n = (n * 2).min(cur.len());
        }
    }
    let mut i = 0;
    while i < cur.len() && cur.len() > 1 {
        let mut c = cur.clone();
        c.remove(i);
        if fails(&c) {
            cur = c;
        } else {
            i += 1;
        }
    }
    cur
}

/// Shrink a diverging block's entry list (same header, same pre-states).
pub fn shrink(
    header: &Header,
    entries: &[(Vec<u8>, &'static str)],
    o: &State,
    e: &FlatState,
) -> Vec<(Vec<u8>, &'static str)> {
    ddmin(entries, |xs| {
        let raw: Vec<Vec<u8>> = xs.iter().map(|x| x.0.clone()).collect();
        diverges(header, &raw, o, e).is_some()
    })
}

fn reproducer(blk: &GenBlock, o: &State, e: &FlatState, what: &str) -> String {
    // The first line of `what` is already the report's headline.
    let mut r = String::new();
    for l in what.lines().skip(1) {
        r += &format!("  {}\n", l.trim_start());
    }
    r += &format!(
        "  block header: parent 0x{} number {} timestamp_ms {}; {} entries; pre-state digest 0x{} \
         ({} accounts)\n",
        hex::encode(blk.header.parent),
        blk.header.number,
        blk.header.timestamp_ms,
        blk.entries.len(),
        hex::encode(state_digest(o)),
        o.len()
    );
    if let Some(c) = blk.corruption {
        r += &format!(
            "  framing corruption '{c}'; wire = {}\n",
            hex_short(&blk.wire, 4096)
        );
        return r;
    }
    let tagged: Vec<(Vec<u8>, &'static str)> = blk
        .entries
        .iter()
        .cloned()
        .zip(blk.kinds.iter().copied())
        .collect();
    let min = shrink(&blk.header, &tagged, o, e);
    let raw: Vec<Vec<u8>> = min.iter().map(|x| x.0.clone()).collect();
    r += &format!(
        "  SHRUNK to {} of {} entries (same header and pre-state):\n",
        min.len(),
        tagged.len()
    );
    if let Some(d) = diverges(&blk.header, &raw, o, e) {
        r += &format!("    divergence: {}\n", d.replace('\n', "\n    "));
    }
    for (i, (bytes, kind)) in min.iter().enumerate() {
        r += &format!("    entry {i} [{kind}] {}\n", hex_short(bytes, 2048));
    }
    // Pre-state of every account either side writes for the shrunk block.
    let wire = encode_block(&blk.header, &raw);
    let mut addrs: Vec<[u8; 20]> = Vec::new();
    for n in [oracle_block(o, &wire).0, engine_block(e, &wire, true).0] {
        if let Some(a) = n.accepted() {
            addrs.extend(a.writes.iter().map(|w| w.0));
        }
    }
    addrs.sort();
    addrs.dedup();
    r += "    pre-state of written accounts:\n";
    for l in account_lines(o, &addrs) {
        r += &format!("      {l}\n");
    }
    r += &format!("    shrunk wire = {}\n", hex_short(&wire, 8192));
    r
}

/// One-line human summary of a stats block.
pub fn summary_line(st: &DiffStats) -> String {
    let kinds: Vec<String> = KINDS
        .iter()
        .map(|k| format!("{k}={}", st.entries_by_kind.get(*k).copied().unwrap_or(0)))
        .collect();
    format!(
        "{} chains, {} blocks ({} rejected framing, {} empty), {} entries -> {} items ({} applied, \
         {} no-op, {} outputs) in {:.1}s [{}]",
        st.chains,
        st.blocks,
        st.blocks_rejected,
        st.blocks_empty,
        st.entries,
        st.items,
        st.applied,
        st.noops,
        st.outputs,
        st.seconds,
        kinds.join(" ")
    )
}
