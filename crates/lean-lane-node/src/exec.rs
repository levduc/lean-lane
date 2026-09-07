//! Node-level TOTAL state transition function over the flat state.
//!
//! Same rules as the lean-native crate executor (whole-tx atomic, same-block
//! incoming credits NOT spendable, duplicate recipients sum, one deferred
//! beneficiary credit at end of block), with ONE deliberate difference: an
//! invalid tx at execution time is a NO-OP, never a block failure. The pool
//! keeps no-ops rare (nonce/funds validated at admission against the
//! post-latest-block state); a byzantine proposer packing invalid txs wastes
//! bytes, never halts the lane. This is the "total STF" the deferred-exec
//! design wants under vote-on-hash.

use crate::state::FlatState;
use alloy_primitives::{Address, B256};
use lean_native::{lean_fee, LeanTx, TransferLog, AMOUNT_UNIT};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeanReceipt {
    pub tx_hash: B256,
    pub block_number: u64,
    pub index: u32,
    pub sender: Address,
    /// false = no-op (invalid at execution; state untouched).
    pub applied: bool,
    pub logs: Vec<TransferLog>,
}

pub struct BlockOutcome {
    pub receipts: Vec<LeanReceipt>,
    pub applied: usize,
    pub noops: usize,
    pub outputs: usize,
    pub fees_to_beneficiary: u128,
}

/// Apply a block of recovered lean txs to `state`. `items` are in block order.
pub fn apply_block(
    state: &mut FlatState,
    items: &[(Address, LeanTx, B256)],
    block_number: u64,
    beneficiary: Address,
) -> BlockOutcome {
    // Debit view: block-start balances, own debits accumulate, incoming
    // same-block credits invisible (deterministic-parallel rule).
    let mut debit_view: HashMap<Address, crate::state::Acct> = HashMap::new();
    let mut credits: HashMap<Address, u128> = HashMap::new();
    let mut receipts = Vec::with_capacity(items.len());
    let mut fees = 0u128;
    let mut applied = 0usize;
    let mut outputs = 0usize;

    for (i, (sender, tx, hash)) in items.iter().enumerate() {
        let entry = debit_view.entry(*sender).or_insert_with(|| state.get(sender));
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
        if ok && entry.balance < total {
            ok = false;
        }
        if !ok {
            receipts.push(LeanReceipt {
                tx_hash: *hash,
                block_number,
                index: i as u32,
                sender: *sender,
                applied: false,
                logs: Vec::new(),
            });
            continue;
        }
        entry.balance -= total;
        entry.nonce += 1;
        let mut logs = Vec::with_capacity(tx.outputs.len());
        for o in &tx.outputs {
            let wei = o.amount as u128 * AMOUNT_UNIT;
            *credits.entry(o.to).or_default() += wei;
            if o.amount != 0 {
                logs.push(TransferLog { from: *sender, to: o.to, amount: wei });
            }
        }
        outputs += tx.outputs.len();
        fees += fee;
        applied += 1;
        receipts.push(LeanReceipt {
            tx_hash: *hash,
            block_number,
            index: i as u32,
            sender: *sender,
            applied: true,
            logs,
        });
    }

    for (addr, acct) in debit_view {
        state.accounts.insert(addr, acct);
    }
    for (addr, wei) in credits {
        state.credit(addr, wei);
    }
    state.credit(beneficiary, fees);

    BlockOutcome { receipts, applied, noops: items.len() - applied, outputs, fees_to_beneficiary: fees }
}
