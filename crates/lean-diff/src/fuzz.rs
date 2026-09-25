//! Seeded mutational fuzzer over the decode paths of the engine and the
//! oracle (cargo-fuzz is not available; this runs inside `cargo test`).
//!
//! Iteration `i` is fully determined by `(seed, i)`: it picks a corpus tx or
//! block, stacks 1–4 mutations (bit flips, interesting bytes, truncation,
//! extension, splicing, n_outputs / v / r / s / nonce overwrites, and for
//! blocks n_txs / length-prefix / entry-level edits), then asserts:
//!
//! * neither side panics;
//! * tx: prefix decode (engine `decode_txs` = the hot path, oracle
//!   `decode_tx_prefix`) and strict decode (`LeanTx::decode` vs `decode_tx`)
//!   give the same verdict and the same parsed fields and tx hash; when it
//!   decodes, sender recovery (engine `recover_txs`, serial or parallel, vs
//!   oracle `classify_entry`) gives the same sender or the same failure;
//! * block: framing (`lean_core::frame`/`scan` vs `decode_block`) gives the
//!   same verdict (same error kind and entry index), header, entries and
//!   commitment; a share of blocks is also fully executed on a funded state
//!   and compared like the differential test.

use crate::norm::{describe_diff, engine_block, oracle_block, FrameErr};
use crate::rng::{mix, Rng};
use crate::util::{
    add_be, be32_from_u64, catch, ctx, domain, engine_genesis, hex_short, keys, sub_be, Key, N,
};
use crate::CHAIN_ID;
use alloy_primitives::{keccak256, Address};
use k256::elliptic_curve::ops::Reduce;
use k256::elliptic_curve::point::AffineCoordinates;
use k256::{FieldBytes, ProjectivePoint, Scalar, U256};
use lean_core::FlatState;
use lean_native::{LeanTx, Output};
use lean_oracle::{encode_block, genesis_state, Header, State};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

#[derive(Clone, Debug, Default, Serialize)]
pub struct FuzzStats {
    pub iterations: u64,
    pub tx_inputs: u64,
    pub tx_prefix_wellformed: u64,
    pub tx_strict_wellformed: u64,
    pub recoveries_compared: u64,
    pub recovered: u64,
    pub block_inputs: u64,
    pub blocks_well_framed: u64,
    pub blocks_executed: u64,
    pub seconds: f64,
}

impl FuzzStats {
    fn add(&mut self, o: &FuzzStats) {
        self.iterations += o.iterations;
        self.tx_inputs += o.tx_inputs;
        self.tx_prefix_wellformed += o.tx_prefix_wellformed;
        self.tx_strict_wellformed += o.tx_strict_wellformed;
        self.recoveries_compared += o.recoveries_compared;
        self.recovered += o.recovered;
        self.block_inputs += o.block_inputs;
        self.blocks_well_framed += o.blocks_well_framed;
        self.blocks_executed += o.blocks_executed;
    }
}

#[derive(Clone, Debug)]
pub struct FuzzFailure {
    pub seed: u64,
    pub iteration: u64,
    pub report: String,
}

impl std::fmt::Display for FuzzFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "FUZZ MISMATCH seed={} iteration={}: {}\n  reproduce: lean-diff fuzz --seed {} \
             --from {} --iters 1",
            self.seed, self.iteration, self.report, self.seed, self.iteration
        )
    }
}

/// The shared seed corpus.
pub struct Corpus {
    pub txs: Vec<Vec<u8>>,
    pub blocks: Vec<(Header, Vec<Vec<u8>>)>,
    pub ostate: State,
    pub estate: FlatState,
}

