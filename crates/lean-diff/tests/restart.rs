//! Replay/restart equivalence at the engine level (see `lean_diff::restart`).
//! Scale with `LEAN_RESTART_CASES` / `LEAN_RESTART_BLOCKS`.

use lean_diff::env_count;
use lean_diff::gen::Rates;

#[test]
fn straight_replay_and_snapshot_restart_agree() {
    let chains = env_count("LEAN_RESTART_CASES", 40);
    let blocks = env_count("LEAN_RESTART_BLOCKS", 30);
    let st = lean_diff::restart::run(
        env_count("LEAN_DIFF_SEED", 1),
        chains,
        blocks,
        &Rates::default(),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    eprintln!("restart: {st:?}");
    assert!(st.restarts >= chains);
}
