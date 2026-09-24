//! Pool live-regression gates: a funded nonce-0 tx lands PENDING and is
//! packed; a recovered datadir keeps the pool's view and the state in
//! lockstep; streamed nonces re-base to pending after every block.

mod common;

use common::*;
use lean_lane_node::chain::LeanBlock;
use lean_lane_node::node::LaneNode;
use reth_transaction_pool::TransactionPool;

fn pending_queued(n: &LaneNode) -> (usize, usize) {
    let s = n.pool.pool_size();
    (s.pending, s.queued)
}

#[tokio::test(flavor = "multi_thread")]
async fn funded_nonce0_tx_is_pending_and_packed() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    // fund-file analog: seeded AFTER open, 1e18 wei
    fund(&n, 1, 1_000_000_000_000_000_000).await;
    n.submit_raw(signed(1, 0, 2, 6).raw())
        .await
        .expect("must be accepted");
    assert_eq!(
        pending_queued(&n),
        (1, 0),
        "nonce-0 funded tx must be PENDING"
    );
    let (_, wire) = build_next(&n).await;
    assert_eq!(LeanBlock::from_wire_bytes(&wire).unwrap().txs.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn recovered_datadir_keeps_provider_and_state_in_lockstep() {
    let dir = tempfile::tempdir().unwrap();
    let key = 7u64;
    {
        let n = open(dir.path());
        fund(&n, key, 10 * cost(1, 1)).await;
        snapshot_now(&n).await;
        for nonce in 0..3u32 {
            n.submit_raw(signed(key, nonce, 1, 1).raw()).await.unwrap();
            produce(&n, 1000 + nonce as u64, 150_000_000).await.unwrap();
        }
        assert_eq!(n.state.lock().await.get(&sender(key)).nonce, 3);
    }
    let (n, rec) = LaneNode::open(cfg(dir.path())).unwrap();
    assert_eq!(rec.replayed_blocks, 3);
    assert_eq!(n.state.lock().await.get(&sender(key)).nonce, 3);
    n.submit_raw(signed(key, 3, 1, 1).raw())
        .await
        .expect("the next nonce must be accepted after recovery");
    assert_eq!(pending_queued(&n), (1, 0), "recovered-view divergence");
    assert!(
        n.submit_raw(signed(key, 0, 1, 1).raw()).await.is_err(),
        "a stale nonce must be rejected"
    );
}

/// One sender streams nonces 0..30 faster than blocks consume them: after
/// every block the remainder must re-base to PENDING, not park in queued.
#[tokio::test(flavor = "multi_thread")]
async fn streamed_nonces_promote_across_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let n = open(dir.path());
    let key = 42u64;
    fund(&n, key, 40 * cost(1, 1)).await;
    for nonce in 0..30u32 {
        n.submit_raw(signed(key, nonce, 1, 1).raw()).await.unwrap();
    }
    assert_eq!(pending_queued(&n), (30, 0));
    let mut height = 0u64;
    loop {
        let (p, q) = pending_queued(&n);
        if p + q == 0 {
            break;
        }
        height += 1;
        assert!(height < 10, "must drain within a few blocks");
        // ~10 single-output txs per block (26k gas each)
        assert!(produce(&n, 1000 + height, 260_000).await.is_some());
        assert_eq!(
            pending_queued(&n).1,
            0,
            "txs parked in queued after block {height}"
        );
    }
    assert_eq!(n.state.lock().await.get(&sender(key)).nonce, 30);
}
