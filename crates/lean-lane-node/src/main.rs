//! Lean lane node binary.
//!
//!   lean-lane-node run   [--datadir D] [--port P] [--block-ms T] [--budget-gas G]
//!                        [--snapshot-every K] [--chain-id C]
//!   lean-lane-node bench [--txs M] [--mix 1,10,100] [--block-ms T] [--budget-gas G]
//!                        [--datadir D]
//!
//! `bench` is the 2b' gate: generate M fan-out txs (mixed N), submit through
//! the REAL RPC batch endpoint into the REAL reth pool, drive the
//! build-execute-commit loop at a fixed cadence, and report sustained
//! outputs/s + the per-stage split + log bytes/output + recovery time.

use alloy_primitives::Address;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::HttpClientBuilder;
use jsonrpsee::rpc_params;
use lean_lane_node::node::{Config, LaneNode};
use lean_lane_node::rpc;
use lean_lane_node::state::Acct;
use lean_native::envelope::{current_lane_domain, LeanSigned};
use lean_native::{lean_fee, LeanTx, Output, AMOUNT_UNIT};
use secp256k1::SecretKey;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

fn sk(i: u64) -> SecretKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(i + 1).to_be_bytes());
    b[31] = 1;
    SecretKey::from_slice(&b).unwrap()
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => run(&args).await,
        Some("bench") => bench(&args).await,
        _ => {
            eprintln!("usage: lean-lane-node run|bench [flags]  (see src/main.rs header)");
            Ok(())
        }
    }
}

async fn run(args: &[String]) -> eyre::Result<()> {
    let peers: Vec<String> = arg(args, "--peers", String::new())
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.trim().to_string())
        .collect();
    let cfg = Config {
        datadir: arg(args, "--datadir", "lean-lane-data".to_string()).into(),
        chain_id: arg(args, "--chain-id", 1338),
        budget_gas: arg(args, "--budget-gas", 150_000_000),
        snapshot_every: arg(args, "--snapshot-every", 1024),
        peers,
        ..Default::default()
    };
    let block_ms: u64 = arg(args, "--block-ms", 250);
    let port: u16 = arg(args, "--port", 8560);
    // --fund-file F: one 0x-address per line, each seeded with --fund-balance
    // (gwei units, default ~1e15 = 1M native). The Arc integration generates
    // this from the payment genesis alloc; the spammer smoke uses its mnemonic
    // accounts. Only applied to a FRESH datadir (recovered state wins).
    let fund_file: String = arg(args, "--fund-file", String::new());
    let fund_balance: u64 = arg(args, "--fund-balance", 1_000_000_000_000_000);
    let (node, rec) = LaneNode::open(cfg)?;
    let node = Arc::new(node);
    node.spawn_sync();
    println!(
        "recovered: snapshot@{} + {} blocks / {} txs replayed in {:?}",
        rec.snapshot_number, rec.replayed_blocks, rec.replayed_txs, rec.elapsed
    );
    if !fund_file.is_empty() && rec.replayed_blocks == 0 && rec.snapshot_number == 0 {
        let mut n = 0usize;
        for line in std::fs::read_to_string(&fund_file)?.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let a: Address = line.parse().map_err(|e| eyre::eyre!("fund-file '{line}': {e}"))?;
            node.seed_account(a, Acct { nonce: 0, balance: fund_balance as u128 }).await;
            n += 1;
        }
        // Persist the seeded genesis (recovery gotcha: unsnapshotted seeds
        // silently diverge on restart).
        let (c, num, t) = *node.head.lock().await;
        node.state.lock().await.write_snapshot(&node.cfg.datadir, c, num, t)?;
        println!("funded {n} accounts @ {fund_balance} gwei-units from {fund_file}");
    }
    let shim = args.iter().any(|a| a == "--shim")
        || std::env::var("ARC_LEAN_SHIM").map_or(false, |v| v == "1");
    let bind: String = arg(args, "--bind", "127.0.0.1".to_string());
    let bind_ip: std::net::IpAddr = bind.parse().map_err(|e| eyre::eyre!("--bind {bind}: {e}"))?;
    let (addr, _handle) = rpc::serve(node.clone(), (bind_ip, port).into()).await?;
    println!("rpc: http://{addr}  (eth_sendRawTransaction / arc_sendRawTxBatch / txpool_status / arc_getHead / arc_buildBlock / arc_newBlock / arc_getBlockBytes)");
    if shim {
        println!("SHIM MODE: the CL drives block production (arc_newBlock advances the chain; no self-driving loop)");
    } else {
        let driver = node.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(block_ms));
            loop {
                tick.tick().await;
                if let Err(e) = driver.produce_block(now_ms()).await {
                    eprintln!("produce_block: {e:#}");
                }
            }
        });
    }
    tokio::signal::ctrl_c().await?;
    Ok(())
}

