//! Prints a canonical test vector for cross-implementation pinning (the arc
//! repo's spammer re-implements the codec; its unit test must reproduce these
//! bytes exactly). Deterministic: RFC6979 signing, fixed key/inputs.
//!
//!   cargo run -p lean-native --example gen_vector

use alloy_primitives::{hex, Address};
use lean_native::{lane_domain, LeanTx, Output};
use secp256k1::SecretKey;

fn main() {
    let sk = SecretKey::from_slice(&{
        let mut b = [0u8; 32];
        b[31] = 1;
        b
    })
    .unwrap();
    let domain = lane_domain(1338);
    let outputs = vec![
        Output { to: Address::repeat_byte(0x11), amount: 5 },
        Output { to: Address::repeat_byte(0x22), amount: 0x0102030405060708 },
    ];
    let tx = LeanTx::sign(7, outputs, &sk, &domain);
    println!("chain_id: 1338");
    println!("sk: 0x{}", hex::encode(sk.secret_bytes()));
    println!("sender: {:?}", tx.recover_sender(&domain).unwrap());
    println!("signing_hash: {:?}", tx.signing_hash(&domain));
    println!("encoded: 0x{}", hex::encode(tx.encode()));
}
