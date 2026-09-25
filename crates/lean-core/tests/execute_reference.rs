//! `execute` against a frozen copy of the pre-optimization reference STF
//! (the original two-HashMap implementation, SPEC §4.1 line by line), on
//! LARGE randomized blocks — the gate's differential test uses small blocks,
//! this one exercises the parallel (sort/merge, rayon) paths at block sizes
//! the bench runs, with heavy sender and recipient collisions, nonce gaps and
//! duplicates, insufficient funds, the beneficiary as sender and recipient,
//! unknown senders and zero amounts. Items are synthetic (no signatures —
//! `execute` takes recovered items).
use alloy_primitives::{Address, B256};
use lean_core::{execute, Acct, BlockOutcome, Delta, EngineCtx, FlatState, RecoveredTx, StateView};
use lean_native::{lean_fee, LeanTx, Output, AMOUNT_UNIT, SIG_LEN};
use std::collections::HashMap;

/// The original engine::execute, verbatim in logic.
fn reference(state: &FlatState, items: &[RecoveredTx], beneficiary: Address) -> Delta {
    let mut post: HashMap<Address, Acct> = HashMap::new();
    let mut credits: HashMap<Address, u128> = HashMap::new();
    let mut fees = 0u128;
    let mut applied = 0usize;
    let mut outputs = 0usize;
    for (sender, tx, _) in items {
        let entry = post.entry(*sender).or_insert_with(|| state.get(sender));
        let fee = lean_fee(tx.outputs.len());
        let mut total = fee;
        let mut ok = tx.nonce as u64 == entry.nonce;
        if ok {
            for o in &tx.outputs {
                match total.checked_add(o.amount as u128 * AMOUNT_UNIT) {
                    Some(t) => total = t,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
        }
        if !ok || entry.balance < total {
            continue;
        }
        entry.balance -= total;
        entry.nonce += 1;
        for o in &tx.outputs {
            *credits.entry(o.to).or_default() += o.amount as u128 * AMOUNT_UNIT;
        }
        outputs += tx.outputs.len();
        fees += fee;
        applied += 1;
    }
    for (addr, wei) in credits {
        post.entry(addr).or_insert_with(|| state.get(&addr)).balance += wei;
    }
    post.entry(beneficiary)
        .or_insert_with(|| StateView::get(state, &beneficiary))
        .balance += fees;
    let mut writes: Vec<(Address, Acct)> = post.into_iter().collect();
    writes.sort_unstable_by_key(|(a, _)| *a);
    Delta {
        writes,
        evict: items.iter().map(|(_, _, h)| *h).collect(),
        outcome: BlockOutcome {
            applied,
            noops: items.len() - applied,
            outputs,
            fees_to_beneficiary: fees,
        },
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn addr(r: &mut Rng, pool: u64) -> Address {
    // a small pool (heavy collisions) with addresses spread over the whole
    // byte range, plus near-duplicates differing only in the last bytes
    let i = r.below(pool);
    let mut b = [0u8; 20];
    b[0] = (i % 7) as u8 * 37;
    b[12..20].copy_from_slice(&i.to_be_bytes());
    if i % 5 == 0 {
        b[19] ^= 0x01;
    }
    Address::new(b)
}

fn case(seed: u64, n_items: usize, pool: u64, max_n: u64, parallel: bool) {
    let mut r = Rng(seed);
    let ben = if seed % 3 == 0 {
        addr(&mut r, pool)
    } else {
        Address::with_last_byte(0xbe)
    };
    let mut state = FlatState::default();
    for _ in 0..pool / 2 {
        let a = addr(&mut r, pool);
        let bal = match r.below(4) {
            0 => 0,
            1 => r.below(1_000) as u128 * AMOUNT_UNIT,
            _ => r.below(1 << 40) as u128 * AMOUNT_UNIT * 1000,
        };
        state.accounts.insert(
            a,
            Acct {
                nonce: r.below(3),
                balance: bal,
            },
        );
    }
    let mut nonces: HashMap<Address, u64> = HashMap::new();
    let items: Vec<RecoveredTx> = (0..n_items)
        .map(|j| {
            let sender = if r.below(10) == 0 {
                ben
            } else {
                addr(&mut r, pool)
            };
            let exp = *nonces
                .entry(sender)
                .or_insert_with(|| state.get(&sender).nonce);
            let nonce = match r.below(8) {
                0 => exp + 1,
                1 => exp.saturating_sub(1),
                _ => exp,
            };
            if nonce == exp {
                nonces.insert(sender, exp + 1);
            }
            let n = 1 + r.below(max_n) as usize;
            let outputs = (0..n)
                .map(|_| Output {
                    to: match r.below(20) {
                        0 => ben,
                        1 => sender,
                        _ => addr(&mut r, pool * 4),
                    },
                    amount: match r.below(6) {
                        0 => 0,
                        1 => u64::MAX,
                        _ => r.below(1 << 30),
                    },
                })
                .collect();
            let mut h = [0u8; 32];
            h[..8].copy_from_slice(&(j as u64).to_le_bytes());
            h[8..16].copy_from_slice(&seed.to_le_bytes());
            (
                sender,
                LeanTx {
                    nonce: nonce as u32,
                    outputs,
                    sig: [0u8; SIG_LEN],
                },
                B256::new(h),
            )
        })
        .collect();
    let ctx = EngineCtx::new(1338, ben).with_parallel_recovery(parallel);
    let got = execute(&state, &items, &ctx);
    let want = reference(&state, &items, ben);
    assert_eq!(
        got.outcome, want.outcome,
        "outcome seed={seed} n={n_items} par={parallel}"
    );
    assert_eq!(got.evict, want.evict, "evict seed={seed}");
    assert_eq!(
        got.writes.len(),
        want.writes.len(),
        "write count seed={seed} n={n_items}"
    );
    assert_eq!(
        got.writes, want.writes,
        "writes seed={seed} n={n_items} par={parallel}"
    );
}

#[test]
fn execute_matches_reference_on_large_random_blocks() {
    let mut seed = 1;
    for &(n_items, pool, max_n) in &[
        (0usize, 10u64, 1u64),
        (1, 10, 1),
        (7, 3, 3),
        (200, 20, 5),
        (3_000, 500, 3),
        (5_000, 50_000, 1),
        (1_000, 2_000, 100),
        (20_000, 5_000, 2),
    ] {
        for _ in 0..3 {
            for parallel in [true, false] {
                case(seed, n_items, pool, max_n, parallel);
            }
            seed += 1;
        }
    }
}
