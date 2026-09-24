//! Helpers shared by the integration tests. Each test binary uses a subset.
#![allow(dead_code)]

use alloy_primitives::{Address, B256};
use lean_lane_node::chain::LeanBlock;
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome};
use lean_lane_node::state::Acct;
use lean_native::envelope::{current_lane_domain, LeanSigned};
use lean_native::{lean_fee, LeanTx, Output, AMOUNT_UNIT};
use secp256k1::SecretKey;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

pub fn cfg(dir: &std::path::Path) -> Config {
    Config {
        datadir: dir.to_path_buf(),
        ..Default::default()
    }
}

pub fn open_cfg(cfg: Config) -> Arc<LaneNode> {
    Arc::new(LaneNode::open(cfg).unwrap().0)
}

pub fn open(dir: &std::path::Path) -> Arc<LaneNode> {
    open_cfg(cfg(dir))
}

/// Relaxed read of a metrics counter.
pub fn get(c: &AtomicU64) -> u64 {
    c.load(Ordering::Relaxed)
}

pub async fn head(n: &LaneNode) -> (B256, u64, u64) {
    *n.head.lock().await
}

pub async fn digest(n: &LaneNode) -> B256 {
    n.state.lock().await.digest()
}

/// A signed fan-out tx from `key`: `outputs` payments of `amount` to
/// addresses `i % 200`.
pub fn signed(key: u64, nonce: u32, outputs: usize, amount: u64) -> LeanSigned {
    let outs = (0..outputs)
        .map(|i| Output {
            to: Address::with_last_byte((i % 200) as u8),
            amount,
        })
        .collect();
    LeanSigned::new(LeanTx::sign(nonce, outs, &sk(key), &current_lane_domain()))
}

pub fn sender(key: u64) -> Address {
    signed(key, 0, 1, 1)
        .tx()
        .recover_sender(&current_lane_domain())
        .unwrap()
}

/// What one `outputs`-wide tx paying `amount` each costs, fee included.
pub fn cost(outputs: usize, amount: u64) -> u128 {
    lean_fee(outputs) + outputs as u128 * amount as u128 * AMOUNT_UNIT
}

pub async fn fund(n: &LaneNode, key: u64, balance: u128) {
    n.seed_account(sender(key), Acct { nonce: 0, balance })
        .await;
}

/// Seed `key` (100 txs' worth) and return one of its signed txs, raw.
pub async fn funded_tx(n: &LaneNode, key: u64, nonce: u32, outputs: usize) -> Vec<u8> {
    fund(n, key, 100 * cost(outputs, 3)).await;
    signed(key, nonce, outputs, 3).raw().to_vec()
}

/// Admit one tx from `key` (seeding it on nonce 0).
pub async fn seed_and_submit(n: &LaneNode, key: u64, nonce: u32, outputs: usize) {
    if nonce == 0 {
        fund(n, key, 10 * cost(outputs, 3)).await;
    }
    n.submit_raw(signed(key, nonce, outputs, 3).raw())
        .await
        .unwrap();
}

/// `arc_buildBlock` on the current head (which also self-stages the result).
pub async fn build_next(n: &LaneNode) -> (B256, Vec<u8>) {
    let (hc, hn, ts) = head(n).await;
    n.shim_build(hc, hn + 1, ts + 1000, 1_000_000)
        .await
        .unwrap()
}

/// Admit one tx, build head+1 and commit it by bytes.
pub async fn advance(n: &LaneNode, key: u64, nonce: u32) -> (B256, Vec<u8>) {
    seed_and_submit(n, key, nonce, 2).await;
    let (c, wire) = build_next(n).await;
    assert_eq!(n.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(c));
    (c, wire)
}

/// A block on an arbitrary parent with `txs` funded txs (keys `key..`),
/// built by hand so nothing is staged for it.
pub async fn wire_child(
    n: &LaneNode,
    parent: B256,
    number: u64,
    timestamp_ms: u64,
    key: u64,
    txs: usize,
) -> (B256, Vec<u8>) {
    let mut raw = Vec::new();
    for i in 0..txs {
        raw.push(funded_tx(n, key + i as u64, 0, 4).await);
    }
    let b = LeanBlock::new(parent, number, timestamp_ms, raw);
    (b.commitment, b.to_wire_bytes())
}

/// head+1 built by hand (not via `arc_buildBlock`, which would stage it).
pub async fn wire_head_plus_one(n: &LaneNode, key: u64, txs: usize) -> (B256, Vec<u8>) {
    let (hc, hn, ts) = head(n).await;
    wire_child(n, hc, hn + 1, ts + 1000, key, txs).await
}

/// Records physically in `blocks.log`, counted off the length-prefix framing
/// (`[u32 len][payload][u32 len]`). A block appended twice shows up here and
/// nowhere else: the number index and the head both still look right.
pub fn records_on_disk(dir: &std::path::Path) -> usize {
    let bytes = std::fs::read(dir.join("blocks.log")).unwrap_or_default();
    let (mut at, mut n) = (0usize, 0usize);
    while at + 4 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let end = at + 4 + len + 4;
        if end > bytes.len() {
            break;
        }
        n += 1;
        at = end;
    }
    n
}

/// Build head+1 from the pool under `budget_gas` (`arc_buildBlock`) and
/// commit it through the FULL execution path (`arc_newBlock{blockBytes}` with
/// the self-staged copy dropped, so `commit_block` runs rather than a
/// promote). Returns the new height, or `None` (appending nothing) if the pool
/// had nothing to pack.
pub async fn produce(n: &LaneNode, timestamp_ms: u64, budget_gas: u64) -> Option<u64> {
    let (head_c, head_n, _) = head(n).await;
    let (c, wire) = n
        .shim_build(head_c, head_n + 1, timestamp_ms, budget_gas)
        .await
        .unwrap();
    n.staged.lock().await.retain(|(k, _)| *k != c);
    if LeanBlock::from_wire_bytes(&wire).unwrap().txs.is_empty() {
        return None;
    }
    assert_eq!(n.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(c));
    Some(head_n + 1)
}

/// Persist the current state as the genesis snapshot, so accounts seeded
/// with `seed_account` (a test hook, not a logged event) survive a reopen.
pub async fn snapshot_now(n: &LaneNode) {
    let (c, num, ts) = head(n).await;
    n.state
        .lock()
        .await
        .write_snapshot(&n.cfg.datadir, c, num, ts)
        .unwrap();
}

pub async fn wait_head(n: &LaneNode, want: u64, secs: u64) -> bool {
    let t0 = Instant::now();
    while head(n).await.1 != want {
        if t0.elapsed() > Duration::from_secs(secs) {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    true
}

/// xorshift64: deterministic, seedable, no dependency.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}
