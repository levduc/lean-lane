//! Standalone lean payment-lane node (increment 2b'): reth as a MEMPOOL
//! LIBRARY, not an execution client. Kept: the pool's validator machinery
//! (sig recovery, nonce/funds checks, replacement, eviction) exactly as
//! increment 2a wired it, unmodified. Dropped: MPT state root, receipts root,
//! bloom, the whole Ethereum header — the lane is BFT-final; the CL's signed
//! block commitment (see `chain.rs`) is the source of truth for ordering, and
//! state is a pure function of it.

pub mod chain;
pub mod exec;
pub mod node;
pub mod blockreader;
pub mod provider;
pub mod rpc;
pub mod state;
