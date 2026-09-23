//! Standalone lean payment-lane node: reth as a MEMPOOL LIBRARY, not an
//! execution client. Kept: the pool's validator machinery (sig recovery,
//! nonce/funds checks, replacement, eviction), unmodified. Dropped: MPT state root, receipts root,
//! bloom, the whole Ethereum header — the lane is BFT-final; the CL's signed
//! block commitment (see `chain.rs`) is the source of truth for ordering, and
//! state is a pure function of it.

pub mod blockreader;
pub mod chain;
pub mod exec;
pub mod metrics;
pub mod node;
pub mod provider;
pub mod rpc;
pub mod state;
