# Setup notes

## Fresh machine

| need | why |
|---|---|
| Rust per `rust-toolchain.toml` (rustup) | build |
| Foundry `cast` | `scripts/gen-lean-fund.sh` derives the funded accounts |
| python3 | `scripts/lean-smoke.sh`, `lean-feeder.py`, `lean-replay-bench.py` |

No clang/libclang, no docker: the node has no C dependencies.

```bash
cargo build --release -p lean-lane-node -p spammer
./scripts/lean-smoke.sh            # proves the lane works on this machine (~1 min)
```

## Multi-machine

One lean node per validator, each with `--peers` pointing at every other lean
node, all started from the **same fund file** (genesis is a function of chain id
+ fund file). Then point each validator's CL at its local node
(`docs/integration.md` §3 lists the Arc environment variables):

```
ARC_PAYMENT_LEAN_LANE=1
ARC_PAYMENT_LEAN_RPC=http://127.0.0.1:8560
ARC_PAYMENT_LEAN_BUDGET_GAS=150000000
ARC_PAYMENT_LEAN_PEER_RPCS=http://<node2>:8560,http://<node3>:8560,...
```

Enable on all validators together, from height 1 of a fresh chain.

## Tools

- `scripts/gen-lean-fund.sh [count] [outfile]` — funded address list
  (`m/44'/60'/1'/0/i` from the standard test mnemonic; note the `1'`).
- `scripts/lean-feeder.py` — feeds a pre-generated signed corpus at a fixed
  rate (used for measured runs so signing never bounds ingress).
- `scripts/lean-replay-bench.py SRC_RPC TGT_RPC` — replays a live node's
  canonical blocks into a fresh node and reports pure import throughput.

## Measured

Fleet numbers, and the runner and protocol that produced them, live in
`bench/README.md`. Pure import (no consensus) is ~1.0 M outputs/s; execution
is 7–15 % of a height, consensus transport is the bound.
