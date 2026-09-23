//! Helpers shared by the integration tests.

use lean_lane_node::chain::LeanBlock;
use lean_lane_node::node::{LaneNode, NewBlockOutcome};

/// Build head+1 from the pool under `budget_gas` (`arc_buildBlock`) and
/// commit it through the FULL execution path (`arc_newBlock{blockBytes}` with
/// the self-staged copy dropped, so `commit_block` runs rather than a
/// promote). Returns the new height, or `None` (appending nothing) if the pool
/// had nothing to pack.
pub async fn produce(n: &LaneNode, timestamp_ms: u64, budget_gas: u64) -> Option<u64> {
    let (head_c, head_n, _) = *n.head.lock().await;
    let (c, wire) = n
        .shim_build(head_c, head_n + 1, timestamp_ms, budget_gas)
        .await
        .unwrap();
    n.staged.lock().await.retain(|(k, _)| *k != c);
    if LeanBlock::from_wire_bytes(&wire).unwrap().txs.is_empty() {
        return None;
    }
    assert_eq!(n.shim_new(&wire).await.unwrap(), NewBlockOutcome::Valid(c));
    Some(head_n + 1)
}
