//! `lean-diff` — the correctness safety net for optimizing `lean-core`.
//!
//! Four independent checks, each a library function returning a summary or a
//! printable failure, so the same code runs from `cargo test` (quick profile)
//! and from the `lean-diff` binary that `scripts/engine-gate.sh` drives
//! (release, large fixed budgets):
//!
//! * [`diff`] — differential property test, engine (`lean_core`, parallel AND
//!   serial recovery) vs the independent oracle (`lean_oracle`, written from
//!   SPEC.md only), over seeded adversarial chains from a funded genesis
//!   ([`gen`]). Every block compares framing verdict, header, commitment, the
//!   four §4.1 counters, the full sorted write set, the item hash list and
//!   the post-state digest. A mismatch is shrunk by delta debugging over the
//!   block's entry list and printed as a reproducer.
//! * [`restart`] — engine applied straight through vs the node's replay path
//!   vs a mid-chain snapshot (the node's own snapshot writer/reader) + tail:
//!   identical digests at every height.
//! * [`fuzz`] — seeded mutational fuzzer over the tx and block decode paths of
//!   both implementations (no cargo-fuzz): no panics, identical
//!   well-formed/malformed verdicts, identical parsed fields and senders.
//! * [`golden`] — every SPEC.md Appendix A vector asserted against the ENGINE.
//!
//! OUT OF DOMAIN (deliberately never generated): balances near 2^128. SPEC
//! §4.4 leaves balance `+=` overflow unchecked (the engine wraps in release,
//! panics in debug) and the oracle panics by design (SPEC-QUESTIONS Q7/Q8).
//! Balances only move between accounts (fees go to the in-state
//! beneficiary), so the total supply is the genesis sum and bounds every
//! balance and every credit accumulator; [`gen`] caps
//! `fund_balance * funded_accounts` below 2^110, far under 2^120.

pub mod diff;
pub mod fuzz;
pub mod gen;
pub mod golden;
pub mod norm;
pub mod restart;
pub mod rng;
pub mod util;

/// The lane chain id every check runs under (SPEC.md default).
pub const CHAIN_ID: u64 = 1338;

/// Read a count from the environment (`LEAN_DIFF_CASES` etc.), else `default`.
pub fn env_count(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().replace('_', "").parse().ok())
        .unwrap_or(default)
}
