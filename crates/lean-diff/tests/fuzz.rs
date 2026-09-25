//! Mutational decode fuzzer (see `lean_diff::fuzz`). Quick budget by
//! default; scale with `LEAN_FUZZ_ITERS` (and `LEAN_FUZZ_SEED`).

use lean_diff::env_count;
use lean_diff::fuzz::{check_tx, infinity_tx, run, FuzzStats};
use lean_diff::rng::Rng;
use lean_diff::util::{keys, threads, N};
use lean_native::Output;

#[test]
fn decode_paths_agree_under_mutation() {
    let seed = env_count("LEAN_FUZZ_SEED", 1);
    let iters = env_count("LEAN_FUZZ_ITERS", 30_000);
    match run(seed, 0, iters, threads()) {
        Ok(st) => {
            eprintln!("fuzz: {st:?}");
            assert_eq!(st.iterations, iters);
            assert!(st.tx_prefix_wellformed > 0 && st.recovered > 0 && st.blocks_well_framed > 0);
            assert!(
                st.tx_inputs > st.tx_prefix_wellformed,
                "no malformed inputs generated"
            );
        }
        Err(f) => panic!("\n{f}\n"),
    }
}

/// SPEC-QUESTIONS Q2: a recovery that lands on the point at infinity is a
/// failure on both sides.
#[test]
fn point_at_infinity_is_unrecoverable_on_both_sides() {
    let mut st = FuzzStats::default();
    for k in [2u64, 3, 5, 0xabcdef, u64::MAX] {
        let b = infinity_tx(&keys()[3], k);
        for par in [true, false] {
            check_tx(&b, par, true, &mut st).unwrap();
        }
        assert!(lean_oracle::classify_entry(&b, 1338).is_none());
    }
    assert_eq!(st.recovered, 0);
}

/// SPEC §1.5 / SPEC-QUESTIONS Q12: v in {2,3} (x = r + n) CAN recover when
/// r < p - n. Both sides must agree on which such signatures recover, and to
/// which address.
#[test]
fn recovery_ids_2_and_3_agree() {
    let mut rng = Rng::new(0x2323);
    let mut st = FuzzStats::default();
    let key = &keys()[5];
    for i in 0..600u32 {
        let mut tx = key.sign(
            i,
            vec![Output {
                to: key.addr,
                amount: 1,
            }],
        );
        let mut r = [0u8; 32];
        match i % 3 {
            0 => rng.fill(&mut r[16..]), // r < 2^128 < p - n: x = r + n is a field element
            1 => rng.fill(&mut r[15..]), // r ~ 2^136: r + n >= p, must fail
            _ => r = lean_diff::util::sub_be(&N, &lean_diff::util::be32_from_u64(1 + i as u64)),
        }
        tx.sig[..32].copy_from_slice(&r);
        tx.sig[64] = 2 + (i as u8 & 1);
        for par in [true, false] {
            check_tx(&tx.encode(), par, true, &mut st).unwrap();
        }
    }
    eprintln!(
        "v in {{2,3}}: {} of {} recovered",
        st.recovered, st.recoveries_compared
    );
    assert!(
        st.recovered > 0,
        "the x = r + n recovery path was never exercised"
    );
}
