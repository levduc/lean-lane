//! `lean-oracle` — an independent reference implementation of the lean payment lane,
//! written from `SPEC.md` (repository root) and nothing else.
//!
//! Design goals, in order: obviously correct, literal to the spec, simple. Not fast.
//! Every rule cites the SPEC.md section it implements. Where the spec is silent or
//! ambiguous the choice made is recorded in `SPEC-QUESTIONS.md` next to this crate.
//!
//! Module map:
//! - [`primitives`] — §0 conventions, keccak, addresses, constants of §1.1 / §3.3.
//! - [`tx`]         — §1 the fan-out transaction: decode, signing hash, recovery, hash, fee.
//! - [`block`]      — §2 block wire form, framing, `txs_hash`, commitment, genesis, §2.4 items.
//! - [`state`]      — §3 flat state, genesis funding, digest.
//! - [`stf`]        — §4 the state transition function, `apply_block`, and a §2.5 [`stf::Chain`].
//! - [`sign`]       — test/differential-harness support: produce signed txs (not normative).

#![forbid(unsafe_code)]

pub mod block;
pub mod primitives;
pub mod sign;
pub mod state;
pub mod stf;
pub mod tx;

pub use block::{
    commitment, decode_block, encode_block, genesis_commitment, txs_hash, Block, BlockError, Header,
};
pub use primitives::{keccak256, Address, Hash, BENEFICIARY};
pub use state::{genesis_state, state_digest, Account, FundFileError, State};
pub use stf::{apply_block, classify_entry, execute, AppendError, Chain, Head, Item, Outcome};
pub use tx::{
    decode_tx, decode_tx_prefix, lane_domain, lean_fee, lean_gas, recover_sender, signing_hash,
    tx_hash, Malformed, Output, Tx, Unrecoverable,
};
