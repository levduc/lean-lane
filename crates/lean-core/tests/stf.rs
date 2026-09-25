//! The total state transition function (moved from the node's `exec.rs`):
//! whole-tx atomicity, duplicate recipients, unspendable same-block credits,
//! purity and the exact write set.
use alloy_primitives::{Address, B256};
use lean_core::{execute, Acct, EngineCtx, FlatState, RecoveredTx};
use lean_native::{lane_domain, lean_fee, LeanTx, Output, AMOUNT_UNIT};
use secp256k1::SecretKey;

fn ctx(beneficiary: Address) -> EngineCtx {
    EngineCtx::new(1338, beneficiary)
}

fn apply_block(
    state: &mut FlatState,
    items: &[RecoveredTx],
    ben: Address,
) -> lean_core::BlockOutcome {
    lean_core::apply_block(state, items, &ctx(ben))
}

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

/// Sign `outputs` from key `i` at `nonce`; returns the recovered item.
fn item(i: u64, nonce: u32, outputs: Vec<Output>) -> RecoveredTx {
    let d = lane_domain(1338);
    let tx = LeanTx::sign(nonce, outputs, &sk(i), &d);
    (tx.recover_sender(&d).unwrap(), tx, B256::ZERO)
}

fn funded(pairs: &[(Address, u128)]) -> FlatState {
    let mut s = FlatState::default();
    for (a, balance) in pairs {
        s.accounts.insert(
            *a,
            Acct {
                nonce: 0,
                balance: *balance,
            },
        );
    }
    s
}

fn to(i: u8, amount: u64) -> Output {
    Output {
        to: Address::with_last_byte(i),
        amount,
    }
}

#[test]
fn fan_out_applies_whole_and_duplicate_recipients_sum() {
    let ben = Address::with_last_byte(0xbe);
    let it = item(1, 0, vec![to(1, 400_000), to(2, 0), to(1, 600_000)]);
    let mut state = funded(&[(it.0, 1_000_000 * AMOUNT_UNIT + lean_fee(3))]);
    let out = apply_block(&mut state, std::slice::from_ref(&it), ben);
    assert_eq!((out.applied, out.noops, out.outputs), (1, 0, 3));
    assert_eq!(
        state.get(&Address::with_last_byte(1)).balance,
        1_000_000 * AMOUNT_UNIT
    );
    assert_eq!(state.get(&Address::with_last_byte(2)).balance, 0);
    assert_eq!(
        state.get(&it.0),
        Acct {
            nonce: 1,
            balance: 0
        }
    );
    assert_eq!(state.get(&ben).balance, lean_fee(3));
}

#[test]
fn insufficient_funds_is_a_whole_tx_no_op() {
    // can afford the first output alone, not both
    let it = item(2, 0, vec![to(1, 50), to(2, 50)]);
    let start = lean_fee(2) + 99;
    let mut state = funded(&[(it.0, start)]);
    let out = apply_block(&mut state, std::slice::from_ref(&it), Address::ZERO);
    assert_eq!((out.applied, out.noops, out.fees_to_beneficiary), (0, 1, 0));
    assert_eq!(
        state.get(&it.0),
        Acct {
            nonce: 0,
            balance: start
        }
    );
    assert!(!state.accounts.contains_key(&Address::with_last_byte(1)));
    assert!(!state.accounts.contains_key(&Address::with_last_byte(2)));
}

#[test]
fn same_block_credit_is_not_spendable() {
    let b = item(4, 0, vec![]).0;
    let t1 = item(
        3,
        0,
        vec![Output {
            to: b,
            amount: 1_000,
        }],
    );
    let t2 = item(4, 0, vec![to(9, 500)]);
    // b can pay its fee, but not forward a's same-block credit
    let mut state = funded(&[(t1.0, 1_000 * AMOUNT_UNIT + lean_fee(1)), (b, lean_fee(1))]);
    let out = apply_block(&mut state, &[t1, t2], Address::ZERO);
    assert_eq!((out.applied, out.noops), (1, 1), "t2 is a no-op");
    assert_eq!(
        state.get(&b),
        Acct {
            nonce: 0,
            balance: lean_fee(1) + 1_000 * AMOUNT_UNIT
        }
    );
    assert_eq!(state.get(&Address::with_last_byte(9)).balance, 0);
}

#[test]
fn execute_is_pure_and_writes_only_the_stf_write_set() {
    let ben = Address::with_last_byte(0xbe);
    let ok = item(5, 0, vec![to(1, 10)]);
    let bankrupt = item(6, 0, vec![to(2, 10)]); // unfunded: a no-op
    let state = funded(&[(ok.0, 10 * AMOUNT_UNIT + lean_fee(1))]);
    let before = state.digest();
    let d = execute(&state, &[ok.clone(), bankrupt.clone()], &ctx(ben));
    assert_eq!(state.digest(), before, "execute must not mutate");
    let written: Vec<Address> = d.writes.iter().map(|(a, _)| *a).collect();
    let mut want = vec![ok.0, bankrupt.0, Address::with_last_byte(1), ben];
    want.sort_unstable();
    assert_eq!(written, want, "a no-op's recipient is never written");
    assert_eq!((d.txs(), d.evict.len()), (2, 2));
}
