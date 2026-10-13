# Native-transfer benchmark: lean lane vs reth

This benchmark compares how fast two engines process blocks of native transfers.
Consensus is not part of it.

- **lean**: the lean payment lane engine (`crates/lean-core`), through
  `lean-bench native`.
- **reth**: the latest reth release, **v2.7.0**, through a small standalone
  program (`reth-side/`) that calls reth's own crates.

Both sides get the same workload and the same thread budget. One script builds
both, runs both, prints a table and writes a JSON file.

## Run it

```bash
bench/native-transfer/run.sh              # full run: 1M accounts, 20k txs/block, 1+5 blocks
QUICK=1 bench/native-transfer/run.sh      # quick run: 100k accounts, 1+3 blocks
ACCOUNTS=1000000 TXS=20000 BLOCKS=5 THREADS=8 bench/native-transfer/run.sh
bench/native-transfer/run.sh --accounts 100000 --txs 10000 --blocks 3 --threads 4
```

| setting | default | meaning |
|---|---|---|
| `ACCOUNTS` / `--accounts` | 1,000,000 (QUICK: 100,000) | pre-funded accounts |
| `TXS` / `--txs` | 20,000 | transactions per block |
| `BLOCKS` / `--blocks` | 5 (QUICK: 3) | measured blocks |
| `WARMUP` / `--warmup` | 1 | warm-up blocks, not in the result |
| `THREADS` / `--threads` | physical cores | thread budget of both sides |
| `BENCH_TMPDIR` / `--tmpdir` | `bench/native-transfer/.tmp` | reth datadirs and the lean block log |
| `SKIP_BUILD=1` | off | use the binaries that are already built |
| `NO_VERIFY_ROOT=1` | off | skip reth's final full state-root recompute |

You need `rustup` (each side pins its Rust version; the script installs it),
`clang`, `libclang` and a C++ compiler (reth builds MDBX and RocksDB from C/C++).
On Debian or Ubuntu: `sudo apt-get install -y clang libclang-dev build-essential`.
On macOS: `xcode-select --install`. The first reth build takes some minutes.
A full run takes under a minute after the build and needs about 2 GB of RAM.

Put `BENCH_TMPDIR` on the disk you want to measure. Do not use a tmpfs: the
persist and fsync numbers then measure RAM. The script deletes its temporary
files when it stops.

## The workload (N = 1)

Each transaction makes one payment and carries one signature.

- `ACCOUNTS` accounts exist at genesis, each with 1,000,000 ETH.
- Each block has `TXS` transactions from `TXS` different senders. Sender `t`
  sends transaction `t` of every block, so each block raises each sender's
  nonce by 1.
- The recipient of each transaction is uniform over all accounts. The value is
  1 gwei. Both sides use the same keys, recipients and order.
- The blocks are consecutive. Each block runs on the state after the block
  before it.
- Signing is never timed. All blocks are built before the clock starts.

lean transaction: type `0x50`, one output, 100 bytes.
reth transaction: signed EIP-1559 transfer, 21,000 gas, 115 bytes. The chain
has all forks up to Osaka active from genesis, and the four system contracts
(EIP-4788, 2935, 7002, 7251) at genesis, as on a real chain. The base fee
starts at 7 wei and follows EIP-1559.

## What is measured

All numbers are milliseconds per block and transactions per second (= payments
per second, because N = 1). Each number is the median of the measured blocks.

**lean** (`lean-bench native`):

| row | what it is |
|---|---|
| frame + decode | block framing, block commitment hash, transaction decode |
| recover (parallel) | signature recovery on the rayon pool |
| execute | the state transition, against the in-memory state |
| apply | write the result into the in-memory state |
| **decode + recover + execute + apply** | `lean_core::process_block` + `apply`: the engine's cost of one block (headline) |
| block-log append + fsync | the node's block-log write, with `fsync`, in `BENCH_TMPDIR` |

