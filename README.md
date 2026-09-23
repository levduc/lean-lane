# lean-lane — a lean payment lane for Arc

A self-contained execution node for high-volume native payments that runs
*beside* an Arc EVM node under the same Malachite BFT consensus. No reth fork:
the workspace builds against upstream reth `v2.3.0`.

| crate | what |
|---|---|
| `crates/lean-native` | the fan-out transaction wire format (`0x50`, one signature, N outputs, 72 + 28N bytes) and its signing (no reth dependency) |
| `crates/lean-lane-node` | the node: the reth pool integration (`ArcPooledTx`), flat nonce+balance state, keccak commitment chain, append-only log + snapshots, gossip/backfill self-healing, and the JSON-RPC shim the consensus layer drives |
| `crates/spammer` | load generator (EVM transfers and lean fan-out, weighted mixes) |

The node has **no Ethereum header, no state trie, no forkchoice**. A block is
`{parent, number, timestamp_ms, txs}` and its commitment is a keccak chain over
the framed bytes. The consensus layer treats the block bytes as opaque, checks
structure, and anchors execution at decide — see `docs/integration.md` for the
exact contract.

## Get it

```bash
git clone git@github.com:levduc/lean-lane.git
```

| branch | what |
|---|---|
| `main` (tag `v0.1.0`) | the by-bytes shim contract (`arc_newBlock{blockBytes}`) that the Arc CL series `lean-lane-v0.1` (tag `lean-lane-v0.1.0`) drives |
| `v0.2` | adds the commitment-addressed verbs (`arc_newBlock{commitment}`, `arc_getBlockBytes{commitment}`) the Arc CL v0.2 series uses, plus the performance work measured on the 4-validator fleet: no log scan on the serving path, the proposer stages its own block, no re-pull of held blocks, the anchor waits for an in-flight or not-yet-arrived stage, one commit gate per append |
| `v0.3-dev` | cleanup on top of `v0.2`, same shim contract: commitment index complete from log replay (no log scan at all), every setting is a CLI flag, diagnostics through `tracing`, no self-drive mode, no `bench` subcommand, no receipt ring |

The shim contract — what a consensus layer must call and what it gets back —
is `docs/integration.md` in this repo.

## Verify a machine in one command

```bash
./scripts/lean-smoke.sh          # ~1 min; needs rust + foundry's `cast`
```

Builds the node and the spammer, boots three nodes on loopback, drives them with
a stand-in consensus layer (round-robin `arc_buildBlock` → `arc_newBlock`),
feeds real signed fan-out transactions, and asserts all three converge on one
commitment at the expected height with non-empty blocks. Then replays the chain
into a fresh node and asserts the same head.

Last run on this repo: `40 heights · 9856 transactions · 98560 payments · avg
N=10.0 · 3/3 nodes at one commitment`.

## Build

```bash
cargo build --release -p lean-lane-node -p spammer
```

`rust-toolchain.toml` pins the toolchain; `Cargo.lock` is pinned to the exact
dependency set the lane was measured with (newer alloy releases require a newer
rustc than the pin — keep the lock).

## Run a node

```bash
./scripts/gen-lean-fund.sh 800 ~/lean-fund.txt      # genesis-funded accounts (needs `cast`)

./target/release/lean-lane-node run \
  --datadir ~/lean-lane-data/val1 --port 8560 --bind 0.0.0.0 --chain-id 1338 \
  --peers http://<peer2>:8560,http://<peer3>:8560 \
  --fund-file ~/lean-fund.txt --fund-balance 10000000000000000000
```

Every setting is a flag (`lean-lane-node run --help`); three of them also
read an environment variable as a fallback, shown in the table.

