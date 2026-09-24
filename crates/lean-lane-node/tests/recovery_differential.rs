//! The consensus-critical gate for parallel recovery: `decode_block_txs`
//! (rayon) must return the IDENTICAL item vector — order, skips, senders,
//! hashes — as a plain serial decode, on mixed-N blocks with invalid entries
//! interleaved. A single divergence here is a chain split.

mod common;

use alloy_eips::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, B256};
use common::sk;
use lean_lane_node::node::decode_block_txs as decode_block_txs_parallel;
use lean_native::envelope::{current_lane_domain, ArcTxEnvelope, LeanSigned};
use lean_native::{LeanTx, Output};
use reth_primitives_traits::SignerRecoverable;

/// The serial oracle: one tx at a time, same skips.
fn decode_block_txs_serial(txs: &[Vec<u8>]) -> Vec<(Address, LeanTx, B256)> {
    let mut items = Vec::with_capacity(txs.len());
    for bytes in txs {
        let Ok(env) = ArcTxEnvelope::decode_2718(&mut bytes.as_slice()) else {
            continue;
        };
        let Ok(signer) = env.recover_signer() else {
            continue;
        };
        if let ArcTxEnvelope::Lean(lean) = env {
            let hash = *lean.hash();
            items.push((signer, lean.tx().clone(), hash));
        }
    }
    items
}

fn signed_fanout(key: u64, nonce: u32, n: usize) -> Vec<u8> {
    let outs = (0..n)
        .map(|i| Output {
            to: Address::with_last_byte((i % 250) as u8),
            amount: 1 + i as u64,
        })
        .collect();
    let env = ArcTxEnvelope::Lean(LeanSigned::new(LeanTx::sign(
        nonce,
        outs,
        &sk(key),
        &current_lane_domain(),
    )));
    env.encoded_2718()
}

/// Build the mixed-N block V3 requires: the equal-by-tx cycle {1,5,10,50,100}
/// over many senders, with invalid entries interleaved at fixed positions —
/// garbage bytes, a truncated tx, an empty entry, and a corrupted signature.
fn mixed_block_with_invalid(txs_per_n: usize) -> Vec<Vec<u8>> {
    let pattern = [1usize, 5, 10, 50, 100];
    let mut txs = Vec::new();
    for i in 0..txs_per_n * pattern.len() {
        let n = pattern[i % pattern.len()];
        txs.push(signed_fanout(i as u64, 0, n));
        match i % 7 {
            2 => txs.push(vec![0xde, 0xad, 0xbe, 0xef]), // garbage
            4 => {
                let mut t = signed_fanout(i as u64 + 10_000, 0, 5);
                t.truncate(t.len() / 2); // truncated
                txs.push(t);
            }
            5 => txs.push(Vec::new()), // empty
            6 => {
                let mut t = signed_fanout(i as u64 + 20_000, 0, 10);
                let last = t.len() - 1;
                t[last] ^= 0xFF; // corrupted signature byte
                txs.push(t);
            }
            _ => {}
        }
    }
    txs
}

#[test]
fn parallel_recovery_identical_on_mixed_block_with_invalid_entries() {
    let txs = mixed_block_with_invalid(40); // 200 valid + ~114 invalid entries
    let serial = decode_block_txs_serial(&txs);
    let parallel = decode_block_txs_parallel(&txs);
    assert_eq!(serial.len(), parallel.len(), "item count diverged");
    for (i, (s, p)) in serial.iter().zip(parallel.iter()).enumerate() {
        assert_eq!(s.0, p.0, "sender diverged at item {i}");
        assert_eq!(s.2, p.2, "hash diverged at item {i}");
        assert_eq!(s.1.nonce, p.1.nonce, "nonce diverged at item {i}");
        assert_eq!(s.1.outputs, p.1.outputs, "outputs diverged at item {i}");
    }
}

#[test]
fn parallel_recovery_identical_on_all_invalid_and_empty_blocks() {
    let all_bad: Vec<Vec<u8>> = vec![vec![0xff; 30], Vec::new(), vec![0x02, 0x01]];
    assert_eq!(decode_block_txs_serial(&all_bad).len(), 0);
    assert_eq!(decode_block_txs_parallel(&all_bad).len(), 0);
    let empty: Vec<Vec<u8>> = Vec::new();
    assert_eq!(decode_block_txs_serial(&empty).len(), 0);
    assert_eq!(decode_block_txs_parallel(&empty).len(), 0);
}

/// A corrupted signature usually still RECOVERS (to a wrong, unfunded sender —
/// total-STF then makes it a no-op at execution). The differential contract is
/// that serial and parallel agree on WHATEVER the recovery outcome is, wrong
/// sender included — assert that explicitly on a corrupted-but-recoverable tx.
#[test]
fn corrupted_sig_agrees_between_paths() {
    let mut t = signed_fanout(7, 3, 10);
    let mid = t.len() - 20;
    t[mid] ^= 0x55;
    let txs = vec![t];
    let s = decode_block_txs_serial(&txs);
    let p = decode_block_txs_parallel(&txs);
    assert_eq!(s.len(), p.len());
    if let (Some(a), Some(b)) = (s.first(), p.first()) {
        assert_eq!(a.0, b.0, "recovered (wrong) sender must be identical");
    }
}
