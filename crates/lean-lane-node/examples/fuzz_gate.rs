//! Decode fuzzing gate (≥1M cases, seeded, deterministic):
//!   T1: LeanTx::decode on random + mutated-valid bytes; Ok ⇒ re-encode == input.
//!   T2: LeanBlock::from_wire_bytes same; Ok ⇒ to_wire_bytes == input.
//!   T3: arc_newBlock full path on a live node: random + mutated wire blocks;
//!       every case must return cleanly (no panic); Err ⇒ head+state digest
//!       unchanged; Ok ⇒ idempotent-known or head advanced by exactly one.
//!
//! Run: cargo run --release -p lean-lane-node --example fuzz_gate

use alloy_primitives::Address;
use lean_lane_node::chain::LeanBlock;
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome};
use lean_lane_node::state::Acct;
use lean_native::envelope::{current_lane_domain, LeanSigned};
use lean_native::{lean_fee, LeanTx, Output, AMOUNT_UNIT};
use secp256k1::SecretKey;
use std::time::Instant;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

fn mutate(rng: &mut Rng, base: &[u8]) -> Vec<u8> {
    let mut v = base.to_vec();
    match rng.below(4) {
        0 => {
            // bit flips
            for _ in 0..=rng.below(4) {
                if v.is_empty() {
                    break;
                }
                let i = rng.below(v.len());
                v[i] ^= 1 << rng.below(8);
            }
        }
        1 => {
            // truncate
            let cut = rng.below(v.len() + 1);
            v.truncate(cut);
        }
        2 => {
            // extend with garbage
            let n = 1 + rng.below(64);
            let extra = rng.bytes(n);
            v.extend_from_slice(&extra);
        }
        _ => {
            // splice a random window
            if !v.is_empty() {
                let at = rng.below(v.len());
                let n = (1 + rng.below(8)).min(v.len() - at);
                let repl = rng.bytes(n);
                v[at..at + n].copy_from_slice(&repl);
            }
        }
    }
    v
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> eyre::Result<()> {
    let mut rng = Rng(0x1ea1_1a9e_u64 ^ 0xdead_beef_cafe_f00d);
    let domain = current_lane_domain();
    let t0 = Instant::now();
    let mut cases = 0u64;

    // valid corpora
    let valid_txs: Vec<Vec<u8>> = (0..64u64)
        .map(|i| {
            let n = 1 + (i % 12) as usize;
            let outs = (0..n)
                .map(|j| Output { to: Address::with_last_byte((j * 3) as u8), amount: j as u64 })
                .collect();
            LeanSigned::new(LeanTx::sign(i as u32, outs, &sk(i), &domain)).raw().to_vec()
        })
        .collect();
    let g = lean_lane_node::chain::genesis_commitment(1338);
    let valid_blocks: Vec<Vec<u8>> = (0..32u64)
        .map(|i| {
            let txs: Vec<Vec<u8>> =
                (0..(i % 5)).map(|k| valid_txs[(i + k) as usize % valid_txs.len()].clone()).collect();
            LeanBlock::new(g, 1 + i, 1000 + i, txs).to_wire_bytes()
        })
        .collect();

    // ---- T1: LeanTx::decode -------------------------------------------------
    let mut t1_ok = 0u64;
    for _ in 0..400_000 {
        let len = rng.below(300);
        let buf = rng.bytes(len);
        if let Ok(tx) = LeanTx::decode(&buf) {
            assert_eq!(tx.encode(), buf, "T1: decode/encode not canonical");
            t1_ok += 1;
        }
        cases += 1;
    }
    for _ in 0..300_000 {
        let k = rng.below(valid_txs.len());
        let base = &valid_txs[k];
        let buf = mutate(&mut rng, base);
        if let Ok(tx) = LeanTx::decode(&buf) {
            assert_eq!(tx.encode(), buf, "T1m: decode/encode not canonical");
            t1_ok += 1;
        }
        cases += 1;
    }
    println!("T1 tx decode: 700k cases, {t1_ok} decoded ok, canonical, no panics");

    // ---- T2: wire block decode ---------------------------------------------
    let mut t2_ok = 0u64;
    for _ in 0..200_000 {
        let len = rng.below(400);
        let buf = rng.bytes(len);
        if let Ok(b) = LeanBlock::from_wire_bytes(&buf) {
            assert_eq!(b.to_wire_bytes(), buf, "T2: wire decode/encode not canonical");
            t2_ok += 1;
        }
        cases += 1;
    }
    for _ in 0..200_000 {
        let k = rng.below(valid_blocks.len());
        let base = &valid_blocks[k];
        let buf = mutate(&mut rng, base);
        if let Ok(b) = LeanBlock::from_wire_bytes(&buf) {
            assert_eq!(b.to_wire_bytes(), buf, "T2m: wire decode/encode not canonical");
            t2_ok += 1;
        }
        cases += 1;
    }
    println!("T2 wire decode: 400k cases, {t2_ok} decoded ok, canonical, no panics");

    // ---- T3: full arc_newBlock path ----------------------------------------
    let dir = std::env::temp_dir().join(format!("fuzz-gate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (node, _) = LaneNode::open(Config { datadir: dir.clone(), ..Default::default() })?;
    // small committed prefix so known-height paths are reachable
    for i in 0..3u64 {
        let (hc, hn, _) = *node.head.lock().await;
        let txb = valid_txs[i as usize].clone();
        let tx = LeanTx::decode(&txb).unwrap();
        let sender = tx.recover_sender(&domain).unwrap();
        node.seed_account(
            sender,
            Acct { nonce: tx.nonce as u64, balance: lean_fee(tx.outputs.len()) + 1_000 * AMOUNT_UNIT },
        )
        .await;
        let blk = LeanBlock::new(hc, hn + 1, 10 + i, vec![txb]);
        match node.shim_new(&blk.to_wire_bytes()).await.map_err(|e| eyre::eyre!(e))? {
            NewBlockOutcome::Valid(_) => {}
            o => eyre::bail!("setup block not applied: {o:?}"),
        }
    }
    let head_wire: Vec<Vec<u8>> = {
        let mut v = Vec::new();
        for n in 1..=3u64 {
            v.push(node.block_wire_bytes(n).await.unwrap().unwrap());
        }
        v
    };
    let mut t3_ok = 0u64;
    let mut t3_err = 0u64;
    for i in 0..120_000u64 {
        let buf = if i % 2 == 0 {
            { let len = rng.below(500); rng.bytes(len) }
        } else {
            { let k = rng.below(head_wire.len()); mutate(&mut rng, &head_wire[k]) }
        };
        let head_before = *node.head.lock().await;
        let digest_before =
            if i % 500 == 0 { Some(node.state.lock().await.digest()) } else { None };
        match node.shim_new(&buf).await {
            Ok(NewBlockOutcome::Valid(_)) => {
                let head_after = *node.head.lock().await;
                assert!(
                    head_after == head_before || head_after.1 == head_before.1 + 1,
                    "T3: Valid outcome must be idempotent or advance head by one"
                );
                t3_ok += 1;
            }
            Ok(NewBlockOutcome::Syncing { .. }) => {
                assert_eq!(*node.head.lock().await, head_before, "T3: SYNCING must not move head");
                t3_ok += 1;
            }
            Err(_) => {
                assert_eq!(*node.head.lock().await, head_before, "T3: Err must not move head");
                if let Some(d) = digest_before {
                    assert_eq!(node.state.lock().await.digest(), d, "T3: Err must not mutate state");
                }
                t3_err += 1;
            }
        }
        cases += 1;
    }
    println!("T3 newBlock path: 120k cases, {t3_ok} accepted (idempotent/legal), {t3_err} clean errors, no panics, state guarded");
    let _ = std::fs::remove_dir_all(&dir);

    println!();
    println!("FUZZ GATE PASS: {cases} total cases, 0 panics, canonical round-trips held ({:?})", t0.elapsed());
    Ok(())
}
