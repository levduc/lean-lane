//! One test per SPEC.md §4.2 rule / §4.3 write-set clause, plus §2.4 skips and §2.5 linkage.

use lean_oracle::sign::{address_of, sign_tx, small_secret};
use lean_oracle::*;

const CHAIN: u64 = 1338;
const GWEI: u128 = 1_000_000_000;

fn out(to: Address, amount: u64) -> Output {
    Output { to, amount }
}

fn a(k: u8) -> Address {
    address_of(&small_secret(k))
}

/// Signed canonical bytes for key `k`.
fn tx(k: u8, nonce: u32, outputs: Vec<Output>) -> Vec<u8> {
    sign_tx(&small_secret(k), nonce, outputs, CHAIN).encode()
}

/// State funding keys `ks` with `bal` wei each (via §3.2).
fn funded(ks: &[u8], bal: u128) -> State {
    let file: String = ks
        .iter()
        .map(|k| format!("0x{}\n", hex::encode(a(*k))))
        .collect();
    genesis_state(&file, bal).unwrap()
}

fn block(entries: Vec<Vec<u8>>) -> Vec<u8> {
    encode_block(
        &Header {
            parent: genesis_commitment(CHAIN),
            number: 1,
            timestamp_ms: 0,
        },
        &entries,
    )
}

fn run(state: &mut State, entries: Vec<Vec<u8>>) -> Outcome {
    apply_block(state, &block(entries), CHAIN).unwrap()
}

const R: Address = [0x77; 20];
const R2: Address = [0x78; 20];

// §4.2.2 exact nonce -----------------------------------------------------------------

#[test]
fn exact_nonce_future_nonce_is_noop() {
    let mut s = funded(&[1], 1_000_000 * GWEI * GWEI);
    let o = run(&mut s, vec![tx(1, 1, vec![out(R, 1)])]);
    assert_eq!((o.applied, o.noops), (0, 1));
    assert_eq!(s.get(&a(1)).nonce, 0);
    assert!(!s.contains(&R));
}

#[test]
fn exact_nonce_consecutive_in_one_block_and_replay_noop() {
    let mut s = funded(&[1], 1_000_000 * GWEI * GWEI);
    let o = run(
        &mut s,
        vec![
            tx(1, 0, vec![out(R, 1)]),
            tx(1, 1, vec![out(R, 1)]),
            tx(1, 1, vec![out(R, 1)]), // replay of nonce 1: no-op
            tx(1, 3, vec![out(R, 1)]), // gap: no-op
            tx(1, 2, vec![out(R, 1)]),
        ],
    );
    assert_eq!((o.applied, o.noops, o.outputs), (3, 2, 3));
    assert_eq!(s.get(&a(1)).nonce, 3);
    assert_eq!(s.get(&R).balance, 3 * GWEI);
}

#[test]
fn exact_nonce_order_matters() {
    // nonce 1 before nonce 0: the first is a no-op (§4.2.1 strict entry order).
    let mut s = funded(&[1], GWEI * GWEI);
    let o = run(
        &mut s,
        vec![tx(1, 1, vec![out(R, 1)]), tx(1, 0, vec![out(R, 1)])],
    );
    assert_eq!((o.applied, o.noops), (1, 1));
    assert_eq!(s.get(&a(1)).nonce, 1);
}

// §4.2.3 / §4.2.4 insufficient funds -------------------------------------------------

#[test]
fn insufficient_funds_boundary() {
    let amount = 7u64;
    let total = lean_fee(1) + u128::from(amount) * GWEI;

    // balance == total: applies, sender ends at 0.
    let mut s = funded(&[1], total);
    let o = run(&mut s, vec![tx(1, 0, vec![out(R, amount)])]);
    assert_eq!(o.applied, 1);
    assert_eq!(
        s.get(&a(1)),
        Account {
            nonce: 1,
            balance: 0
        }
    );
    assert_eq!(s.get(&R).balance, u128::from(amount) * GWEI);
    assert_eq!(s.get(&BENEFICIARY).balance, lean_fee(1));

    // balance == total - 1: no-op.
    let mut s = funded(&[1], total - 1);
    let o = run(&mut s, vec![tx(1, 0, vec![out(R, amount)])]);
    assert_eq!((o.applied, o.noops, o.fees_to_beneficiary), (0, 1, 0));
    assert_eq!(
        s.get(&a(1)),
        Account {
            nonce: 0,
            balance: total - 1
        }
    );
    assert!(!s.contains(&R));
}

