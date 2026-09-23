# lean-lane — a lean payment lane for Arc

A self-contained execution node for high-volume native payments that runs
*beside* an Arc EVM node under the same Malachite BFT consensus. No reth fork:
the workspace builds against upstream reth `v2.3.0`.

| crate | what |
|---|---|
| `crates/lean-native` | the fan-out transaction wire format (`0x50`, one signature, N outputs, 72 + 28N bytes) and its pool integration |
| `crates/lean-lane-node` | the node: flat nonce+balance state, keccak commitment chain, append-only log + snapshots, gossip/backfill self-healing, and the JSON-RPC shim the consensus layer drives |
| `crates/spammer` | load generator (EVM transfers and lean fan-out, weighted mixes) |

The node has **no Ethereum header, no state trie, no forkchoice**. A block is
`{parent, number, timestamp_ms, txs}` and its commitment is a keccak chain over
the framed bytes. The consensus layer treats the block bytes as opaque, checks
structure, and anchors execution at decide — see `docs/integration.md` for the
exact contract.

## Get it

Single branch `main`, tag `v0.1.0`:

```bash
git clone git@github.com:levduc/lean-lane.git
```

Branch `v0.2` carries the commitment-addressed verbs (`arc_newBlock{commitment}`, `arc_getBlockBytes{commitment}`) the Arc CL series `lean-lane-v0.2` uses, plus the 2026-09-15 performance fixes (no log scan on the serving path, the proposer stages its own block, no re-pull of held blocks, the anchor waits for an in-flight stage, one commit gate per append). The anchor also waits a short grace (`LEAN_ANCHOR_GRACE_MS`, default 60 ms, `0` disables) for a stage that has not ARRIVED yet: on a LAN the CL's tiny `arc_newBlock{commitment}` beats its own fire-and-forget `arc_stageBlock`, whose task is still encoding a 2.5 MB body, and the anchor was paying a 200-250 ms peer pull for a block landing milliseconds later (fleet, 450 M budget: `path=peer` on 515 of 1,315 anchors). Optional diagnostics, all off by default: `LEAN_STATS=1` (stats every 10 s + per-call anchor/stage/locate lines, each stamped `t_in=<ms since node start>` so the stage-vs-anchor ARRIVAL gap is measurable offline), `LEAN_FSYNC=0`, `LEAN_ANNOUNCE_GRACE_MS`, `LEAN_STAGE_RACE_MS`.

The matching Arc consensus-layer changes are the `lean-lane-v0.1` branch of
`levduc/arc-node` (tag `lean-lane-v0.1.0`); its
`docs/lean-lane-integration.md` explains how the two fit together.

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
  --datadir ~/lean-lane-data/val1 --port 8560 --bind 0.0.0.0 --chain-id 1338 --shim \
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
| `--shim` | off | the consensus layer drives block production; without it the node self-drives every `--block-ms` (standalone benchmarks only) |
| `--block-ms` / `--budget-gas` | 250 / 150,000,000 | self-drive cadence and block budget (the shim passes its own budget per call) |
| `--peers` | none | comma-separated lean-node RPC urls: announce targets on append, and the backfill source when this node is behind |
| `--fund-file` / `--fund-balance` | none / 1e15 wei | addresses funded at genesis (fresh datadir only); must be exactly the accounts the spammer signs with (derivation `m/44'/60'/1'/0/i` — note the `1'`) |
| `--snapshot-every` | 1024 | blocks between state snapshots (restart = snapshot + replay); `0` never snapshots, so a restart replays the whole log from genesis |
| `--receipt-ring` | 200,000 | most-recent receipts kept in memory for `eth_getTransactionReceipt`; `0` keeps none |
| `--no-fsync` | off (fsync every append) | skip the per-append fsync of the block log (see below) |
| `--stats` (env `LEAN_STATS=1`) | off | one `lean_stats` line every 10 s plus per-call anchor/stage/locate timing lines |
| `--announce-grace-ms` (env `LEAN_ANNOUNCE_GRACE_MS`) | 250 | how long an announcement for head+1 that this node does not hold waits for its own CL's anchor before the backfill pulls it; `0` pulls at once |
| `--anchor-grace-ms` (env `LEAN_ANCHOR_GRACE_MS`) | 60 | how long an `arc_newBlock{commitment}` that holds none of the block waits for a stage that has not arrived yet before pulling from a peer; `0` disables |

The old `LEAN_FSYNC`, `LEAN_STAGE_RACE_MS`, `LEAN_PARALLEL_RECOVERY` and
`ARC_LEAN_SHIM` variables are gone (the node warns if one is set). Signature
recovery is always rayon-parallel.

Genesis is a pure function of chain id + fund file, so every node in a fleet
must be started with the same fund file.

`--receipt-ring` is pure RSS, and the price scales with fan-out: receipts are
derived, never persisted, and nothing but `eth_getTransactionReceipt` reads
them. One receipt is 96 B plus 64 B per non-zero output — ~160 B at N=1 but
~6.4 KB at N=100, so the 200,000 default costs ~32 MB on single payments and
**~1.3 GB** at N=100. A validator under a fan-out load that nobody queries
receipts from should set it well below the default (or to `0`).

`--no-fsync` drops the per-append fsync on the block log. The log tail can
then be lost in a crash — safe, because the head is re-derived from the log on
open and the backfill re-pulls whatever is missing above it. Note the
asymmetry: state snapshots are fsync'd either way, so a crash can leave a
snapshot *ahead* of the log; `open` detects that, warns (`lean_recovery
WARNING ...`), and restarts from the newest snapshot the log can be continued
from rather than resuming above a hole it could never fill.

`--stats` prints one line every 10 s on stdout:

```
lean_stats head=812 accounts=44913 staged=1 sync_queue=0 by_commitment=812 pool=1974 build_ms_last=12.9 stage_ms_last=21.6 append_ms_last=9.8 rss_mb=1842
```

Everything in it is either a counter or a `len()` (it never walks the state,
the log or the pool). `head`/`accounts`/`rss_mb` answer "what grew";
`*_ms_last` are the last block's build / stage / append service times, which
is what a drifting cadence needs to attribute.

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
./target/release/lean-lane-node bench --txs 200000 --mix 1,10,100
python3 scripts/lean-replay-bench.py   # import rate from a recorded log
```

## Docs

- `docs/integration.md` — the shim contract (the five verbs, block bytes,
  commitment formula, SYNCING semantics) and what the consensus layer does at
  each phase. This is the plug-and-play boundary.
- `docs/setup.md` — fresh-machine and multi-machine notes.
