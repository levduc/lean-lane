//! SPEC.md §4 — the state transition function; §2.4 entry classification; §2.5 linkage.

use std::collections::BTreeMap;

use crate::block::{decode_block, genesis_commitment, BlockError};
use crate::primitives::{Address, Hash, BENEFICIARY, LEAN_TX_TYPE};
use crate::state::{Account, State};
use crate::tx::{decode_tx_prefix, recover_sender, tx_hash, Tx};

/// §2.4 a lean-tx item `(sender, tx, tx_hash)`; `tx_hash` is over the canonical prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub sender: Address,
    pub tx: Tx,
    pub tx_hash: Hash,
}

/// §4.1 `outcome`, plus two informational §2.4 counters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// §4.1 `applied`.
    pub applied: u64,
    /// §4.1 `noops = len(items) - applied`.
    pub noops: u64,
    /// §4.1 `outputs`: total outputs of applied txs.
    pub outputs: u64,
    /// §4.1 `fees_to_beneficiary` (wei).
    pub fees_to_beneficiary: u128,
    /// Informational: `len(items)` (= applied + noops).
    pub items: u64,
    /// Informational: entries classified as skipped by §2.4 (not part of §4.1's outcome;
    /// skipped entries are "not counted (neither applied nor no-op)").
    pub skipped: u64,
}

/// §2.4: classify one entry. `Some(item)` iff it is a lean tx: `entry[0] == 0x50`,
/// prefix decode (§1.3) succeeds and sender recovery (§1.5) succeeds. `None` = skipped.
pub fn classify_entry(entry: &[u8], chain_id: u64) -> Option<Item> {
    // Empty entry or any other first byte (incl. valid Ethereum txs): skipped.
    if entry.first() != Some(&LEAN_TX_TYPE) {
        return None;
    }
    // A 0x50 entry failing §1.3 steps 1–4: skipped. Trailing bytes are ignored.
    let tx = decode_tx_prefix(entry).ok()?;
    // Unrecoverable signature: skipped.
    let sender = recover_sender(&tx, chain_id).ok()?;
    // tx_hash over the canonical prefix only (§2.4, §1.6, Appendix B.8).
    let tx_hash = tx_hash(&tx.encode());
    Some(Item {
        sender,
        tx,
        tx_hash,
    })
}

/// §2.4: `items` = the lean-tx items in entry order; also returns the skipped count.
pub fn classify_entries(entries: &[Vec<u8>], chain_id: u64) -> (Vec<Item>, u64) {
    let mut items = Vec::new();
    let mut skipped = 0u64;
    for e in entries {
        match classify_entry(e, chain_id) {
            Some(item) => items.push(item),
            None => skipped += 1,
        }
    }
    (items, skipped)
}

/// §4.4: balance `+=`. The spec says release wraps / debug panics and that conforming
/// implementations SHOULD treat such inputs as out of domain; the oracle refuses loudly.
fn add_balance(acct: &mut Account, w: u128) {
    acct.balance = acct
        .balance
        .checked_add(w)
        .expect("§4.4: balance overflow — input out of domain");
}