#[test]
fn fee_alone_must_be_covered() {
    // Zero-amount output still costs lean_fee(1).
    let mut s = funded(&[1], lean_fee(1) - 1);
    let o = run(&mut s, vec![tx(1, 0, vec![out(R, 0)])]);
    assert_eq!(o.noops, 1);
}

#[test]
fn earlier_debit_visible_to_later_tx_of_same_sender() {
    let total = lean_fee(1) + 10 * GWEI;
    let mut s = funded(&[1], total + total / 2);
    let o = run(
        &mut s,
        vec![tx(1, 0, vec![out(R, 10)]), tx(1, 1, vec![out(R, 10)])],
    );
    assert_eq!((o.applied, o.noops), (1, 1));
    assert_eq!(
        s.get(&a(1)),
        Account {
            nonce: 1,
            balance: total / 2
        }
    );
}

// §4.2.5 whole-tx atomicity ------------------------------------------------------------

#[test]
fn atomicity_second_output_unaffordable_means_nothing_happens() {
    // Enough for fee + first output, not for the second.
    let bal = lean_fee(2) + 5 * GWEI;
    let mut s = funded(&[1], bal);
    let o = run(&mut s, vec![tx(1, 0, vec![out(R, 5), out(R2, 1)])]);
    assert_eq!((o.applied, o.noops, o.outputs), (0, 1, 0));
    assert_eq!(
        s.get(&a(1)),
        Account {
            nonce: 0,
            balance: bal
        }
    );
    assert!(!s.contains(&R));
    assert!(!s.contains(&R2));
    assert_eq!(s.get(&BENEFICIARY).balance, 0);
}

// §4.2.6 same-block credits unspendable --------------------------------------------------

#[test]
fn same_block_credit_not_spendable() {
    // k1 funds k2 (unfunded at block start); k2 then tries to spend it in the same block.
    let mut s = funded(&[1], GWEI * GWEI);
    let o = run(
        &mut s,
        vec![
            tx(1, 0, vec![out(a(2), 1_000_000)]),
            tx(2, 0, vec![out(R, 1)]),
        ],
    );
    assert_eq!((o.applied, o.noops), (1, 1));
    // k2 gets the credit, nonce unchanged.
    assert_eq!(
        s.get(&a(2)),
        Account {
            nonce: 0,
            balance: 1_000_000 * GWEI
        }
    );
    assert!(!s.contains(&R));

    // Next block, the credit IS spendable.
    let o = run(&mut s, vec![tx(2, 0, vec![out(R, 1)])]);
    assert_eq!(o.applied, 1);
    assert_eq!(s.get(&R).balance, GWEI);
}

#[test]
fn beneficiary_gets_pre_balance_plus_credits_plus_fees() {
    // §4.2.8: 0x…be has no known key, so "beneficiary as sender" cannot be exercised with
    // signed txs; check that its write = pre + credits + fees (all landing at block end).
    let mut s = funded(&[1], GWEI * GWEI);
    s.set(
        BENEFICIARY,
        Account {
            nonce: 0,
            balance: 5,
        },
    );
    let o = run(
        &mut s,
        vec![
            tx(1, 0, vec![out(BENEFICIARY, 2)]),
            tx(1, 1, vec![out(R, 1)]),
        ],
    );
    assert_eq!(o.applied, 2);
    assert_eq!(s.get(&BENEFICIARY).balance, 5 + 2 * GWEI + lean_fee(1) * 2);
}

// §4.2.7 duplicate recipients / self-transfer --------------------------------------------

#[test]
fn duplicate_recipients_sum_within_and_across_txs() {
    let mut s = funded(&[1, 2], GWEI * GWEI);
    let o = run(
        &mut s,
        vec![
            tx(1, 0, vec![out(R, 1), out(R, 2), out(R2, 4)]),
            tx(2, 0, vec![out(R, 8)]),
        ],
    );
    assert_eq!((o.applied, o.outputs), (2, 4));
    assert_eq!(s.get(&R).balance, 11 * GWEI);
    assert_eq!(s.get(&R2).balance, 4 * GWEI);
    assert_eq!(o.fees_to_beneficiary, lean_fee(3) + lean_fee(1));
    assert_eq!(s.get(&a(1)).balance, GWEI * GWEI - lean_fee(3) - 7 * GWEI);
}

