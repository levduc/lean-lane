//! Crash-consistency gate: kill mid-append (torn tail, garbage tail) and
//! snapshot+tail recovery must reproduce exactly the state of the intact
//! prefix.

mod common;

use alloy_primitives::Address;
use lean_lane_node::node::{Config, LaneNode, NewBlockOutcome};
use lean_lane_node::state::Acct;
use lean_native::envelope::{current_lane_domain, LeanSigned};
use lean_native::{lean_fee, LeanTx, Output, AMOUNT_UNIT};
use secp256k1::SecretKey;
use std::io::Write;

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

async fn make_node(dir: &std::path::Path, snapshot_every: u64) -> LaneNode {
    let cfg = Config {
        datadir: dir.to_path_buf(),
        snapshot_every,
        ..Default::default()
    };
    LaneNode::open(cfg).unwrap().0
}

/// Seed + submit `count` single-output txs (one per sender), producing one
/// block per `per_block` txs. Returns nothing; state lives in the node.
async fn drive(node: &LaneNode, count: u64, per_block: u64) {
    let domain = current_lane_domain();
    for i in 0..count {
        let tx = LeanTx::sign(
            0,
            vec![Output {
                to: Address::with_last_byte((i % 200) as u8),
                amount: 5,
            }],
            &sk(i),
            &domain,
        );
        let signed = LeanSigned::new(tx);
        let sender = signed.tx().recover_sender(&domain).unwrap();
        node.seed_account(
            sender,
            Acct {
                nonce: 0,
                balance: lean_fee(1) + 5 * AMOUNT_UNIT,
            },
        )
        .await;
        node.submit_raw(signed.raw()).await.unwrap();
        if (i + 1) % per_block == 0 {
            common::produce(node, 1_000 + i, 150_000_000).await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_from_snapshot_plus_tail_and_torn_writes() {
    let dir = tempfile::tempdir().unwrap();
    let node = make_node(dir.path(), 2).await; // snapshot every 2 blocks
    drive(&node, 12, 3).await; // 4 blocks: snapshots at 2 and 4, tail = none
    let digest = node.state.lock().await.digest();
    let head = *node.head.lock().await;
    drop(node);

    // 1) clean recovery: snapshot@4 (+ zero tail)
    let node2 = make_node(dir.path(), 2).await;
    assert_eq!(node2.state.lock().await.digest(), digest);
    assert_eq!(
        *node2.head.lock().await,
        head,
        "head must be re-derived from the log tail"
    );
    drop(node2);

    // 2) torn tail: append a partial record (kill mid-append)
    let log_path = dir.path().join("blocks.log");
    let len_before = std::fs::metadata(&log_path).unwrap().len();
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .unwrap();
        f.write_all(&123u32.to_le_bytes()).unwrap(); // claims 123-byte payload...
        f.write_all(&[0xAB; 40]).unwrap(); // ...delivers 40 garbage bytes
    }
    let node3 = make_node(dir.path(), 2).await;
    assert_eq!(
        node3.state.lock().await.digest(),
        digest,
        "torn tail must be ignored"
    );
    assert_eq!(
        std::fs::metadata(&log_path).unwrap().len(),
        len_before,
        "torn tail must be truncated away"
    );
    drop(node3);

    // 3) garbage that parses as a length but fails the commitment check
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .unwrap();
        let garbage = vec![0u8; 96];
        f.write_all(&(96u32).to_le_bytes()).unwrap();
        f.write_all(&garbage).unwrap();
        f.write_all(&(96u32).to_le_bytes()).unwrap();
    }
    let node4 = make_node(dir.path(), 2).await;
    assert_eq!(
        node4.state.lock().await.digest(),
        digest,
        "bad-commitment tail must be ignored"
    );

    // 4) and the lane keeps working after recovery: one more block applies
    drive(&node4, 3, 3).await;
    assert_ne!(node4.state.lock().await.digest(), digest);
    let head4 = *node4.head.lock().await;
    assert_eq!(
        head4.1,
        head.1 + 1,
        "exactly one block appended after recovery"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn total_stf_noop_never_halts() {
    // A tx that passes pool admission but is invalidated before execution
    // (another tx from the same sender executed first via direct injection)
    // must become a NO-OP, not an error.
    let dir = tempfile::tempdir().unwrap();
    let node = make_node(dir.path(), 1024).await;
    let domain = current_lane_domain();
    let t1 = LeanTx::sign(
        0,
        vec![Output {
            to: Address::with_last_byte(1),
            amount: 5,
        }],
        &sk(500),
        &domain,
    );
    let s1 = LeanSigned::new(t1);
    let sender = s1.tx().recover_sender(&domain).unwrap();
    node.seed_account(
        sender,
        Acct {
            nonce: 0,
            balance: 2 * (lean_fee(1) + 5 * AMOUNT_UNIT),
        },
    )
    .await;
    node.submit_raw(s1.raw()).await.unwrap();
    common::produce(&node, 1_000, 150_000_000).await.unwrap();

    // Same sender, nonce 0 again — stale by now; force it around the pool by
    // injecting directly into a block via the log-replay path semantics:
    // simplest equivalent: submit nonce-2 (gap) is rejected by pool; so build
    // a block by hand through apply_block with a stale item.
    let stale = LeanTx::sign(
        0,
        vec![Output {
            to: Address::with_last_byte(2),
            amount: 5,
        }],
        &sk(500),
        &domain,
    );
    let stale_signed = LeanSigned::new(stale);
    let mut state = node.state.lock().await;
    let before = state.digest();
    let out = lean_lane_node::exec::apply_block(
        &mut state,
        &[(sender, stale_signed.tx().clone(), *stale_signed.hash())],
        Address::with_last_byte(0xbe),
    );
    assert_eq!(out.applied, 0);
    assert_eq!(out.noops, 1);
    assert_eq!(state.digest(), before, "no-op leaves state untouched");
}

/// I2: `fsync: false` fsyncs the SNAPSHOT (tmp + sync_all + rename) but not the
/// log append, so a crash can leave a durable snapshot at a height the log
/// never reached. Resuming from that snapshot would put the head above a hole
/// the backfill never repairs (it only pulls `head + 1` upward), leaving those
/// heights unservable forever and turning a re-feed of one of them into the
/// hard error "height N predates the log".
///
/// `open` must instead fall back to the newest snapshot the log can continue
/// from and replay to the last intact record — state and log ending at the
/// same block, the difference becoming ordinary lag.
#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_ahead_of_the_log_falls_back_instead_of_leaving_a_hole() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("blocks.log");
    // snapshot every 2 blocks. Every sender is seeded BEFORE the first block:
    // `seed_account` is a test hook, not a logged event, so an account created
    // after a snapshot cannot be reproduced by replaying the log and the
    // digests would not compare.
    let node = make_node(dir.path(), 2).await;
    let domain = current_lane_domain();
    let txs: Vec<_> = (0..12u64)
        .map(|i| {
            let out = vec![Output {
                to: Address::with_last_byte((i % 200) as u8),
                amount: 5,
            }];
            LeanSigned::new(LeanTx::sign(0, out, &sk(900 + i), &domain))
        })
        .collect();
    for t in &txs {
        let sender = t.tx().recover_sender(&domain).unwrap();
        node.seed_account(
            sender,
            Acct {
                nonce: 0,
                balance: lean_fee(1) + 5 * AMOUNT_UNIT,
            },
        )
        .await;
    }
    let mut produced = 0u64;
    for (i, t) in txs.iter().enumerate() {
        node.submit_raw(t.raw()).await.unwrap();
        if (i + 1) % 3 == 0 {
            common::produce(&node, 1_000 + i as u64, 150_000_000)
                .await
                .unwrap();
            produced += 1;
        }
        if produced == 3 && (i + 1) % 3 == 0 {
            break;
        }
    }
    // blocks 1..3 are in the log, snapshot at 2
    let len3 = std::fs::metadata(&log_path).unwrap().len();
    let digest3 = node.state.lock().await.digest();
    let head3 = *node.head.lock().await;
    assert_eq!(head3.1, 3);
    for t in txs.iter().skip(9) {
        node.submit_raw(t.raw()).await.unwrap();
    }
    common::produce(&node, 2_000, 150_000_000).await.unwrap(); // block 4 -> snapshot-4.bin
    assert_eq!(node.head.lock().await.1, 4);
    drop(node);
    assert!(dir.path().join("snapshot-4.bin").exists());

    // the crash: the snapshot survives, the log tail does not
    std::fs::OpenOptions::new()
        .write(true)
        .open(&log_path)
        .unwrap()
        .set_len(len3)
        .unwrap();

    let (n2, report) = LaneNode::open(Config {
        datadir: dir.path().to_path_buf(),
        snapshot_every: 2,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        report.snapshot_number, 2,
        "recovered from the snapshot BELOW the log tail"
    );
    assert_eq!(
        *n2.head.lock().await,
        head3,
        "head is the last intact record"
    );
    assert_eq!(
        n2.state.lock().await.digest(),
        digest3,
        "state matches that head exactly"
    );
    assert!(
        dir.path().join("snapshot-4.bin").exists(),
        "the ahead snapshot is left alone"
    );

    // no hole: height 4 is head+1 again, so it is ordinary lag the chain fills
    for t in txs.iter().skip(9) {
        n2.submit_raw(t.raw()).await.unwrap();
    }
    common::produce(&n2, 3_000, 150_000_000).await.unwrap();
    assert_eq!(n2.head.lock().await.1, 4);
    assert!(
        n2.log.lock().await.read_block(4).unwrap().is_some(),
        "4 is in the log now"
    );
}

/// `--snapshot-every 0` is the natural way to say "never snapshot", and both
/// append paths took `block.number % self.cfg.snapshot_every` unguarded: the
/// node panicked with a divide-by-zero on the FIRST block it committed. Zero
/// now means never, so recovery replays the whole log from genesis.
///
/// Both `% 0` sites are driven here: `commit_block` (the by-bytes
/// path) and `promote_staged` (the vote-gap anchor).
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_every_zero_never_snapshots_and_never_panics() {
    let dir = tempfile::tempdir().unwrap();
    let head;
    {
        let node = make_node(dir.path(), 0).await;
        drive(&node, 12, 3).await; // 4 blocks through `commit_block`
        assert_eq!(node.head.lock().await.1, 4);

        // ...and one through `promote_staged`
        let domain = current_lane_domain();
        let signed = LeanSigned::new(LeanTx::sign(
            0,
            vec![Output {
                to: Address::with_last_byte(9),
                amount: 5,
            }],
            &sk(999),
            &domain,
        ));
        let sender = signed.tx().recover_sender(&domain).unwrap();
        node.seed_account(
            sender,
            Acct {
                nonce: 0,
                balance: lean_fee(1) + 5 * AMOUNT_UNIT,
            },
        )
        .await;
        node.submit_raw(signed.raw()).await.unwrap();
        let (hc, hn, ts) = *node.head.lock().await;
        let (c, _) = node
            .shim_build(hc, hn + 1, ts + 1_000, 150_000_000)
            .await
            .unwrap();
        assert_eq!(
            node.shim_new_by_commitment(c).await.unwrap(),
            NewBlockOutcome::Valid(c)
        );
        assert_eq!(
            node.metrics
                .promoted
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        head = *node.head.lock().await;
        assert_eq!(head.1, 5);
    }

    let snaps: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("snapshot-"))
        .collect();
    assert!(snaps.is_empty(), "snapshot_every 0 still wrote {snaps:?}");

    // Recovery is a full replay from genesis. (The pre-block `seed_account`
    // funding is NOT in the log, so the replayed txs are no-ops and the state
    // is legitimately different — the documented "unsnapshotted seeds diverge"
    // gotcha. The chain itself is what this asserts.)
    let (n2, report) = LaneNode::open(Config {
        datadir: dir.path().to_path_buf(),
        snapshot_every: 0,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(report.snapshot_number, 0, "no snapshot to start from");
    assert_eq!(report.replayed_blocks, 5, "the whole log replays");
    assert_eq!(*n2.head.lock().await, head);
}
