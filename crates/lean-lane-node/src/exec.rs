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

use crate::state::Acct;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockOutcome {
    pub applied: usize,
    pub noops: usize,
    pub outputs: usize,
    pub fees_to_beneficiary: u128,
}

/// What executing one block does to the state, computed WITHOUT mutating it:
/// the post-value of every account the block writes, plus what the node needs
/// to finish the append (pool evictions, counters).
///
/// The write set is exactly the total STF's: every sender (a no-op's too — its
/// entry is created at the value it already had), every recipient of an
/// APPLIED tx, and the beneficiary. The outputs of a no-op tx are never
/// written. `FlatState::digest` covers the account count, so this set must be
/// the same on every path that executes a block — and it is, because `execute`
/// is the only code that computes it.
#[derive(Clone, Debug, Default)]
pub struct Delta {
    /// (address, post-state), sorted by address.
    pub writes: Vec<(Address, Acct)>,
    /// hashes of every tx in the block (applied or no-op): pool evictions
    pub evict: Vec<B256>,
    pub outcome: BlockOutcome,
}

impl Delta {
    /// Write the delta into `state`.
    pub fn apply_to(&self, state: &mut FlatState) {
        for (a, acct) in &self.writes {
            state.accounts.insert(*a, *acct);
        }
    }

    /// Number of txs in the block (applied + no-op).
    pub fn txs(&self) -> usize {
        self.outcome.applied + self.outcome.noops
    }
}

/// THE state transition function: execute a block of recovered lean txs
/// (`items`, in block order) against `state` and return the delta. Pure —
/// `state` is only read, so a staging can execute against the committed
/// state without copying it.
pub fn execute(
    state: &FlatState,
    items: &[(Address, LeanTx, B256)],
    beneficiary: Address,
) -> Delta {
    // Post view: block-start values, own debits accumulate, incoming
    // same-block credits invisible to the debit checks (deterministic-parallel
    // rule) — they are only merged in after the debit phase.
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
        .or_insert_with(|| state.get(&beneficiary))
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

/// `execute` + write the delta into `state` (replay and tests).
pub fn apply_block(
    state: &mut FlatState,
    items: &[(Address, LeanTx, B256)],
    beneficiary: Address,
) -> BlockOutcome {
    let delta = execute(state, items, beneficiary);
    delta.apply_to(state);
    delta.outcome
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn execute_is_pure_and_writes_only_the_stf_write_set() {
        let ben = Address::with_last_byte(0xbe);
        let ok = item(5, 0, vec![to(1, 10)]);
        let bankrupt = item(6, 0, vec![to(2, 10)]); // unfunded: a no-op
        let state = funded(&[(ok.0, 10 * AMOUNT_UNIT + lean_fee(1))]);
        let before = state.digest();
        let d = execute(&state, &[ok.clone(), bankrupt.clone()], ben);
        assert_eq!(state.digest(), before, "execute must not mutate");
        let written: Vec<Address> = d.writes.iter().map(|(a, _)| *a).collect();
        let mut want = vec![ok.0, bankrupt.0, Address::with_last_byte(1), ben];
        want.sort_unstable();
        assert_eq!(written, want, "a no-op's recipient is never written");
        assert_eq!((d.txs(), d.evict.len()), (2, 2));
    }
}