/// A tx whose recovery hits the point at infinity: R = kG, r = x(R),
/// s = z·k⁻¹ ⇒ Q = r⁻¹(sR − zG) = 0 (SPEC-QUESTIONS Q2). Both sides must
/// treat it as unrecoverable.
pub fn infinity_tx(key: &Key, k: u64) -> Vec<u8> {
    let mut tx = key.sign(
        7,
        vec![Output {
            to: Address::with_last_byte(0x77),
            amount: 1,
        }],
    );
    let z = <Scalar as Reduce<U256>>::reduce_bytes(&FieldBytes::from(tx.signing_hash(&domain()).0));
    let k = Scalar::from(k.max(2));
    let r_pt = (ProjectivePoint::GENERATOR * k).to_affine();
    let x = r_pt.x();
    let s = z * Option::<Scalar>::from(k.invert()).expect("k != 0");
    tx.sig[..32].copy_from_slice(&x);
    tx.sig[32..64].copy_from_slice(&s.to_bytes());
    tx.sig[64] = u8::from(bool::from(r_pt.y_is_odd()));
    debug_assert!(!bool::from(s.is_zero()));
    tx.encode()
}

impl Corpus {
    pub fn new(seed: u64) -> Corpus {
        let mut rng = Rng::new(seed ^ 0xc0de);
        let pool = keys();
        let mut txs = Vec::new();
        for (i, key) in pool.iter().take(24).enumerate() {
            for nonce in 0..3u32 {
                let n = *rng.pick(&[1usize, 1, 1, 2, 3, 5, 17, 100]);
                let outs = (0..n)
                    .map(|j| Output {
                        to: if j % 3 == 0 {
                            pool[(i + j) % pool.len()].addr
                        } else {
                            let mut a = [0u8; 20];
                            rng.fill(&mut a);
                            Address::from(a)
                        },
                        amount: rng.below(1_000_000),
                    })
                    .collect();
                txs.push(key.sign(nonce, outs).encode());
            }
        }
        // One maximal tx, the golden T0 and its high-s twin, crafted
        // point-at-infinity recoveries.
        let big = (0..lean_native::MAX_OUTPUTS)
            .map(|j| Output {
                to: Address::with_last_byte(j as u8),
                amount: j as u64,
            })
            .collect();
        txs.push(pool[0].sign(0, big).encode());
        let t0 = {
            let mut s = [0u8; 32];
            s[31] = 1;
            Key::from_secret(s).unwrap().sign(
                0,
                vec![
                    Output {
                        to: Address::from([0x11; 20]),
                        amount: 5,
                    },
                    Output {
                        to: Address::from([0x22; 20]),
                        amount: 1_000_000_000,
                    },
                ],
            )
        };
        let mut twin = t0.clone();
        twin.sig = crate::util::high_s_twin(&t0.sig);
        txs.push(t0.encode());
        txs.push(twin.encode());
        for k in [2u64, 3, 0xdead_beef] {
            txs.push(infinity_tx(&pool[1], k));
        }

        let mut blocks = Vec::new();
        for b in 0..24u64 {
            let n = rng.usize(0, 8);
            let mut entries: Vec<Vec<u8>> = (0..n)
                .map(|_| {
                    if rng.chance(0.85) {
                        rng.pick(&txs[..txs.len() - 6]).clone()
                    } else {
                        let l = rng.usize(0, 40);
                        rng.bytes(l)
                    }
                })
                .collect();
            if b == 0 {
                entries.clear();
            }
            let mut parent = [0u8; 32];
            rng.fill(&mut parent);
            blocks.push((
                Header {
                    parent,
                    number: rng.next_u64(),
                    timestamp_ms: rng.next_u64(),
                },
                entries,
            ));
        }
        let fund: String = pool
            .iter()
            .take(24)
            .map(|k| format!("{:?}\n", k.addr))
            .collect();
        let bal = 1_000_000_000_000_000_000_000u128;
        Corpus {
            txs,
            blocks,
            ostate: genesis_state(&fund, bal).expect("corpus fund file"),
            estate: engine_genesis(&fund, bal).expect("corpus fund file"),
        }
    }
}

const INTERESTING_U8: &[u8] = &[
    0x00, 0x01, 0x02, 0x03, 0x04, 0x1b, 0x1c, 0x50, 0x7f, 0x80, 0xff,
];