| flag | default | meaning |
|---|---|---|
| `--datadir` | `lean-lane-data` | block log + state snapshots |
| `--port` / `--bind` | 8560 / 127.0.0.1 | JSON-RPC listen address |
| `--chain-id` | 1338 | lane chain id: signing domain and genesis commitment |
| `--peers` | none | comma-separated lean-node RPC urls: announce targets on append, and the backfill source when this node is behind |
| `--fund-file` / `--fund-balance` | none / 1e15 wei | addresses funded at genesis (fresh datadir only); must be exactly the accounts the spammer signs with (derivation `m/44'/60'/1'/0/i` — note the `1'`) |
| `--snapshot-every` | 1024 | blocks between state snapshots (restart = snapshot + replay); `0` never snapshots, so a restart replays the whole log from genesis |
| `--no-fsync` | off (fsync every append) | skip the per-append fsync of the block log (see below) |
| `--stats` (env `LEAN_STATS=1`) | off | one `lean_stats` line (counters and sizes) at info every 10 s |
| `--announce-grace-ms` (env `LEAN_ANNOUNCE_GRACE_MS`) | 250 | how long an announcement for head+1 that this node does not hold waits for its own CL's anchor before the backfill pulls it; `0` pulls at once |
| `--anchor-grace-ms` (env `LEAN_ANCHOR_GRACE_MS`) | 60 | how long an `arc_newBlock{commitment}` that holds none of the block waits for a stage that has not arrived yet before pulling from a peer; `0` disables |

The consensus layer always drives the chain (`arc_buildBlock` /
`arc_newBlock`); there is no self-driving mode, so `--shim` is accepted and
ignored, and `--block-ms`, `--budget-gas`, `--receipt-ring` and the `bench`
subcommand are gone, as is `eth_getTransactionReceipt`. The old `LEAN_FSYNC`,
`LEAN_STAGE_RACE_MS`, `LEAN_PARALLEL_RECOVERY` and `ARC_LEAN_SHIM` variables
are gone too (the node warns if one is set). Signature recovery is always
rayon-parallel.

Genesis is a pure function of chain id + fund file, so every node in a fleet
must be started with the same fund file.

`--no-fsync` drops the per-append fsync on the block log. The log tail can
then be lost in a crash — safe, because the head is re-derived from the log on
open and the backfill re-pulls whatever is missing above it. Note the
asymmetry: state snapshots are fsync'd either way, so a crash can leave a
snapshot *ahead* of the log; `open` detects that, warns (`lean_recovery
WARNING ...`), and restarts from the newest snapshot the log can be continued
from rather than resuming above a hole it could never fill.

Logging goes through `tracing` to stdout; `RUST_LOG` sets the filter
(default `warn,lean_lane_node=info`). `--stats` logs one line at info every
10 s:

```
lean_stats head=812 accounts=44913 staged=1 sync_queue=0 by_commitment=812 pending=1974 queued=0 rss_mb=1842 blocks=812 txs=... outputs=... noops=0 promoted=790 admitted=... rejected=0 anchor_index=12 anchor_staged=790 anchor_queued=0 anchor_peer=10 anchor_syncing=0 anchor_bytes=0 stage_race_waits=3 anchor_grace_hits=40 anchor_grace_misses=2 block_pulls=10 backfill_pulls=0 announces_rx=2436 announces_known=1600 announce_skipped_staged=836
```

Everything in it is a counter or a `len()` (it never walks the state, the log
or the pool). `anchor_*` count `arc_newBlock` calls by the route they took
(`staged` is the healthy validator's path; `peer` means the block was pulled
back over HTTP); `anchor_grace_hits/misses` say whether the cold anchor grace
pays for itself. Per-call timing (`lean_anchor`, `lean_stage`, `lean_locate`,
`lean_build`) is emitted as debug events: `RUST_LOG=lean_lane_node=debug`.

## Load

```bash
./target/release/spammer ws --targets ws://127.0.0.1:8560 -r 2000 -g 2 -a 200 \
  -t 12 --chain-id 1338 --mix fanout=100 --fanout-outputs 10
# weighted mix of fan-out widths:
#   --fanout-outputs "1:20,5:20,10:20,50:20,100:20"
```

Fan-out drains sender balances one way; refund by regenerating genesis.

## Bench the execution engine alone

```bash
python3 scripts/lean-replay-bench.py SRC_RPC TGT_RPC   # import rate from a live chain
```

## Docs

- `docs/integration.md` — the shim contract (the five verbs, block bytes,
  commitment formula, SYNCING semantics) and what the consensus layer does at
  each phase. This is the plug-and-play boundary.
- `docs/setup.md` — fresh-machine and multi-machine notes.