#[test]
fn self_transfer_nets_minus_fee() {
    let bal = GWEI * GWEI;
    let mut s = funded(&[1], bal);
    let o = run(&mut s, vec![tx(1, 0, vec![out(a(1), 1000), out(a(1), 1)])]);
    assert_eq!(o.applied, 1);
    assert_eq!(
        s.get(&a(1)),
        Account {
            nonce: 1,
            balance: bal - lean_fee(2)
        }
    );
}

#[test]
fn self_transfer_needs_full_total_up_front() {
    // Balance covers fee but not the self-sent amount: no-op, even though the net is −fee.
    let bal = lean_fee(1) + GWEI - 1;
    let mut s = funded(&[1], bal);
    let o = run(&mut s, vec![tx(1, 0, vec![out(a(1), 1)])]);
    assert_eq!(o.noops, 1);
    assert_eq!(
        s.get(&a(1)),
        Account {
            nonce: 0,
            balance: bal
        }
    );
}

// §4.2.8 fees ----------------------------------------------------------------------------

#[test]
fn fees_credited_to_beneficiary_only_for_applied() {
    let mut s = funded(&[1, 2], GWEI * GWEI);
    let o = run(
        &mut s,
        vec![
            tx(1, 0, vec![out(R, 1)]),                       // applies, N=1
            tx(2, 0, vec![out(R, 1), out(R, 1), out(R, 1)]), // applies, N=3
            tx(2, 5, vec![out(R, 1), out(R, 1)]),            // no-op (nonce)
        ],
    );
    assert_eq!((o.applied, o.noops), (2, 1));
    assert_eq!(o.fees_to_beneficiary, lean_fee(1) + lean_fee(3));
    assert_eq!(s.get(&BENEFICIARY).balance, lean_fee(1) + lean_fee(3));
    assert_eq!(s.get(&BENEFICIARY).nonce, 0);
}

// §2.4 malformed / skipped entries -------------------------------------------------------

#[test]
fn malformed_and_unrecoverable_entries_are_skipped() {
    let mut s = funded(&[1], GWEI * GWEI);
    let good = tx(1, 0, vec![out(R, 1)]);
    let mut truncated = good.clone();
    truncated.pop();
    let mut bad_n = good.clone();
    bad_n[5..7].copy_from_slice(&0u16.to_le_bytes());
    let mut v27 = good.clone();
    *v27.last_mut().unwrap() = 27;
    let mut r_zero = good.clone();
    r_zero[35..67].fill(0); // r starts at 7 + 28*1 = 35
    let mut eth_type = good.clone();
    eth_type[0] = 0x02;

    let before = state_digest(&s);
    let o = run(
        &mut s,
        vec![
            vec![],
            truncated,
            bad_n,
            v27,
            r_zero,
            eth_type,
            vec![0xff; 200],
        ],
    );
    assert_eq!(
        o,
        Outcome {
            skipped: 7,
            ..Outcome::default()
        }
    );
    // Only the beneficiary was written (as (0,0)); sender untouched, no recipients.
    assert_eq!(
        s.get(&a(1)),
        Account {
            nonce: 0,
            balance: GWEI * GWEI
        }
    );
    assert!(!s.contains(&R));
    assert!(s.contains(&BENEFICIARY));
    assert_ne!(state_digest(&s), before);
    assert_eq!(s.len(), 2);

    // The good tx still applies afterwards (nonce was not consumed).
    let o = run(&mut s, vec![good]);
    assert_eq!(o.applied, 1);
}

#[test]
fn tampered_body_recovers_other_sender_and_is_noop() {
    // Changing an amount changes the signing hash: recovery yields some unrelated
    // (unfunded) address, so the entry is a lean tx whose sender no-ops and gets (0,0).
    let mut s = funded(&[1], GWEI * GWEI);
    let mut b = tx(1, 0, vec![out(R, 1)]);
    b[27] ^= 0x01; // amount_0 low byte
                   // Deterministic keys + RFC 6979: this particular tamper recovers (checked once).
    let item = classify_entry(&b, CHAIN).expect("tampered tx still recovers some key");
    assert_ne!(item.sender, a(1));
    let o = run(&mut s, vec![b]);
    assert_eq!((o.items, o.noops), (1, 1));
    assert_eq!(s.get(&item.sender), Account::default());
    assert!(s.contains(&item.sender));
    assert!(!s.contains(&R));
}

// §4.3 write set ------------------------------------------------------------------------

#[test]
fn unknown_noop_sender_acquires_explicit_zero_entry() {
    let mut s = State::new();
    let o = run(&mut s, vec![tx(5, 0, vec![out(R, 1)])]);
    assert_eq!((o.items, o.noops), (1, 1));
    assert!(s.contains(&a(5)));
    assert_eq!(s.get(&a(5)), Account::default());
    assert!(!s.contains(&R));
}