fn interesting_u16(n: u16) -> Vec<u16> {
    vec![
        0,
        1,
        2,
        n.wrapping_sub(1),
        n.wrapping_add(1),
        9_999,
        10_000,
        10_001,
        0x7fff,
        0xffff,
    ]
}

fn interesting_scalar(rng: &mut Rng) -> [u8; 32] {
    match rng.below(10) {
        0 => [0u8; 32],
        1 => be32_from_u64(1),
        2 => sub_be(&N, &be32_from_u64(1)),
        3 => N,
        4 => add_be(&N, &be32_from_u64(1)),
        5 => [0xff; 32],
        6 => {
            // below p − n: x = r + n is a field element (v ∈ {2,3}).
            let mut r = [0u8; 32];
            rng.fill(&mut r[16..]);
            r
        }
        7 => {
            let mut h = N;
            h[0] = 0x7f; // ≈ n/2
            h
        }
        _ => {
            let mut r = [0u8; 32];
            rng.fill(&mut r);
            r
        }
    }
}

/// Offset of the signature given the header's claimed N (clamped to the
/// buffer), or None if the buffer is too short to hold a header.
fn sig_at(b: &[u8]) -> Option<usize> {
    if b.len() < 7 {
        return None;
    }
    let n = u16::from_le_bytes([b[5], b[6]]) as usize;
    let at = 7 + 28 * n;
    (at + 65 <= b.len()).then_some(at)
}

fn mutate_bytes(rng: &mut Rng, b: &mut Vec<u8>, corpus: &Corpus) {
    match rng.below(12) {
        0 if !b.is_empty() => {
            let i = rng.usize(0, b.len() - 1);
            b[i] ^= 1 << rng.below(8);
        }
        1 if !b.is_empty() => {
            let i = if rng.chance(0.3) {
                0
            } else {
                rng.usize(0, b.len() - 1)
            };
            b[i] = *rng.pick(INTERESTING_U8);
        }
        2 if !b.is_empty() => {
            let len = b.len();
            let any = rng.usize(0, len - 1);
            let cut = *rng.pick(&[0, 1, 6, 7, 70, 71, 72, 99, 100, len - 1, any]);
            b.truncate(cut.min(len - 1));
        }
        3 => {
            let extra = rng.usize(1, 64);
            let add = rng.bytes(extra);
            b.extend(add);
        }
        4 => {
            let other = rng.pick(&corpus.txs);
            let i = rng.usize(0, b.len());
            let j = rng.usize(0, other.len());
            b.truncate(i);
            b.extend_from_slice(&other[j..]);
        }
        5 => {
            let at = rng.usize(0, b.len());
            let len = rng.usize(1, 32);
            let ins = rng.bytes(len);
            b.splice(at..at, ins);
        }
        6 if b.len() > 1 => {
            let at = rng.usize(0, b.len() - 1);
            let len = rng.usize(1, (b.len() - at).min(64));
            b.drain(at..at + len);
        }
        7 if b.len() >= 7 => {
            let n = u16::from_le_bytes([b[5], b[6]]);
            let v = *rng.pick(&interesting_u16(n));
            b[5..7].copy_from_slice(&v.to_le_bytes());
        }
        8 => {
            if let Some(at) = sig_at(b) {
                b[at + 64] = *rng.pick(&[0u8, 1, 2, 3, 4, 27, 28, 0xff]);
            }
        }
        9 => {
            if let Some(at) = sig_at(b) {
                let off = if rng.chance(0.5) { at } else { at + 32 };
                let s = interesting_scalar(rng);
                b[off..off + 32].copy_from_slice(&s);
            }
        }
        10 if b.len() >= 5 => {
            let any = rng.next_u64() as u32;
            let v = *rng.pick(&[0u32, 1, u32::MAX, any]);
            b[1..5].copy_from_slice(&v.to_le_bytes());
        }
        _ if !b.is_empty() => {
            let at = rng.usize(0, b.len() - 1);
            let len = rng.usize(1, (b.len() - at).min(100));
            let chunk = b[at..at + len].to_vec();
            let to = rng.usize(0, b.len());
            b.splice(to..to, chunk);
        }
        _ => b.push(rng.next_u64() as u8),
    }
}

