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

## Verify a machine in one command

```bash
./scripts/lean-smoke.sh          # ~1 min; needs rust + foundry's `cast`
```

Builds the node and the spammer, boots three nodes on loopback, drives them with
a stand-in consensus layer (round-robin `arc_buildBlock` → `arc_newBlock`),
feeds real signed fan-out transactions, and asserts all three converge on one
commitment at the expected height with non-empty blocks. Then replays the chain
into a fresh node with parallel signature recovery and asserts the same head.

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

| flag | default | meaning |
|---|---|---|
| `--shim` | off | the consensus layer drives block production; without it the node self-drives every `--block-ms` (standalone benchmarks only) |
| `--peers` | none | other lean nodes — gossip on append, and the backfill source when this node is behind |
| `--fund-file` / `--fund-balance` | none / 1e15 | addresses funded at genesis; must be exactly the accounts the spammer signs with (derivation `m/44'/60'/1'/0/i` — note the `1'`) |
| `--budget-gas` | 150,000,000 | self-drive block budget (the shim passes its own per call) |
| `--snapshot-every` | 1024 | blocks between state snapshots (restart = snapshot + replay) |
| `--chain-id` | 1338 | |

Genesis is a pure function of chain id + fund file, so every node in a fleet
must be started with the same fund file.

`LEAN_PARALLEL_RECOVERY=0` turns off rayon-parallel signature recovery
(default on; `=0` is the measured-slower serial path).

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