#[test]
fn zero_amount_and_zero_address_recipients_acquire_entries() {
    let mut s = funded(&[1], GWEI * GWEI);
    let o = run(&mut s, vec![tx(1, 0, vec![out(R, 0), out([0u8; 20], 0)])]);
    assert_eq!(o.applied, 1);
    assert!(s.contains(&R));
    assert!(s.contains(&[0u8; 20]));
    assert_eq!(s.get(&R), Account::default());
}

#[test]
fn execute_write_set_is_exact_and_sorted() {
    let pre = funded(&[1, 2], GWEI * GWEI);
    let entries = [
        tx(1, 0, vec![out(R, 1)]),
        tx(2, 9, vec![out(R2, 1)]), // no-op: k2 written, R2 not
    ];
    let items: Vec<Item> = entries
        .iter()
        .map(|e| classify_entry(e, CHAIN).unwrap())
        .collect();
    let (writes, o) = execute(&pre, &items);
    let mut expected: Vec<Address> = vec![a(1), a(2), R, BENEFICIARY];
    expected.sort();
    let got: Vec<Address> = writes.iter().map(|(a, _)| *a).collect();
    assert_eq!(got, expected);
    assert_eq!((o.applied, o.noops), (1, 1));
    // execute is pure.
    assert_eq!(pre, funded(&[1, 2], GWEI * GWEI));
}

// empty block ---------------------------------------------------------------------------

#[test]
fn empty_block_writes_only_beneficiary() {
    let mut s = funded(&[1], 3);
    let o = run(&mut s, vec![]);
    assert_eq!(o, Outcome::default());
    assert_eq!(s.len(), 2);
    assert_eq!(s.get(&BENEFICIARY), Account::default());
    // Second empty block: no further change.
    let d = state_digest(&s);
    run(&mut s, vec![]);
    assert_eq!(state_digest(&s), d);
}

// §2.1 / §2.5 ---------------------------------------------------------------------------

#[test]
fn ill_framed_block_rejected_state_untouched() {
    let mut s = funded(&[1], GWEI * GWEI);
    let mut wire = block(vec![tx(1, 0, vec![out(R, 1)])]);
    wire.push(0);
    let d = state_digest(&s);
    assert!(matches!(
        apply_block(&mut s, &wire, CHAIN),
        Err(BlockError::TrailingBytes { extra: 1 })
    ));
    assert_eq!(state_digest(&s), d);
}

#[test]
fn chain_linkage() {
    let mut c = Chain::genesis(CHAIN, State::new());
    let wrong_number = encode_block(
        &Header {
            parent: genesis_commitment(CHAIN),
            number: 2,
            timestamp_ms: 0,
        },
        &[],
    );
    assert!(matches!(
        c.append(&wrong_number),
        Err(AppendError::WrongNumber {
            expected: 1,
            got: 2
        })
    ));
    let wrong_parent = encode_block(
        &Header {
            parent: genesis_commitment(1337),
            number: 1,
            timestamp_ms: 0,
        },
        &[],
    );
    assert!(matches!(
        c.append(&wrong_parent),
        Err(AppendError::WrongParent { .. })
    ));
    assert!(c.state.is_empty()); // refused blocks change nothing

    // No timestamp rule: a later block may carry a smaller timestamp.
    let b1 = encode_block(
        &Header {
            parent: genesis_commitment(CHAIN),
            number: 1,
            timestamp_ms: 1000,
        },
        &[],
    );
    c.append(&b1).unwrap();
    let b2 = encode_block(
        &Header {
            parent: c.head.commitment,
            number: 2,
            timestamp_ms: 5,
        },
        &[],
    );
    c.append(&b2).unwrap();
    assert_eq!(c.head.number, 2);
    assert_eq!(c.head.timestamp_ms, 5);
}

#[test]
fn wrong_chain_id_signature_does_not_debit_real_sender() {
    // A tx signed for chain 1337 recovers to some other address on 1338.
    let mut s = funded(&[1], GWEI * GWEI);
    let b = sign_tx(&small_secret(1), 0, vec![out(R, 1)], 1337).encode();
    let o = run(&mut s, vec![b]);
    assert_eq!(o.applied, 0);
    assert_eq!(
        s.get(&a(1)),
        Account {
            nonce: 0,
            balance: GWEI * GWEI
        }
    );
}