fn mutate_block(rng: &mut Rng, corpus: &Corpus) -> Vec<u8> {
    let (h, entries) = rng.pick(&corpus.blocks);
    let mut entries = entries.clone();
    let mut h = h.clone();
    // Entry-level edits keep the framing valid.
    for _ in 0..rng.usize(0, 3) {
        match rng.below(5) {
            0 if !entries.is_empty() => {
                let i = rng.usize(0, entries.len() - 1);
                mutate_bytes(rng, &mut entries[i], corpus);
            }
            1 if !entries.is_empty() => {
                let i = rng.usize(0, entries.len() - 1);
                entries.push(entries[i].clone());
            }
            2 if !entries.is_empty() => {
                let i = rng.usize(0, entries.len() - 1);
                entries.remove(i);
            }
            3 => entries.push(rng.pick(&corpus.txs).clone()),
            _ => h.number = rng.next_u64(),
        }
    }
    let mut w = encode_block(&h, &entries);
    // Framing-level edits.
    if rng.chance(0.6) {
        let mut offs = Vec::new();
        let mut at = 52;
        for e in &entries {
            offs.push(at);
            at += 4 + e.len();
        }
        match rng.below(6) {
            0 => {
                let n = entries.len() as u32;
                let v = *rng.pick(&[0u32, 1, n.wrapping_sub(1), n + 1, u32::MAX]);
                w[48..52].copy_from_slice(&v.to_le_bytes());
            }
            1 if !offs.is_empty() => {
                let o = *rng.pick(&offs);
                let len = u32::from_le_bytes(w[o..o + 4].try_into().unwrap());
                let any = rng.next_u64() as u32;
                let v = *rng.pick(&[
                    0u32,
                    len.wrapping_sub(1),
                    len.wrapping_add(1),
                    len.wrapping_add(4),
                    u32::MAX,
                    any,
                ]);
                w[o..o + 4].copy_from_slice(&v.to_le_bytes());
            }
            2 if !offs.is_empty() => {
                let o = *rng.pick(&offs) as i64 + rng.range(0, 8) as i64 - 4;
                w.truncate(o.clamp(0, w.len() as i64) as usize);
            }
            3 => {
                let extra = rng.usize(1, 8);
                let add = rng.bytes(extra);
                w.extend(add);
            }
            4 => {
                let cut = rng.usize(0, w.len().min(60));
                w.truncate(cut);
            }
            _ => mutate_bytes(rng, &mut w, corpus),
        }
    }
    w
}

fn frame_kind_engine(e: lean_core::BlockError) -> FrameErr {
    match e {
        lean_core::BlockError::TooShort => FrameErr::TooShort,
        lean_core::BlockError::TruncatedLength(i) => FrameErr::TruncatedLength(i as u64),
        lean_core::BlockError::TruncatedBody(i) => FrameErr::TruncatedBody(i as u64),
        lean_core::BlockError::TrailingBytes => FrameErr::TrailingBytes,
    }
}

