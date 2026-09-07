//! Cross-pin vectors for the arc-side CL (I2/I3): fixed inputs → canonical
//! blockBytes hex + v2 commitment. The arc decoder must reproduce these
//! EXACTLY. Run: cargo run -p lean-lane-node --example gen_vector

use alloy_primitives::{hex, Address};
use lean_lane_node::chain::{genesis_commitment, LeanBlock};
use lean_native::envelope::LeanSigned;
use lean_native::{lane_domain, LeanTx, Output};
use secp256k1::SecretKey;

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

fn main() {
    let chain_id = 1338u64;
    let domain = lane_domain(chain_id);
    let g = genesis_commitment(chain_id);
    println!("chain_id            : {chain_id}");
    println!("lane_domain         : {domain}");
    println!("genesis_commitment  : {g}");
    println!();

    // tx 1: key 1, nonce 0, single output
    let t1 = LeanSigned::new(LeanTx::sign(
        0,
        vec![Output { to: Address::with_last_byte(0x11), amount: 5 }],
        &sk(1),
        &domain,
    ));
    // tx 2: key 2, nonce 0, 3 outputs incl. zero-value
    let t2 = LeanSigned::new(LeanTx::sign(
        0,
        vec![
            Output { to: Address::with_last_byte(0x22), amount: 7 },
            Output { to: Address::with_last_byte(0x33), amount: 0 },
            Output { to: Address::with_last_byte(0x22), amount: 9 },
        ],
        &sk(2),
        &domain,
    ));
    for (i, t) in [&t1, &t2].iter().enumerate() {
        println!("tx{}_sender          : {}", i + 1, t.tx().recover_sender(&domain).unwrap());
        println!("tx{}_hash            : {}", i + 1, t.hash());
        println!("tx{}_bytes           : 0x{}", i + 1, hex::encode(t.raw()));
    }
    println!();

    // vector block: number 1, ts_ms 1234, parent = genesis, txs [t1, t2]
    let block =
        LeanBlock::new(g, 1, 1234, vec![t1.raw().to_vec(), t2.raw().to_vec()]);
    let wire = block.to_wire_bytes();
    println!("block_number        : {}", block.number);
    println!("block_timestamp_ms  : {}", block.timestamp_ms);
    println!("block_commitment_v2 : {}", block.commitment);
    println!("blockBytes          : 0x{}", hex::encode(&wire));
    println!();

    // empty heartbeat block on top
    let hb = LeanBlock::new(block.commitment, 2, 1500, vec![]);
    println!("empty_block_commitment_v2 : {}", hb.commitment);
    println!("empty_blockBytes          : 0x{}", hex::encode(hb.to_wire_bytes()));
}
