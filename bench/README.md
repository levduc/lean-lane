# bench — measuring the lean lane on a real fleet

Tooling to run the lean payment lane (and, for comparison, the plain EVM lane)
under the Arc CL on several machines, drive it with load, and read the result
without fooling yourself. None of it is needed to *run* the lane — see the
top-level README for that.

| tool | what it does |
|---|---|
| `fleet/fleet` | the runner: `up`, `load`, `status`, `health`, `logs`, `set-gas-limit`, `restart-cl`, `kill-lean`, `down`, `purge`; `--dry-run` prints every command instead of running it |
| `fleet/lib.sh` `up.sh` `load.sh` `ops.sh` | the runner's parts: config + remote exec + shipping + preflight; render/ship/boot/health gate; load; status, fault legs, logs, teardown |
| `fleet/sampler.py` | the per-minute sampler `load` runs (cadence, fullness, tx/s, payments/s, pools, CL restarts, CPU, `lean_stats`), and the one set of JSON-RPC parsers the shell side also uses |
| `fleet/split-compose.py` | turns quake's one-machine compose into validator1-here + one compose per remote validator |
| `fleet/fleet4-lean.toml` | the quake scenario (4 validators, lean lane on); `up` renders a per-run copy with the budget and peer list templated in |
| `fleet/fleet.env.example` | the topology file template (placeholders only; the real `fleet.env` is git-ignored) |
| `fleet/selftest.sh` | offline tests: parsers, rate arithmetic, manifest rendering, lane guards, dry runs |
| `analysis/heights.py` | offline CL-log analysis: `proposers` (who proposed each decided height, parked / sync-following validators) and `phases` (per-phase time inside a height; needs the arc `lean-lane-instrumented` build) |

Moved here from the Arc branch `lean-lane-v0.2` (`scripts/fleet-lean.sh` and
friends); history before the move lives there.

## The 4-machine LAN recipe

Each machine runs one CL+EL pair (docker) and one lean node (a host process on
`:8560`, reached from its CL container through the docker bridge gateway
`172.17.0.1`). Validator 1 is the machine you run `fleet` on.

Every machine needs docker and ssh access from validator 1; validator 1 also
needs the arc checkout, rust, python3 (with `pyyaml`) and foundry's `cast`.
Firewalls must pass, between the machines: `8560` (lean), `27000+n-1` (CL p2p),
`30300+n` (EL p2p), `8545+(n-1)*100` / `8546+(n-1)*100` (EL http/ws, sampler).

```bash
# on validator 1
cd $ARC_DIR && make build-docker && cargo build --release -p quake
cd $LEAN_DIR && cargo build --release -p lean-lane-node -p spammer

cp bench/fleet/fleet.env.example bench/fleet/fleet.env   # IPs, user, ARC_DIR
bench/fleet/selftest.sh                                  # offline, seconds
bench/fleet/fleet --dry-run up                           # read what it will do

BUDGET_GAS=400000000 FANOUT=100 bench/fleet/fleet up     # ship, boot, gate
LOAD_SECS=600 bench/fleet/fleet load                     # load + samples
bench/fleet/fleet down                                   # logs pulled, data kept
bench/analysis/heights.py proposers bench/runs/<run-id>
# write the run record, then:
bench/fleet/fleet purge <run-id> --yes
```

The EVM arm is the same fleet with `LANE=evm` (no lean node, every
`ARC_PAYMENT_LEAN_*` stripped so the CL is stock, the gas limit set at genesis
and read back off a produced block, reth's per-sender pool caps lifted).

Configuration: `ARC_DIR` (the arc checkout — images are built and quake runs
there), `LEAN_DIR` (default: this repo), `FLEET_ROOT` (default `~/arc-runs`;
must be the same absolute path on every machine), `SSH_CMD` (default plain
`ssh`; `tailscale ssh` also turns on a preflight that refuses a relayed DERP
path). The lean nodes run with `--stats` (`LEAN_STATS=0` turns it off);
`LEAN_NO_FSYNC=1` adds `--no-fsync`; `LEAN_EXTRA_ARGS` passes anything else.
`ARC_HEIGHT_TIMING=1` and `CL_EXTRA_ENV="K=V ..."` go into every CL's env.
Artefacts of a run on validator 1: `bench/runs/<run-id>/` (git-ignored).

## Measurement protocol (short)

A validator can look healthy — containers up, agreement passing, blocks full —
while contributing nothing (a **parked** CL, or a pure **sync-follower** that
decides by fetching and never proposes); each costs ~25 % of cadence.
`fleet` enforces what it can; the rest is on you:

- **Health gate** after boot: containers up, no "Manual intervention", EL fresh
  (not an old chain), zero CL restarts, the measured chain advancing.
- **Lean nodes before CLs**, lane enabled from height 1 of a fresh chain.
- **Fullness with every number** — every sampler row prints `full%`; a
  non-full block measures delivery, not the chain.
- **One spammer per machine** (`DISTRIBUTED=1`, default) on loopback, disjoint
  account ranges: a single remote spammer is delivery-bound.
- **Restarts and pool depth per validator** every minute; a shallow pool on one
  node caps fullness silently. On the lean lane, `staged%` per node is the
  share of anchors that took the healthy staged route (reference ≥ 95 %).
- **Attribute by proposer** after the run (`heights.py proposers`): a slow or
  absent proposer is invisible in cadence averages.
- **Chain-static before `-l`** (the spammer reads nonces once): no surviving
  feeder may be moving them.
- **Logs captured before teardown** (`down` does it); remote effects verified
  by reading them back, never by ssh exit codes.
- Fan-out drains sender balances one way: long runs need a fresh genesis.

## Reference numbers

4 validators on a LAN-ish fleet (heterogeneous, one on wifi), 500 ms pacer,
lean v0.2 + Arc CL `lean-lane-v0.2`; n=1 per point, ±15 % run to run.

| config | cadence | throughput | fullness |
|---|---|---|---|
| lean, N=100, 400 M gas | 1.86–1.97 blk/s | **142,443–150,965 payments/s** | 100 % |
| lean, N=100, 400 M, 30-min soak | — | 120,529–152,183 payments/s, 28/28 minutes > 120k | 100 % |
| lean, N=1, 450 M gas | ~2 blk/s (knee) | 31,317–31,910 tx/s | 100 % |
| EVM lane, N=1 (native transfers) | — | ~6.5k tx/s at any gas limit | never full |

The EVM lane saturates on execution/building (~0.14 Ggas/s) with blocks
50–78 % full; the lean lane at N=1 is pacer-bound at 100 % full.

## 21-node AWS via quake — see design

Not written yet. The plan is to build on quake's own AWS deployment (Terraform
EC2, SSM tunnels, per-node compose) with a lean service in each node's compose
instead of this runner, which stays for the LAN fleet. Design and the
validation ladder: the deslop plan, "Phase 4 — deployability and the 21-node
AWS network".