**reth, direct path**: reth's own functions, one phase after the other, each
block on the state that phase 4 of the block before it wrote to disk:

| row | what it is |
|---|---|
| 1. recover senders | `recover_signers` on the rayon pool |
| 2. execute | `EthEvmConfig::executor(..).execute(block)` (`BasicBlockExecutor`) on the latest state in MDBX |
| 3. state root | `hashed_post_state` + `state_root_with_updates`: the incremental root against the 1M-account trie in MDBX. This is reth's serial state-root code (the engine uses it as its fallback). |
| 4. persist | `save_blocks` + `commit`, as reth's persistence service does: MDBX, static files and RocksDB (storage v2, the default for new nodes), MDBX in durable sync mode |
| **recover + execute** | 1 + 2. Compare this with the lean headline: neither has a Merkle root or a disk write. |
| **full import** | 1 + 2 + 3 + 4: what reth pays for one block, phase by phase |
| (info) tx root + receipts root + bloom | roots that a node recomputes to check a block. Not in any other row. |

**reth, engine path**: the same finished blocks go through reth's engine tree
(`EngineApiTreeHandler` with `BasicEngineValidator` and the default
`TreeConfig`), as in-process `reth_newPayload` and `forkchoiceUpdated`
messages. This is how a running reth node imports a block: it decodes the
payload, recovers senders in parallel while it executes, prewarms, uses its
cross-block caches, computes the state root in a parallel sparse-trie task
beside execution, and checks the result. The engine accepts a block only if its
state root, receipts root and gas used match the header that the direct path
made. So this pass also checks the direct path.

| row | what it is |
|---|---|
| **engine newPayload** | server-side latency of `reth_newPayload` (recover, execute, state root, checks, overlapped). As `reth-bench` does, each call starts after the previous block's background work. |
| engine forkchoiceUpdated | make the block canonical |
| engine newPayload + persist (4.) | newPayload plus the direct path's per-block persist |

The engine keeps new blocks in memory. It writes them to disk later, in
batches of up to about 45 blocks, in a background thread. No block of a short
run reaches the disk on this path.

## Correctness checks

The run fails if one check fails.

- lean: zero no-op transactions; every transaction applied; every sender's
  nonce = number of blocks; every touched account's balance matches a model;
  the sum of all balance changes is zero (fees go to the beneficiary).
- reth: every receipt successful; gas used = 21,000 x transactions; nonces and
  balances of every touched account match a model, on both paths; the sum of
  all balance changes equals minus the burned base fee; a full state-root
  recompute from the hashed state matches the last incremental root; the engine
  accepts every block as VALID.

## Read the result

The script prints one table, then the ratios of the lean headline to three reth
rows. The JSON file in `results/<host>-<date>-<time>.json` has the parameters,
the hardware, the software versions, every phase of every block, and the check
results.

Use these pairs:

- **lean headline vs reth "recover + execute"**: the same work (check, recover,
  run, compute the new state), no Merkle root, no disk writes.
- **lean headline vs reth "engine newPayload"**: reth with all its
  optimizations, but with its state root (lean has none) and without disk
  writes.
- **lean headline + log fsync vs reth "full import"**: what each engine must do
  before a block is final and durable in its own design.

## Reference result

Host: i7-11700F (8 cores / 16 threads), 62 GB, Ubuntu 24.04, kernel 7.0, NVMe
SSD (ext4). 1,000,000 accounts, 20,000 txs/block, 1 warm-up + 5 measured
blocks, 8 threads, commit `4d9b6be`. Two runs, back to back
(`results/ginny-alienware-20261013-144733.json`, `-144750.json`). All checks
passed in both runs.

