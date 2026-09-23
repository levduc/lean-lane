//! Node-level TOTAL state transition function over the flat state.
//!
//! Rules: whole-tx atomic, same-block incoming credits NOT spendable,
//! duplicate recipients sum, one deferred beneficiary credit at end of block.
//! An invalid tx at execution time (wrong nonce, output-sum overflow,
//! insufficient funds) is a NO-OP, never a block failure. The pool keeps
//! no-ops rare (nonce/funds validated at admission against the
//! post-latest-block state); a byzantine proposer packing invalid txs wastes
//! bytes, never halts the lane.

use crate::state::FlatState;
use alloy_primitives::{Address, B256};
use lean_native::{lean_fee, LeanTx, AMOUNT_UNIT};
use std::collections::HashMap;

pub struct BlockOutcome {
    pub applied: usize,
    pub noops: usize,
    pub outputs: usize,
    pub fees_to_beneficiary: u128,
}

/// Apply a block of recovered lean txs to `state`. `items` are in block order.
pub fn apply_block(
    state: &mut FlatState,
    items: &[(Address, LeanTx, B256)],
    beneficiary: Address,
) -> BlockOutcome {
    // Debit view: block-start balances, own debits accumulate, incoming
    // same-block credits invisible (deterministic-parallel rule).
    let mut debit_view: HashMap<Address, crate::state::Acct> = HashMap::new();
    let mut credits: HashMap<Address, u128> = HashMap::new();
    let mut fees = 0u128;
    let mut applied = 0usize;
    let mut outputs = 0usize;

    for (sender, tx, _) in items {
        let entry = debit_view
            .entry(*sender)
            .or_insert_with(|| state.get(sender));
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

    for (addr, acct) in debit_view {
        state.accounts.insert(addr, acct);
    }
    for (addr, wei) in credits {
        state.credit(addr, wei);
    }
    state.credit(beneficiary, fees);

    BlockOutcome {
        applied,
        noops: items.len() - applied,
        outputs,
        fees_to_beneficiary: fees,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Acct;
    use lean_native::{lane_domain, Output};
    use secp256k1::SecretKey;

    fn sk(i: u64) -> SecretKey {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&(i + 1).to_be_bytes());
        b[31] = 1;
        SecretKey::from_slice(&b).unwrap()
    }

    /// Sign `outputs` from key `i` at `nonce`; returns the recovered item.
    fn item(i: u64, nonce: u32, outputs: Vec<Output>) -> (Address, LeanTx, B256) {
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
}
