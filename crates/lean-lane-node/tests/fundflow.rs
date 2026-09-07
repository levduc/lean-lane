//! Live-regression gate: fund-file boot + nonce-0 submit must land in the
//! PENDING subpool (not queued) and get packed by arc_buildBlock.

use alloy_primitives::Address;
use lean_lane_node::node::{Config, LaneNode};
use lean_lane_node::state::Acct;
use lean_native::envelope::{current_lane_domain, LeanSigned};
use lean_native::{lean_fee, LeanTx, Output, AMOUNT_UNIT};
use reth_transaction_pool::TransactionPool;
use secp256k1::SecretKey;

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn funded_nonce0_tx_is_pending_and_packed() {
    let dir = tempfile::tempdir().unwrap();
    let (n, _) = LaneNode::open(Config { datadir: dir.path().into(), ..Default::default() }).unwrap();
    let domain = current_lane_domain();

    // fund-file analog: seed AFTER open (exactly what run-mode does)
    let tx = LeanTx::sign(
        0,
        vec![
            Output { to: Address::with_last_byte(1), amount: 5 },
            Output { to: Address::with_last_byte(2), amount: 7 },
        ],
        &sk(1),
        &domain,
    );
    let signed = LeanSigned::new(tx);
    let sender = signed.tx().recover_sender(&domain).unwrap();
    // the repro's funding: 1e18 wei
    n.seed_account(sender, Acct { nonce: 0, balance: 1_000_000_000_000_000_000 }).await;
    assert!(1_000_000_000_000_000_000u128 > lean_fee(2) + 12 * AMOUNT_UNIT);

    n.submit_raw(signed.raw()).await.expect("must be accepted");
    let s = n.pool.pool_size();
    assert_eq!(
        (s.pending, s.queued),
        (1, 0),
        "nonce-0 funded tx must be PENDING, got pending={} queued={} basefee={}",
        s.pending,
        s.queued,
        s.basefee
    );

    // and buildBlock must pack it
    let (hc, hn, _) = *n.head.lock().await;
    let (_c, wire) = n.shim_build(hc, hn + 1, 1000, 150_000_000).await.unwrap();
    let blk = lean_lane_node::chain::LeanBlock::from_wire_bytes(&wire).unwrap();
    assert_eq!(blk.txs.len(), 1, "buildBlock must pack the pending tx");
}

#[tokio::test(flavor = "multi_thread")]
async fn recovered_datadir_keeps_provider_and_state_in_lockstep() {
    // fleet shape: datadir carries prior blocks; after reopen, the RPC nonce
    // view, the validator's view, and admission must all agree.
    let dir = tempfile::tempdir().unwrap();
    let domain = current_lane_domain();
    let sender_key = 7u64;
    {
        let (n, _) = LaneNode::open(Config { datadir: dir.path().into(), ..Default::default() }).unwrap();
        let tx0 = LeanTx::sign(0, vec![Output { to: Address::with_last_byte(9), amount: 1 }], &sk(sender_key), &domain);
        let s0 = LeanSigned::new(tx0);
        let sender = s0.tx().recover_sender(&domain).unwrap();
        n.seed_account(sender, Acct { nonce: 0, balance: 10 * (lean_fee(1) + AMOUNT_UNIT) }).await;
        // persist genesis (run-mode does this after funding)
        let (cm, num, ts) = *n.head.lock().await;
        n.state.lock().await.write_snapshot(&n.cfg.datadir, cm, num, ts).unwrap();
        // three blocks from this sender, nonces 0,1,2
        for nonce in 0..3u32 {
            let t = LeanTx::sign(nonce, vec![Output { to: Address::with_last_byte(9), amount: 1 }], &sk(sender_key), &domain);
            let s = LeanSigned::new(t);
            n.submit_raw(s.raw()).await.unwrap();
            n.produce_block(1000 + nonce as u64).await.unwrap().unwrap();
        }
        assert_eq!(n.state.lock().await.get(&sender).nonce, 3);
    }
    // REOPEN (recovery = snapshot + replay)
    let (n2, rec) = LaneNode::open(Config { datadir: dir.path().into(), ..Default::default() }).unwrap();
    assert_eq!(rec.replayed_blocks, 3);
    let probe = LeanTx::sign(3, vec![Output { to: Address::with_last_byte(9), amount: 1 }], &sk(sender_key), &domain);
    let sp = LeanSigned::new(probe);
    let sender = sp.tx().recover_sender(&domain).unwrap();
    // 1) RPC-visible nonce (what spammer -l reads) == 3
    assert_eq!(n2.state.lock().await.get(&sender).nonce, 3, "state nonce after recovery");
    // 2) admission at exactly that nonce must be PENDING
    n2.submit_raw(sp.raw()).await.expect("nonce-3 must be accepted after recovery");
    let s = n2.pool.pool_size();
    assert_eq!((s.pending, s.queued), (1, 0),
        "recovered-view divergence: pending={} queued={} basefee={}", s.pending, s.queued, s.basefee);
    // 3) stale nonce-0 resubmit must be REJECTED (not queued)
    let t0 = LeanTx::sign(0, vec![Output { to: Address::with_last_byte(9), amount: 1 }], &sk(sender_key), &domain);
    assert!(n2.submit_raw(LeanSigned::new(t0).raw()).await.is_err(), "stale nonce must reject");
}

#[tokio::test(flavor = "multi_thread")]
async fn streamed_nonces_promote_across_blocks() {
    // THE fleet regression shape: one sender streams nonces 0..30 into the
    // pool faster than blocks consume them. After each block, the remaining
    // txs must be re-based to PENDING (not park in queued forever).
    let dir = tempfile::tempdir().unwrap();
    let (n, _) = LaneNode::open(Config {
        datadir: dir.path().into(),
        budget_gas: 260_000, // fits ~10 single-output lean txs (26k gas each)
        ..Default::default()
    })
    .unwrap();
    let domain = current_lane_domain();
    let sender_key = 42u64;
    let per_tx = lean_fee(1) + AMOUNT_UNIT;
    let first = LeanTx::sign(0, vec![Output { to: Address::with_last_byte(3), amount: 1 }], &sk(sender_key), &domain);
    let sender = LeanSigned::new(first).tx().recover_sender(&domain).unwrap();
    n.seed_account(sender, Acct { nonce: 0, balance: 40 * per_tx }).await;

    for nonce in 0..30u32 {
        let t = LeanTx::sign(nonce, vec![Output { to: Address::with_last_byte(3), amount: 1 }], &sk(sender_key), &domain);
        n.submit_raw(LeanSigned::new(t).raw()).await.unwrap();
    }
    let s0 = n.pool.pool_size();
    assert_eq!((s0.pending, s0.queued), (30, 0), "burst must be fully pending (consecutive nonces)");

    // consume in ~10-tx blocks; after EVERY block the remainder must be pending
    let mut mined = 0usize;
    let mut height = 0u64;
    while mined < 30 {
        height += 1;
        let bn = n.produce_block(1000 + height).await.unwrap();
        assert!(bn.is_some(), "block must not be empty while txs remain (mined={mined})");
        let s = n.pool.pool_size();
        mined = 30 - (s.pending + s.queued);
        assert_eq!(
            s.queued, 0,
            "REGRESSION: {} txs parked in queued after block {height} (pending={})",
            s.queued, s.pending
        );
        assert!(height < 10, "must drain within a few blocks");
    }
    assert_eq!(n.state.lock().await.get(&sender).nonce, 30);
}
