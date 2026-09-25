//! Differential property test, engine vs oracle (see `lean_diff::diff`).
//!
//! Quick profile by default (200 chains x 20 blocks with the default mix,
//! plus a hostile and a benign mix). Scale with env vars:
//!
//! ```text
//! LEAN_DIFF_CASES=5000 LEAN_DIFF_BLOCKS=20 LEAN_DIFF_SEED=7 \
//!     cargo test --release -p lean-diff --test differential
//! LEAN_DIFF_RATES='valid=5,bad_sig=20,corrupt_framing=0.2'   # change the mix
//! ```

use lean_diff::diff::{ddmin, run, summary_line, DiffCfg};
use lean_diff::env_count;
use lean_diff::gen::Rates;

fn cfg(seed_salt: u64, chains: u64, rates: Rates) -> DiffCfg {
    let mut c = DiffCfg::new(
        env_count("LEAN_DIFF_SEED", 1) ^ seed_salt,
        chains,
        env_count("LEAN_DIFF_BLOCKS", 20),
    );
    c.rates = rates;
    c
}

fn go(c: DiffCfg) {
    match run(&c) {
        Ok(st) => {
            eprintln!("{}", summary_line(&st));
            assert_eq!(st.chains, c.chains);
            assert!(
                st.items > 0 && st.applied > 0 && st.noops > 0,
                "degenerate run: {st:?}"
            );
        }
        Err(m) => panic!("\n{m}\n"),
    }
}

#[test]
fn engine_matches_oracle_default_mix() {
    let rates = Rates::from_env().expect("LEAN_DIFF_RATES");
    go(cfg(0, env_count("LEAN_DIFF_CASES", 200), rates));
}

/// Mostly adversarial content, big blocks, frequent framing corruption.
#[test]
fn engine_matches_oracle_hostile_mix() {
    let rates = Rates::parse(
        "valid=5,burst=2,bad_sig=12,malformed=12,garbage=8,v_variant=10,high_s=10,trailing=8,\
         replay=8,dup_in_block=8,max_entries=80,corrupt_framing=0.15,empty_block=0.1",
    )
    .unwrap();
    go(cfg(
        0x4057,
        (env_count("LEAN_DIFF_CASES", 200) / 4).max(1),
        rates,
    ));
}

/// Mostly applying content: long nonce chains, credits, exact balances,
/// large fan-outs.
#[test]
fn engine_matches_oracle_benign_mix() {
    let rates = Rates::parse(
        "valid=40,burst=15,credit_spend=10,exact_balance=10,over_by_one=5,bad_sig=1,\
         malformed=1,garbage=0,big_fanout=0.03,corrupt_framing=0,empty_block=0",
    )
    .unwrap();
    go(cfg(
        0xbe9,
        (env_count("LEAN_DIFF_CASES", 200) / 4).max(1),
        rates,
    ));
}

#[test]
fn ddmin_finds_the_minimal_failing_subset() {
    let xs: Vec<u32> = (0..40).collect();
    let min = ddmin(&xs, |s| {
        s.contains(&3) && s.contains(&17) && s.contains(&31)
    });
    assert_eq!(min, vec![3, 17, 31]);
    let min = ddmin(&xs, |s| s.iter().filter(|x| *x % 2 == 0).count() >= 2);
    assert_eq!(min.len(), 2);
}

#[test]
fn rates_parse_and_reject() {
    let r = Rates::parse("valid=0,bad_sig=7,empty_block=0.5,max_entries=3").unwrap();
    assert_eq!((r.weights[0], r.empty_block, r.max_entries), (0, 0.5, 3));
    assert!(Rates::parse("nonsense=1").is_err());
    assert!(Rates::parse("valid").is_err());
}