async fn bench(args: &[String]) -> eyre::Result<()> {
    let m: usize = arg(args, "--txs", 20_000);
    let mix: String = arg(args, "--mix", "1,10,100".to_string());
    let block_ms: u64 = arg(args, "--block-ms", 250);
    let dir = tempdir_like(arg(args, "--datadir", String::new()));
    let cfg = Config {
        datadir: dir.clone(),
        budget_gas: arg(args, "--budget-gas", 150_000_000),
        snapshot_every: arg(args, "--snapshot-every", 256),
        ..Default::default()
    };
    let mix: Vec<usize> = mix.split(',').filter_map(|s| s.parse().ok()).collect();
    let (node, _) = LaneNode::open(cfg)?;
    let node = Arc::new(node);

    // -- generate + seed ------------------------------------------------------
    println!("signing {m} txs (mix N={mix:?}) ...");
    let domain = current_lane_domain();
    let t0 = Instant::now();
    let mut raws: Vec<Vec<u8>> = Vec::with_capacity(m);
    for i in 0..m as u64 {
        let n = mix[i as usize % mix.len()];
        let outs: Vec<Output> = (0..n)
            .map(|j| Output {
                to: Address::with_last_byte(((i as usize * 13 + j * 7) % 251) as u8),
                amount: 1 + j as u64,
            })
            .collect();
        let tx = LeanTx::sign(0, outs, &sk(i), &domain);
        let signed = LeanSigned::new(tx);
        let sender = signed.tx().recover_sender(&domain).unwrap();
        let need: u128 = lean_fee(n)
            + signed.tx().outputs.iter().map(|o| o.amount as u128 * AMOUNT_UNIT).sum::<u128>();
        node.seed_account(sender, Acct { nonce: 0, balance: need }).await;
        raws.push(signed.raw().to_vec());
    }
    println!("  signed+seeded in {:?}", t0.elapsed());
    {
        // Persist the seeded genesis so recovery has the same starting state
        // a real lane would get from its genesis file.
        let (c, n, t) = *node.head.lock().await;
        node.state.lock().await.write_snapshot(&node.cfg.datadir, c, n, t)?;
    }

    // -- rpc + driver ---------------------------------------------------------
    let (addr, handle) = rpc::serve(node.clone(), ([127, 0, 0, 1], 0).into()).await?;
    let client = HttpClientBuilder::default()
        .max_request_size(64 * 1024 * 1024)
        .build(format!("http://{addr}"))?;
    let driver = node.clone();
    let window: Arc<std::sync::Mutex<(Option<Instant>, Option<Instant>)>> =
        Arc::new(std::sync::Mutex::new((None, None)));
    let win = window.clone();
    let drv = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(block_ms));
        loop {
            tick.tick().await;
            if let Ok(Some(_)) = driver.produce_block(now_ms()).await {
                let mut w = win.lock().unwrap();
                let now = Instant::now();
                if w.0.is_none() {
                    w.0 = Some(now);
                }
                w.1 = Some(now);
            }
        }
    });

    // -- submit through the real batch endpoint -------------------------------
    let t_submit = Instant::now();
    for chunk in raws.chunks(2000) {
        let b64 = rpc::encode_batch(chunk);
        let r: serde_json::Value = client.request("arc_sendRawTxBatch", rpc_params![b64]).await?;
        if r["rejected"].as_u64().unwrap_or(0) > 0 {
            eprintln!("batch rejects: {r}");
        }
    }
    let submit_elapsed = t_submit.elapsed();

    // -- drain ----------------------------------------------------------------
    loop {
        tokio::time::sleep(Duration::from_millis(2 * block_ms)).await;
        let s: serde_json::Value = client.request("txpool_status", rpc_params![]).await?;
        if s["pending"].as_u64() == Some(0) && s["queued"].as_u64() == Some(0) {
            break;
        }
    }
    drv.abort();
    handle.stop().ok();

    // -- report ---------------------------------------------------------------
    let s = &node.stats;
    let outputs = s.outputs.load(Ordering::Relaxed).max(1);
    let txs = s.txs.load(Ordering::Relaxed).max(1);
    let blocks = s.blocks.load(Ordering::Relaxed).max(1);
    let admitted = s.admitted.load(Ordering::Relaxed).max(1);
    let log_bytes = node.log.lock().await.bytes_written;
    let per_out = |ns: u64| ns as f64 / 1000.0 / outputs as f64;
    let (w0, w1) = *window.lock().unwrap();
    let active_s = match (w0, w1) {
        // first->last block, plus one cadence for the first block's fill
        (Some(a), Some(b)) => (b - a).as_secs_f64() + block_ms as f64 / 1000.0,
        _ => 1.0,
    };
    println!();
    println!("=== lean-lane-node bench ===");
    println!("txs {txs} ({} noops)  blocks {blocks}  outputs {outputs}  budget {}M gas  cadence {block_ms}ms",
        s.noops.load(Ordering::Relaxed), node.cfg.budget_gas / 1_000_000);
    println!("SUSTAINED: {:.0} outputs/s  ({:.0} tx/s) over {:.1}s active window (first->last block)",
        outputs as f64 / active_s, txs as f64 / active_s, active_s);
    println!("submit: {} txs in {:?} through arc_sendRawTxBatch ({:.1}k tx/s offered)",
        admitted, submit_elapsed, admitted as f64 / submit_elapsed.as_secs_f64() / 1000.0);
    println!("per-output µs: admission {:.2} (per tx {:.2})  build {:.3}  execute {:.3}  append+fsync {:.3}",
        s.admit_ns.load(Ordering::Relaxed) as f64 / 1000.0 / outputs as f64,
        s.admit_ns.load(Ordering::Relaxed) as f64 / 1000.0 / admitted as f64,
        per_out(s.build_ns.load(Ordering::Relaxed)),
        per_out(s.exec_ns.load(Ordering::Relaxed)),
        per_out(s.append_ns.load(Ordering::Relaxed)));
    println!("log: {} bytes total = {:.1} B/output  ({:.1} B/tx)",
        log_bytes, log_bytes as f64 / outputs as f64, log_bytes as f64 / txs as f64);

    // -- recovery -------------------------------------------------------------
    let digest_live = node.state.lock().await.digest();
    let cfg2 = Config { datadir: dir.clone(), ..Default::default() };
    let t_rec = Instant::now();
    let (node2, rec) = LaneNode::open(cfg2)?;
    let rec_elapsed = t_rec.elapsed();
    let digest_rec = node2.state.lock().await.digest();
    println!("recovery: snapshot@{} + {} blocks / {} txs replayed in {:?}  state {}",
        rec.snapshot_number, rec.replayed_blocks, rec.replayed_txs, rec_elapsed,
        if digest_rec == digest_live { "MATCHES" } else { "MISMATCH!!" });
    if digest_rec != digest_live {
        eyre::bail!("recovery digest mismatch");
    }
    println!("(baselines: type-2 ecrecover ~33µs/tx serial, persist ~44µs/tx, 122 B/tx)");
    Ok(())
}

fn tempdir_like(explicit: String) -> std::path::PathBuf {
    if !explicit.is_empty() {
        return explicit.into();
    }
    let p = std::env::temp_dir().join(format!("lean-lane-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}
