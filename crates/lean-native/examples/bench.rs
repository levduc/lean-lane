//! Offline bench: per-OUTPUT cost of decode + ecrecover + execute for the lean
//! fan-out transfer, serial vs parallel, at N = 1 / 10 / 100 outputs per tx.
//! Run: cargo run --release -p lean-native --example bench

use alloy_primitives::Address;
use lean_native::*;
use secp256k1::SecretKey;
use std::time::Instant;

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

fn main() {
    let domain = lane_domain(1338);
    let beneficiary = Address::with_last_byte(0xbe);
    const TOTAL_OUTPUTS: usize = 100_000;
    println!("lean-native bench — {} total outputs per config, {} rayon threads", TOTAL_OUTPUTS, rayon::current_num_threads());
    println!("baseline: type-2 EIP-1559 transfer = 122 B/output, ecrecover per output");
    println!();
    println!("{:>4} {:>7} {:>9} | {:>9} {:>9} {:>9} | {:>9} {:>9} {:>9} | {:>8}",
        "N", "txs", "B/out",
        "dec us/o", "recS us/o", "excS us/o",
        "recP us/o", "excP us/o", "totP us/o", "pack x");
    for n in [1usize, 10, 100] {
        let n_txs = TOTAL_OUTPUTS / n;
        // sign corpus (one tx per sender; hot+spread recipient mix)
        let txs: Vec<LeanTx> = (0..n_txs as u64)
            .map(|k| {
                let outs: Vec<Output> = (0..n)
                    .map(|i| Output {
                        to: Address::with_last_byte(((k as usize * 17 + i * 3) % 251) as u8),
                        amount: 1 + (i as u64),
                    })
                    .collect();
                LeanTx::sign(0, outs, &sk(k), &domain)
            })
            .collect();
        let encoded: Vec<Vec<u8>> = txs.iter().map(|t| t.encode()).collect();
        let bytes_per_output = encoded[0].len() as f64 / n as f64;

        // decode
        let t0 = Instant::now();
        let decoded: Vec<LeanTx> = encoded.iter().map(|b| LeanTx::decode(b).unwrap()).collect();
        let dec = t0.elapsed().as_secs_f64();

        // recover serial / parallel
        let t0 = Instant::now();
        let rec_s = recover_all_serial(&decoded, &domain).unwrap();
        let recs = t0.elapsed().as_secs_f64();
        let t0 = Instant::now();
        let rec_p = recover_all_parallel(&decoded, &domain).unwrap();
        let recp = t0.elapsed().as_secs_f64();

        // pre-state: fund every sender
        let mut base = State::new();
        for r in &rec_s {
            base.insert(r.sender, Account { nonce: 0, balance: u128::MAX / 2 });
        }

        // execute serial / parallel
        let mut s1 = base.clone();
        let t0 = Instant::now();
        let o1 = execute_block_serial(&mut s1, &rec_s, beneficiary).unwrap();
        let exs = t0.elapsed().as_secs_f64();
        let mut s2 = base.clone();
        let t0 = Instant::now();
        let o2 = execute_block_parallel(&mut s2, &rec_p, beneficiary).unwrap();
        let exp = t0.elapsed().as_secs_f64();
        assert_eq!(o1, o2);
        assert_eq!(s1, s2);

        let per = |secs: f64| secs * 1e6 / TOTAL_OUTPUTS as f64;
        println!(
            "{:>4} {:>7} {:>9.1} | {:>9.3} {:>9.2} {:>9.3} | {:>9.2} {:>9.3} {:>9.2} | {:>8.2}",
            n, n_txs, bytes_per_output,
            per(dec), per(recs), per(exs),
            per(recp), per(exp), per(dec + recp + exp),
            122.0 / bytes_per_output,
        );
    }
    println!();
    println!("cols: dec=decode, recS/recP=ecrecover serial/parallel, excS/excP=execute serial/parallel,");
    println!("      totP=decode+parallel recover+parallel execute, pack x = 122B / (B/out).");
}
