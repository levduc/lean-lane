//! The whole pipeline: framing errors, the commitment, `process_block` ==
//! its phases, serial == parallel, and a golden vector that pins the engine's
//! output bit-for-bit (any engine rewrite must reproduce it).

use alloy_primitives::{b256, Address, B256};
use lean_core::framing::{commitment_of_list, to_wire};
use lean_core::{
    apply, frame, process_block, recover_txs, scan, Acct, BlockError, BlockHeader, EngineCtx,
    FlatState,
};
use lean_native::{lane_domain, LeanTx, Output};
use secp256k1::SecretKey;

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

fn ctx() -> EngineCtx {
    EngineCtx::new(1338, Address::with_last_byte(0xbe))
}

fn header() -> BlockHeader {
    BlockHeader {
        parent: B256::repeat_byte(0x42),
        number: 7,
        timestamp_ms: 1_700_000_000_000,
    }
}

/// A deterministic block over 60 senders: fan-outs of 1/5/10/50/100 outputs,
/// a stale nonce, an overdraft, garbage, an empty entry and a truncated tx.
fn golden_block() -> (FlatState, Vec<Vec<u8>>) {
    let d = lane_domain(1338);
    let pattern = [1usize, 5, 10, 50, 100];
    let mut state = FlatState::default();
    let mut txs = Vec::new();
    for i in 0..60u64 {
        let n = pattern[i as usize % pattern.len()];
        let outs: Vec<Output> = (0..n)
            .map(|j| Output {
                to: Address::with_last_byte(((i as usize * 7 + j) % 251) as u8),
                amount: 1 + (i * 1_000 + j as u64),
            })
            .collect();
        let nonce = if i == 13 { 5 } else { 0 }; // stale nonce -> no-op
        let tx = LeanTx::sign(nonce, outs, &sk(i), &d);
        let sender = tx.recover_sender(&d).unwrap();
        let balance = if i == 21 {
            1
        } else {
            1_000_000_000_000_000_000u128
        }; // overdraft
        state.accounts.insert(sender, Acct { nonce: 0, balance });
        txs.push(tx.encode());
        match i % 17 {
            3 => txs.push(vec![0xde, 0xad]),
            9 => txs.push(Vec::new()),
            11 => {
                let mut t = LeanTx::sign(
                    0,
                    vec![Output {
                        to: Address::ZERO,
                        amount: 1,
                    }],
                    &sk(999),
                    &d,
                )
                .encode();
                t.truncate(30);
                txs.push(t);
            }
            _ => {}
        }
    }
    (state, txs)
}

#[test]
fn framing_errors_keep_the_node_messages() {
    let wire = to_wire(&header(), &[vec![1u8, 2, 3], vec![4]]);
    assert_eq!(scan(&wire[..51]).unwrap_err(), BlockError::TooShort);
    assert_eq!(
        scan(&wire[..54]).unwrap_err(),
        BlockError::TruncatedLength(0)
    );
    assert_eq!(scan(&wire[..57]).unwrap_err(), BlockError::TruncatedBody(0));
    let mut long = wire.clone();
    long.push(0);
    assert_eq!(scan(&long).unwrap_err(), BlockError::TrailingBytes);
    assert_eq!(
        BlockError::TruncatedBody(3).to_string(),
        "wire block truncated at tx 3 body"
    );
    assert_eq!(BlockError::TooShort.to_string(), "wire block too short");
    assert_eq!(
        BlockError::TrailingBytes.to_string(),
        "trailing bytes after wire block"
    );
}

#[test]
fn wire_commitment_equals_list_commitment() {
    let (_, txs) = golden_block();
    let wire = to_wire(&header(), &txs);
    let f = frame(&wire).unwrap();
    assert_eq!(f.header, header());
    assert_eq!(f.commitment, commitment_of_list(&header(), &txs));
    assert_eq!(f.txs.len(), txs.len());
    assert!(f.txs.iter().zip(&txs).all(|(a, b)| *a == b.as_slice()));
}

#[test]
fn process_block_equals_its_phases_and_serial_equals_parallel() {
    let (state, txs) = golden_block();
    let wire = to_wire(&header(), &txs);
    let p = process_block(&state, &wire, &ctx()).unwrap();
    let s = process_block(&state, &wire, &ctx().with_parallel_recovery(false)).unwrap();
    let items = recover_txs(&txs, &ctx());
    let d = lean_core::execute(&state, &items, &ctx());
    assert_eq!(p.delta.writes, d.writes);
    assert_eq!(p.delta.evict, d.evict);
    assert_eq!(p.delta.outcome, d.outcome);
    assert_eq!(s.delta.writes, d.writes);
    assert_eq!(s.delta.evict, d.evict);
    assert_eq!(p.header, header());
}

/// The golden vector. If this fails after an engine change, the change is
/// NOT behaviour-preserving — do not update the constants to make it pass.
#[test]
fn golden_vector() {
    let (mut state, txs) = golden_block();
    let wire = to_wire(&header(), &txs);
    let p = process_block(&state, &wire, &ctx()).unwrap();
    assert_eq!(
        (
            p.delta.outcome.applied,
            p.delta.outcome.noops,
            p.delta.outcome.outputs
        ),
        (58, 2, 1_937)
    );
    apply(&mut state, &p.delta);
    assert_eq!(
        p.commitment,
        b256!("212fc88e4ad8033f86aadd8b708363d10c2b9a4ec62062064ff887c7ed49a017")
    );
    assert_eq!(
        state.digest(),
        b256!("80ab43a37548dbe5b58f820cbd9091d80b4fa98050c2fb0165a8f75ac61c61bf")
    );
}
