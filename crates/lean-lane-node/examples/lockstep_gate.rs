//! THE I1 CONFIDENCE GATE: two-node lockstep determinism.
//!
//! Node A: ingests txs into its real pool, arc_buildBlock per tick, then
//! arc_newBlock's its OWN built block. Node B: receives ONLY arc_newBlock
//! bytes of A's blocks. Head commitments must be equal at every height, and
//! the flat-state digests equal at the end. Includes a kill-and-restart of B
//! mid-stream (recovery from snapshot+log tail, then continued feeding), and
//! in-stream idempotency re-feeds. This substitutes for A/B-vs-unmodified-
//! peers on a brand-new lane type.
//!
//! Run: cargo run --release -p lean-lane-node --example lockstep_gate -- [blocks]

use alloy_primitives::Address;
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome};
use lean_lane_node::state::Acct;
use lean_native::envelope::{current_lane_domain, LeanSigned};
use lean_native::{lean_fee, LeanTx, Output, AMOUNT_UNIT};
use secp256k1::SecretKey;
use std::time::Instant;

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> eyre::Result<()> {
    let blocks: u64 =
        std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(10_000);
    let dir_a = std::env::temp_dir().join(format!("lockstep-a-{}", std::process::id()));
    let dir_b = std::env::temp_dir().join(format!("lockstep-b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
    // deliberately different snapshot cadences on A and B
    let (a, _) = LaneNode::open(Config {
        datadir: dir_a.clone(),
        snapshot_every: 512,
        ..Default::default()
    })?;
    let mut b = LaneNode::open(Config {
        datadir: dir_b.clone(),
        snapshot_every: 257,
        ..Default::default()
    })?
    .0;

    let domain = current_lane_domain();
    let mix = [1usize, 3, 10, 1, 3, 1, 10, 100];
    let restart_at = blocks / 2;
    let t0 = Instant::now();

    // GENESIS-STYLE setup (the lockstep gate's own first catch: mid-stream
    // out-of-band seeding is in neither snapshot nor blocks, so a restart
    // loses it — production funds at genesis only). Pre-sign every tx, seed
    // every sender on BOTH nodes, persist snapshot-0 on both; from here on,
    // blocks are the ONLY state source.
    println!("  pre-signing + seeding genesis ...");
    let mut per_height: Vec<Vec<Vec<u8>>> = Vec::with_capacity(blocks as usize + 1);
    per_height.push(Vec::new()); // height 0 unused
    let mut key = 0u64;
    let mut total_txs = 0u64;
    for height in 1..=blocks {
        let mut txs = Vec::new();
        if height % 17 != 0 {
            for t in 0..12u64 {
                let n = mix[((height + t) % mix.len() as u64) as usize];
                let outs: Vec<Output> = (0..n)
                    .map(|j| Output {
                        to: Address::with_last_byte(((key as usize + j * 7) % 251) as u8),
                        amount: 1 + (j % 5) as u64,
                    })
                    .collect();
                let need: u128 = lean_fee(n)
                    + outs.iter().map(|o| o.amount as u128 * AMOUNT_UNIT).sum::<u128>();
                let tx = LeanTx::sign(0, outs, &sk(key), &domain);
                let signed = LeanSigned::new(tx);
                let sender = signed.tx().recover_sender(&domain).unwrap();
                a.seed_account(sender, Acct { nonce: 0, balance: need }).await;
                b.seed_account(sender, Acct { nonce: 0, balance: need }).await;
                txs.push(signed.raw().to_vec());
                key += 1;
                total_txs += 1;
            }
        }
        per_height.push(txs);
    }
    for n in [&a, &b] {
        let (c, num, ts) = *n.head.lock().await;
        n.state.lock().await.write_snapshot(&n.cfg.datadir, c, num, ts)?;
    }
    println!("  genesis ready: {total_txs} txs, {key} senders ({:?})", t0.elapsed());

    let mut prev_wire: Option<Vec<u8>> = None;
    for height in 1..=blocks {
        for raw in &per_height[height as usize] {
            a.submit_raw(raw).await.map_err(|e| eyre::eyre!(e))?;
        }
        let (head_c, head_n, _) = *a.head.lock().await;
        let (ca, wire) = a
            .shim_build(head_c, head_n + 1, height * 250, 150_000_000)
            .await
            .map_err(|e| eyre::eyre!(e))?;
        let NewBlockOutcome::Valid(ca2) = a.shim_new(&wire).await.map_err(|e| eyre::eyre!(e))?
        else { eyre::bail!("A SYNCING on own block") };
        assert_eq!(ca, ca2, "A's own newBlock must agree with its build");
        let NewBlockOutcome::Valid(cb) = b.shim_new(&wire).await.map_err(|e| eyre::eyre!(e))?
        else { eyre::bail!("B SYNCING on in-order feed at height {height}") };
        assert_eq!(ca, cb, "lockstep broke at height {height}");

        // in-stream idempotency: occasionally re-feed the previous block to B
        if height % 97 == 0 {
            if let Some(w) = &prev_wire {
                b.shim_new(w).await.map_err(|e| eyre::eyre!(e))?;
            }
        }
        prev_wire = Some(wire);

        // kill-and-restart B mid-stream: recovery = snapshot + tail replay
        if height == restart_at {
            let digest_before = b.state.lock().await.digest();
            let head_before = *b.head.lock().await;
            drop(b);
            let t_rec = Instant::now();
            let (nb, rec) = LaneNode::open(Config {
                datadir: dir_b.clone(),
                snapshot_every: 257,
                ..Default::default()
            })?;
            b = nb;
            assert_eq!(*b.head.lock().await, head_before, "B head after restart");
            assert_eq!(b.state.lock().await.digest(), digest_before, "B state after restart");
            println!(
                "  [restart @ {height}] B recovered: snapshot@{} + {} blocks in {:?}",
                rec.snapshot_number, rec.replayed_blocks, t_rec.elapsed()
            );
        }
        if height % 1000 == 0 {
            println!("  {height}/{blocks} heights, {total_txs} txs, {:?}", t0.elapsed());
        }
    }

    let (ca, na, _) = *a.head.lock().await;
    let (cb, nb, _) = *b.head.lock().await;
    assert_eq!((ca, na), (cb, nb), "final heads differ");
    let da = a.state.lock().await.digest();
    let db = b.state.lock().await.digest();
    assert_eq!(da, db, "final state digests differ");
    println!();
    println!("LOCKSTEP GATE PASS: {blocks} blocks, {total_txs} txs, heads+digests equal throughout, B restart survived, in-stream idempotency ok ({:?})", t0.elapsed());
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
    Ok(())
}
