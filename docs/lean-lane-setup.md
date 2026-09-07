# Running the lean payment lane on a fresh machine

The lean lane is **in this repo** — `crates/lean-native` (wire format + pool
integration) and `crates/lean-lane-node` (the node). They build against upstream
reth `v2.3.0` like the rest of the workspace, so **no reth fork is required**.
`~/reth-fork` remains only as the experiment history; nothing here depends on it.

## What a fresh machine needs

| need | how |
|---|---|
| Rust (per `rust-toolchain.toml`) | rustup |
| clang + libclang-dev | reth-mdbx bindgen (the EVM lane; the lean node itself has no C deps) |
| Docker | EVM lane CL/EL images (`make build-docker`) |
| Foundry (`.foundry-version`) | `cast`, used by governance + fund-file generation |
| Node ≥20.19 / 22.x (even) | genesis step only |

## Build (one command each)

```bash
cargo build --release -p lean-lane-node     # the lean lane node  (~1 min warm, ~10 min cold)
cargo build --release -p spammer            # load generator
make build-docker                           # EVM lane CL+EL images (long)
```

## Genesis funding

The node funds a fixed address set at genesis (`--fund-file`). It must be exactly
the accounts the spammer signs with — derivation `m/44'/60'/1'/0/{i}` from the
standard test mnemonic. Note the **`1'`**, not the usual `0'`.

```bash
./experiments/dual-el/gen-lean-fund.sh 800 ~/lean-fund.txt
```

## Run a node

```bash
./target/release/lean-lane-node run \
  --datadir ~/lean-lane/val1 --port 8560 --bind 0.0.0.0 --chain-id 1338 --shim \
  --peers http://<peer2>:8560,http://<peer3>:8560 \
  --fund-file ~/lean-fund.txt --fund-balance 10000000000000000000
```

`--shim` means the CL drives block production (`arc_buildBlock` / `arc_newBlock`);
without it the node self-drives, which is only useful for standalone benchmarks.
`--peers` is the gossip/backfill mesh — see the shim contract in
`docs/lean-lane-integration.md`.

## Point a CL at it

Per-validator environment (default off — without these the CL is byte-identical
to stock):

```
ARC_PAYMENT_LEAN_LANE=1
ARC_PAYMENT_LEAN_RPC=http://<this machine's lean node>:8560
ARC_PAYMENT_LEAN_BUDGET_GAS=150000000
ARC_PAYMENT_LEAN_PEER_RPCS=http://<all four lean nodes>:8560,...
```

Boot order matters: **start the lean node before the CL**. A CL that boots while
its lean node is down parks in "Manual intervention required" and never proposes
again (it costs ~25% of cadence and is invisible to agreement checks).

## Benchmark

```bash
./experiments/dual-el/lane-bench.sh lean 150000000 100 600   # lean lane, N=100, 10-min window
./experiments/dual-el/lane-bench.sh evm  75000000            # EVM lane
```

Defaults to the repo-local binary and `~/lean-fund.txt`; override with `LEAN_BIN`
and `FUND`. It carries the measurement protocol (container census, pool wipe
before CL recreate, chain-static check, corpus probe, per-machine corpus
generation, remote-effect verification, CL health gate, fullness on every row) —
extend this script rather than hand-rolling per-experiment shell.

## Verify a fresh machine in one command

```bash
./experiments/dual-el/lean-smoke.sh        # ~1 min
```

Boots three lean nodes on loopback, drives them with a stand-in for the CL
(round-robin `arc_buildBlock` → `arc_newBlock` on all three), feeds real signed
fan-out transactions, and asserts all three converge on one commitment at the
expected height with non-empty blocks. No docker, no fleet, no fork. If this
passes, the lane works on that machine.

## Multi-machine

```bash
cp experiments/dual-el/fleet.env.example experiments/dual-el/fleet.env   # edit hosts/IPs
./experiments/dual-el/deploy-lean.sh --check    # what is stale where
./experiments/dual-el/deploy-lean.sh            # build + ship, sha-verified
```

`deploy-lean.sh` ships the lean binary, the spammer, `lean-feeder.py` and the
fund file to every host in `fleet.env`, verifying each by **sha after transfer**
— tailscale ssh returns success when its session check has expired, so exit
codes cannot be trusted. `lane-bench.sh` reads the same file for topology.

## Still machine-specific

The historical fleet launchers under `experiments/dual-el/fleet/`
(`demo-fleet*.sh`, `gen-fleet.py`, …) still hardcode this fleet's hostnames and
`/home/papaduck` paths; they are kept as campaign history. The live path —
`lean-smoke.sh`, `deploy-lean.sh`, `lane-bench.sh`, `gen-lean-fund.sh` — is
parameterised.