fn frame_kind_oracle(e: &lean_oracle::BlockError) -> FrameErr {
    match *e {
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

fn same_tx(e: &LeanTx, o: &lean_oracle::Tx) -> bool {
    e.nonce == o.nonce
        && e.outputs.len() == o.outputs.len()
        && e.outputs
            .iter()
            .zip(&o.outputs)
            .all(|(x, y)| x.to.0 .0 == y.to && x.amount == y.amount)
        && e.sig[..32] == o.r
        && e.sig[32..64] == o.s
        && e.sig[64] == o.v
}

/// Check one tx-shaped input. `Err` describes the disagreement.
pub fn check_tx(b: &[u8], parallel: bool, recover: bool, st: &mut FuzzStats) -> Result<(), String> {
    st.tx_inputs += 1;
    let input = || format!("input {}", hex_short(b, 4096));
    let o_pre = catch(|| lean_oracle::decode_tx_prefix(b))
        .map_err(|p| format!("oracle decode_tx_prefix PANIC {p}; {}", input()))?;
    let o_strict = catch(|| lean_oracle::decode_tx(b))
        .map_err(|p| format!("oracle decode_tx PANIC {p}; {}", input()))?;
    let e_pre = catch(|| lean_core::decode_txs(&[b], &ctx(parallel)))
        .map_err(|p| format!("engine decode_txs PANIC {p}; {}", input()))?;
    let e_strict = catch(|| LeanTx::decode(b))
        .map_err(|p| format!("engine LeanTx::decode PANIC {p}; {}", input()))?;

    if o_pre.is_ok() != (e_pre.len() == 1) || e_pre.len() > 1 {
        return Err(format!(
            "prefix-decode verdict: engine {} item(s), oracle {:?}; {}",
            e_pre.len(),
            o_pre.as_ref().map(|_| "well-formed"),
            input()
        ));
    }
    if let (Ok(o), Some(e)) = (&o_pre, e_pre.first()) {
        st.tx_prefix_wellformed += 1;
        if !same_tx(&e.tx, o) {
            return Err(format!("prefix-decode fields differ; {}", input()));
        }
        let canon = o.encode();
        if e.hash.0 != lean_oracle::tx_hash(&canon) || e.hash != keccak256(&b[..canon.len()]) {
            return Err(format!("tx hash differs; {}", input()));
        }
    }
    if o_strict.is_ok() != e_strict.is_ok() {
        return Err(format!(
            "strict-decode verdict: engine {:?} oracle {:?}; {}",
            e_strict.as_ref().err(),
            o_strict.as_ref().err(),
            input()
        ));
    }
    if let (Ok(o), Ok(e)) = (&o_strict, &e_strict) {
        st.tx_strict_wellformed += 1;
        if !same_tx(e, o) {
            return Err(format!("strict-decode fields differ; {}", input()));
        }
    }
    if recover && o_pre.is_ok() {
        st.recoveries_compared += 1;
        let e = catch(|| lean_core::recover_txs(&[b], &ctx(parallel)))
            .map_err(|p| format!("engine recover_txs PANIC {p}; {}", input()))?;
        let o = catch(|| lean_oracle::classify_entry(b, CHAIN_ID))
            .map_err(|p| format!("oracle classify_entry PANIC {p}; {}", input()))?;
        let es = e.first().map(|x| (x.0 .0 .0, x.2 .0));
        let os = o.as_ref().map(|x| (x.sender, x.tx_hash));
        if es != os {
            return Err(format!(
                "sender recovery: engine {} oracle {} (parallel={parallel}); {}",
                es.map(|x| format!("0x{}", hex::encode(x.0)))
                    .unwrap_or("unrecoverable".into()),
                os.map(|x| format!("0x{}", hex::encode(x.0)))
                    .unwrap_or("unrecoverable".into()),
                input()
            ));
        }
        if es.is_some() {
            st.recovered += 1;
        }
    }
    Ok(())
}

/// Check one block-shaped input.
pub fn check_block(
    w: &[u8],
    corpus: &Corpus,
    parallel: bool,
    execute: bool,
    st: &mut FuzzStats,
) -> Result<(), String> {
    st.block_inputs += 1;
    let input = || format!("wire {}", hex_short(w, 4096));
    let e = catch(|| {
        lean_core::frame(w).map(|f| {
            (
                f.header,
                f.commitment,
                f.txs.iter().map(|t| t.to_vec()).collect::<Vec<_>>(),
            )
        })
    })
    .map_err(|p| format!("engine frame PANIC {p}; {}", input()))?;
    let e_scan = catch(|| lean_core::scan(w).map(|(h, t)| (h, t.len())))
        .map_err(|p| format!("engine scan PANIC {p}; {}", input()))?;
    let o = catch(|| lean_oracle::decode_block(w))
        .map_err(|p| format!("oracle decode_block PANIC {p}; {}", input()))?;
    let ev = e.as_ref().map(|_| ()).map_err(|x| frame_kind_engine(*x));
    let sv = e_scan
        .as_ref()
        .map(|_| ())
        .map_err(|x| frame_kind_engine(*x));
    let ov = o.as_ref().map(|_| ()).map_err(frame_kind_oracle);
    if ev != ov || sv != ev {
        return Err(format!(
            "framing verdict: engine frame {ev:?} scan {sv:?} oracle {ov:?}; {}",
            input()
        ));
    }
    if let (Ok((h, c, txs)), Ok(b)) = (&e, &o) {
        st.blocks_well_framed += 1;
        if h.parent.0 != b.header.parent
            || h.number != b.header.number
            || h.timestamp_ms != b.header.timestamp_ms
        {
            return Err(format!("framed header differs; {}", input()));
        }
        if *txs != b.entries {
            return Err(format!("framed entries differ; {}", input()));
        }
        if c.0 != b.commitment() {
            return Err(format!(
                "commitment: engine {c} oracle 0x{}; {}",
                hex::encode(b.commitment()),
                input()
            ));
        }
    }
    if execute {
        st.blocks_executed += 1;
        let (en, _) = engine_block(&corpus.estate, w, parallel);
        let (on, _) = oracle_block(&corpus.ostate, w);
        if let Some(d) = describe_diff("engine", &en, "oracle", &on) {
            return Err(format!(
                "block execution on the funded corpus state: {d}; {}",
                input()
            ));
        }
    }
    Ok(())
}

/// One iteration, fully determined by `(seed, i)`.
pub fn iteration(seed: u64, i: u64, corpus: &Corpus, st: &mut FuzzStats) -> Result<(), String> {
    let mut rng = Rng::new(mix(seed, i));
    st.iterations += 1;
    let parallel = i.is_multiple_of(2);
    if rng.chance(0.7) {
        let mut b = rng.pick(&corpus.txs).clone();
        // Large inputs are mutated but rarely: keep iterations cheap.
        if b.len() > 10_000 && rng.chance(0.9) {
            b = corpus.txs[0].clone();
        }
        let muts = if rng.chance(0.1) { 0 } else { rng.usize(1, 4) };
        for _ in 0..muts {
            mutate_bytes(&mut rng, &mut b, corpus);
        }
        let recover = rng.chance(0.5);
        check_tx(&b, parallel, recover, st)
    } else {
        let w = mutate_block(&mut rng, corpus);
        let execute = rng.chance(0.3);
        check_block(&w, corpus, parallel, execute, st)
    }
}

/// Run iterations `from..from+iters` over worker threads.
pub fn run(
    seed: u64,
    from: u64,
    iters: u64,
    threads: usize,
) -> Result<FuzzStats, Box<FuzzFailure>> {
    let t0 = Instant::now();
    let corpus = Corpus::new(seed);
    let next = AtomicU64::new(from);
    let stop = AtomicBool::new(false);
    let total = Mutex::new(FuzzStats::default());
    let failure: Mutex<Option<Box<FuzzFailure>>> = Mutex::new(None);
    const CHUNK: u64 = 256;
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| {
                let mut st = FuzzStats::default();
                'outer: loop {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let start = next.fetch_add(CHUNK, Ordering::Relaxed);
                    if start >= from + iters {
                        break;
                    }
                    for i in start..(start + CHUNK).min(from + iters) {
                        if let Err(report) = iteration(seed, i, &corpus, &mut st) {
                            stop.store(true, Ordering::Relaxed);
                            let mut f = failure.lock().unwrap();
                            if f.as_ref().is_none_or(|x| i < x.iteration) {
                                *f = Some(Box::new(FuzzFailure {
                                    seed,
                                    iteration: i,
                                    report,
                                }));
                            }
                            break 'outer;
                        }
                    }
                }
                total.lock().unwrap().add(&st);
            });
        }
    });
    if let Some(f) = failure.into_inner().unwrap() {
        return Err(f);
    }
    let mut st = total.into_inner().unwrap();
    st.seconds = t0.elapsed().as_secs_f64();
    Ok(st)
}