/// §4.1 `execute(state, items, BENEFICIARY) → writes` (pure). Returns the write set
/// (sorted by address) and the outcome. `outcome.skipped` is left 0 here.
pub fn execute(state: &State, items: &[Item]) -> (Vec<(Address, Account)>, Outcome) {
    // post: accounts touched this block. credits: deferred recipient credits.
    let mut post: BTreeMap<Address, Account> = BTreeMap::new();
    let mut credits: BTreeMap<Address, u128> = BTreeMap::new();
    let mut fees: u128 = 0;
    let mut applied: u64 = 0;
    let mut outputs: u64 = 0;

    // §4.2.1: strictly in entry order.
    for item in items {
        // §4.3: the sender is written EVEN IF the tx is a no-op.
        let e = post
            .entry(item.sender)
            .or_insert_with(|| state.get(&item.sender));
        let fee = item.tx.fee();
        // §4.2.2 exact nonce: u32 wire nonce compared as u64 against the in-block nonce.
        let mut ok = u64::from(item.tx.nonce) == e.nonce;
        // §4.2.3 total = fee + Σ amount_i·1e9, checked.
        let mut total = fee;
        if ok {
            for o in &item.tx.outputs {
                match total.checked_add(o.wei()) {
                    Some(t) => total = t,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
        }
        // §4.2.4 insufficient funds (equality applies). §4.2.5 no-op: nothing else happens.
        if !ok || e.balance < total {
            continue;
        }
        // Debit is visible to later txs of this sender (§4.2.6).
        e.balance -= total;
        // e.nonce == tx.nonce <= u32::MAX here, so +1 cannot overflow u64.
        e.nonce += 1;
        // §4.2.6/7: credits are deferred to the end of the block; duplicates sum.
        for o in &item.tx.outputs {
            let c = credits.entry(o.to).or_insert(0);
            *c = c
                .checked_add(o.wei())
                .expect("§4.4: credit overflow — input out of domain");
        }
        outputs += item.tx.outputs.len() as u64;
        fees += fee;
        applied += 1;
    }

    // Credits land after all txs (§4.2.6). Each applied recipient is written (§4.3).
    for (a, w) in &credits {
        let acct = post.entry(*a).or_insert_with(|| state.get(a));
        add_balance(acct, *w);
    }
    // §4.2.8: fees credited to BENEFICIARY once, at end of block — ALWAYS, even if 0.
    let ben = post
        .entry(BENEFICIARY)
        .or_insert_with(|| state.get(&BENEFICIARY));
    add_balance(ben, fees);

    // writes := post sorted by address (BTreeMap iteration order).
    let writes: Vec<(Address, Account)> = post.into_iter().collect();
    let n_items = items.len() as u64;
    let outcome = Outcome {
        applied,
        noops: n_items - applied,
        outputs,
        fees_to_beneficiary: fees,
        items: n_items,
        skipped: 0,
    };
    (writes, outcome)
}

/// §4.1 `apply`: `state[a] = acct` for every write.
pub fn apply_writes(state: &mut State, writes: &[(Address, Account)]) {
    for (a, acct) in writes {
        state.set(*a, *acct);
    }
}

/// Full STF for one wire block: §2.1 framing, §2.4 classification, §4.1 execute + apply.
///
/// Does NOT check §2.5 chain linkage (the state carries no head); use [`Chain::append`]
/// for that. A block that is not well-framed is rejected as a whole and leaves `state`
/// untouched (§2.1).
pub fn apply_block(
    state: &mut State,
    block_bytes: &[u8],
    chain_id: u64,
) -> Result<Outcome, BlockError> {
    let block = decode_block(block_bytes)?;
    let (items, skipped) = classify_entries(&block.entries, chain_id);
    let (writes, mut outcome) = execute(state, &items);
    apply_writes(state, &writes);
    outcome.skipped = skipped;
    Ok(outcome)
}

/// §2.3 / §2.5 chain head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub commitment: Hash,
    pub number: u64,
    pub timestamp_ms: u64,
}

/// Why [`Chain::append`] refused a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppendError {
    /// §2.1: not well-framed.
    Framing(BlockError),
    /// §2.5: `number != head.number + 1`.
    WrongNumber { expected: u64, got: u64 },
    /// §2.5: `parent != head.commitment`.
    WrongParent { expected: Hash, got: Hash },
}

/// §5 canonical chain state: head + state, advanced one block at a time.
#[derive(Clone, Debug)]
pub struct Chain {
    pub chain_id: u64,
    pub head: Head,
    pub state: State,
}

impl Chain {
    /// §2.3: genesis head `(GENESIS_COMMITMENT(chain_id), 0, 0)` over a §3.2 genesis state.
    pub fn genesis(chain_id: u64, state: State) -> Self {
        Chain {
            chain_id,
            head: Head {
                commitment: genesis_commitment(chain_id),
                number: 0,
                timestamp_ms: 0,
            },
            state,
        }
    }

    /// §2.5: append iff `number == head.number + 1` and `parent == head.commitment`; no
    /// timestamp rule, no size/gas limit. On success the head moves to the block's
    /// recomputed commitment (§2.2) and the STF is applied.
    pub fn append(&mut self, block_bytes: &[u8]) -> Result<Outcome, AppendError> {
        let block = decode_block(block_bytes).map_err(AppendError::Framing)?;
        let expected = self.head.number.checked_add(1).expect("height overflow");
        if block.header.number != expected {
            return Err(AppendError::WrongNumber {
                expected,
                got: block.header.number,
            });
        }
        if block.header.parent != self.head.commitment {
            return Err(AppendError::WrongParent {
                expected: self.head.commitment,
                got: block.header.parent,
            });
        }
        let (items, skipped) = classify_entries(&block.entries, self.chain_id);
        let (writes, mut outcome) = execute(&self.state, &items);
        apply_writes(&mut self.state, &writes);
        outcome.skipped = skipped;
        self.head = Head {
            commitment: block.commitment(),
            number: block.header.number,
            timestamp_ms: block.header.timestamp_ms,
        };
        Ok(outcome)
    }
}