| side | row | run 1 ms | run 1 tx/s | run 2 ms | run 2 tx/s |
|---|---|---:|---:|---:|---:|
| lean | recover (parallel) | 82.50 | 242,425 | 82.50 | 242,415 |
| lean | execute | 2.25 | 8,884,857 | 2.39 | 8,373,113 |
| lean | **decode + recover + execute + apply** | **88.26** | **226,595** | **87.80** | **227,803** |
| lean | block-log append + fsync | 4.08 | 4,898,286 | 4.06 | 4,930,316 |
| reth | 1. recover senders | 82.79 | 241,574 | 82.79 | 241,587 |
| reth | 2. execute | 77.86 | 256,879 | 78.54 | 254,659 |
| reth | 3. state root | 341.67 | 58,537 | 336.58 | 59,421 |
| reth | 4. persist | 151.58 | 131,945 | 151.22 | 132,258 |
| reth | **recover + execute** | **160.93** | **124,278** | **161.34** | **123,962** |
| reth | **full import** (1+2+3+4) | **662.83** | **30,174** | **651.62** | **30,693** |
| reth | **engine newPayload** | **196.83** | **101,609** | **199.03** | **100,486** |
| reth | engine newPayload + persist | 348.41 | 57,403 | 350.25 | 57,102 |

What this shows, on this host:

- Signature recovery costs the same on both sides (about 82.5 ms per 20,000
  signatures on 8 threads). It is 94 % of the lean block.
- Without recovery, lean executes and applies a block in about 3 ms; reth's EVM
  execution takes about 78 ms.
- reth's serial state root (about 340 ms for 40,000 changed accounts in a
  1M-account trie) and its per-block persist (about 150 ms) are the largest
  reth phases. reth's engine hides most of the state root behind execution:
  newPayload takes about 197 ms.
- Ratios (lean headline / reth row): 1.8x vs recover + execute, 2.2x vs engine
  newPayload, 7.4-7.5x vs the direct full import.

## What is not measured

- No consensus, no networking, no mempool, no RPC. No JSON encoding.
- No block building. Both sides only import blocks.
- lean keeps its state in memory as a flat map. It has no Merkle trie, by
  design: the lane commits to blocks, not to state. The lean node makes the
  state durable through its block log and periodic snapshots; this benchmark
  times only the block-log append.
- reth keeps its state in MDBX with a Merkle Patricia trie. The OS page cache
  is warm (the genesis was just written).

## Fairness caveats

- **Signature recovery dominates the lean number.** Both sides use the same
  library (libsecp256k1) and the same threads, so recovery costs about the same
  on both sides. A real reth node often skips this cost: it recovers the
  senders of mempool transactions when they arrive and reuses them. The lean
  node recovers every transaction in the block.
- **reth's direct path is serial by design.** It shows the cost of each phase.
  A real node overlaps recovery, execution and the state root (see the engine
  path), so "full import" is an upper bound for one block.
- **Persist is per block on the direct path.** A real reth node persists in
  background batches, which costs less per block and runs beside execution.
- **The engine path uses more threads.** Its runtime has several pools
  (prewarm, proof workers, sparse trie). The CPU pool is set to `THREADS`, but
  other pools may add threads. lean uses one rayon pool of `THREADS` threads.
- **Allocators differ.** reth runs on jemalloc, as the reth binary does. lean
  uses the system allocator, as the lean node does.
- **Transaction formats differ.** reth runs full EVM semantics (EIP-1559 fees,
  system contracts, receipts). lean runs a payment-only state transition.
  That is the point of the lane, but it is not the same work.
- **Small samples.** Each run has 5 measured blocks. Run the script more than
  once. Treat differences under about 10 % as noise.
- The reth side is a harness around reth's public crates, not the reth binary.
  It uses reth's tempdir provider factory (`test-utils` feature) with the
  production MDBX settings.

## Files

- `run.sh`: the one script.
- `reth-side/`: standalone cargo project against reth `v2.7.0` (git tag), with
  its own `Cargo.lock` and Rust version. It is not part of the lean-lane
  workspace, which pins reth v2.3.0.
- `../../crates/lean-bench/src/native.rs`: the lean side (`lean-bench native`).
- `results/`: result files.
