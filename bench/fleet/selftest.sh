#!/usr/bin/env bash
# selftest.sh — offline tests for bench/fleet (no machine, no docker, no fleet).
#
#   bench/fleet/selftest.sh
#
# Sources `fleet` with FLEET_SOURCE_ONLY=1 against the placeholder
# fleet.env.example, stubs rpc() with canned JSON-RPC bodies, and asserts on
# the parsers, the rate arithmetic, the manifest rendering and the lane guards.
# Each case is something that was once wrong in a hand-run experiment:
#   * a hex header field read as decimal — a silent 100x
#   * an unreachable RPC parsed as 0, not "unreadable" — a fake 0 % fullness
#   * the EL ws port confused with the http port — spammers talking to nothing
#   * fullness against the wrong denominator on one lane
#   * a manifest strip that leaves an ARC_PAYMENT_LEAN_* line: a half-lean EVM arm
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
no(){ FAIL=$((FAIL+1)); printf '  FAIL %s\n       want: %s\n       got:  %s\n' "$1" "$2" "$3"; }
eq(){ if [ "$2" = "$3" ]; then ok "$1"; else no "$1" "$2" "$3"; fi; }
has(){ case "$3" in *"$2"*) ok "$1";; *) no "$1" "contains '$2'" "$3";; esac; }

export FLEET_ENV=$HERE/fleet.env.example
unset BUDGET_GAS BUDGET_TXS FANOUT
LANE=evm; FLEET_SOURCE_ONLY=1
# shellcheck source=fleet
. "$HERE/fleet"
cd "$HERE" || exit 1

MOCK=''
rpc(){ printf '%s' "$MOCK"; }

echo "selftest: EL block parsing"
MOCK='{"jsonrpc":"2.0","id":1,"result":{
  "number":"0x3039","gasUsed":"0x8ef8a80","gasLimit":"0x8f0d180",
  "hash":"0xabc1230000000000000000000000000000000000000000000000000000000def",
  "baseFeePerGas":"0x3b9aca00","transactions":["0x01","0x02","0x03","0x04","0x05"]}}'
eq "el_block_stats decodes number/gasUsed/gasLimit/ntx/hash" \
   "12345 149916288 150000000 5 0xabc1230000000000000000000000000000000000000000000000000000000def" \
   "$(el_block_stats 1 latest)"
eq "el_gas_limit decodes the header gas limit from hex" "150000000" "$(el_gas_limit 1)"
MOCK='{"jsonrpc":"2.0","id":1,"result":null}'
eq "el_block_stats on a null result stays numeric" "0 0 0 0 -" "$(el_block_stats 1 latest)"
eq "el_gas_limit on a null result is 0" "0" "$(el_gas_limit 1)"
MOCK=''
eq "el_block_stats on an empty body" "0 0 0 0 -" "$(el_block_stats 1 latest)"
MOCK='not json'
eq "el_block_stats on a non-JSON body" "0 0 0 0 -" "$(el_block_stats 1 latest)"

echo "selftest: txpool depth"
MOCK='{"jsonrpc":"2.0","id":1,"result":{"pending":"0x1f4","queued":"0x64"}}'
eq "el_pool_depth sums hex pending+queued" "600" "$(el_pool_depth 1)"
MOCK='{"jsonrpc":"2.0","id":1,"result":{"pending":500,"queued":100}}'
eq "el_pool_depth accepts decimals" "600" "$(el_pool_depth 1)"
MOCK='{"jsonrpc":"2.0","id":1,"error":{"code":-32601}}'
eq "el_pool_depth on an error is -1, never 0" "-1" "$(el_pool_depth 1)"
MOCK='{"jsonrpc":"2.0","id":1,"result":{"pending":1974,"queued":3}}'
eq "lean pool_depth is pending only" "1974" "$(pool_depth 1)"

echo "selftest: EL ports (quake: 8545/8546 + (n-1)*100)"
eq "validator1 EL ws port" "8546" "$(el_ws_port 1)"
eq "validator4 EL ws port" "8846" "$(el_ws_port 4)"
has "validator4 EL http url" ":8845" "$(el_url 4)"

echo "selftest: lane_metrics"
eq "evm: blk/min tx/s pay/s Mgas/s full%" "100.00 11903 11903 250.0 100" \
   "$(lane_metrics 100 60 7142 149982000 7142 150000000 1)"
eq "lean N=50: fullness by tx count, derived Mgas/s" "120.00 1106 55300 299.7 100" \
   "$(lane_metrics 120 60 553 $((553 * (21000 + 5000 * 50))) 553 0 50)"
read -r _ _ _ _ lfull <<< "$(lane_metrics 120 60 276 $((276 * 271000)) 553 0 50)"
eq "lean: half-full block reads 50 %" "50" "$lfull"
read -r _ _ _ _ efull <<< "$(lane_metrics 120 60 3571 75000000 7142 150000000 1)"
eq "evm: half-full block reads 50 % from gas" "50" "$efull"
eq "dt=0: zero rates, real fullness" "0.00 0 0 0.0 100" "$(lane_metrics 0 0 7142 150000000 7142 150000000 1)"
eq "dh=0 over a live interval is zeroes" "0.00 0 0 0.0 0" "$(lane_metrics 0 60 0 0 7142 150000000 1)"

echo "selftest: EVM agreement"
el_block_stats(){ echo "900 1 150000000 7 0xdead"; }
has "identical head hashes report agreement" "agreement: all 4 ELs" "$(evm_status 2>&1)"
el_block_stats(){ if [ "$1" = 3 ]; then echo "900 1 150000000 7 0xfeed"; else echo "900 1 150000000 7 0xdead"; fi; }
has "a dissenting head hash reports DIVERGENCE" "DIVERGENCE" "$(evm_status 2>&1)"
el_block_stats(){ if [ "$1" = 4 ]; then echo "700 1 150000000 7 0xdead"; else echo "900 1 150000000 7 0xdead"; fi; }
has "the height spread is printed" "spread 200" "$(evm_status 2>&1)"
unset -f el_block_stats

echo "selftest: manifest rendering"
EFF=$(mktemp -d); trap 'rm -rf "$EFF"' EXIT
LANE=evm; render_manifest "$EFF/evm/fleet4-lean.toml"
eq "evm: no ARC_PAYMENT_LEAN_* survives" "0" "$(grep -c ARC_PAYMENT_LEAN "$EFF/evm/fleet4-lean.toml")"
python3 - "$EFF/evm/fleet4-lean.toml" <<'PY'
import sys, tomllib
d = tomllib.load(open(sys.argv[1], "rb"))
assert d["cl"]["env"] == {}, d["cl"]["env"]
assert d["el"]["config"]["txpool"]["max_account_slots"] == 4096
assert len(d["nodes"]) == 4
PY
eq "evm: valid TOML, empty [cl.env], txpool caps lifted, 4 nodes" "0" "$?"
LANE=lean BUDGET_GAS=400000000 ARC_HEIGHT_TIMING=0 CL_EXTRA_ENV="ARC_X=7"
render_manifest "$EFF/lean/fleet4-lean.toml"
python3 - "$EFF/lean/fleet4-lean.toml" <<'PY'
import sys, tomllib
e = tomllib.load(open(sys.argv[1], "rb"))["cl"]["env"]
assert e["ARC_PAYMENT_LEAN_BUDGET_GAS"] == "400000000", e
assert e["ARC_PAYMENT_LEAN_PEER_RPCS"] == ",".join(f"http://192.0.2.1{i}:8560" for i in range(1, 5)), e
assert "ARC_HEIGHT_TIMING" not in e, "ARC_HEIGHT_TIMING=0 must stay OFF"
assert e["ARC_X"] == "7", e
PY
eq "lean: budget + peer list templated, TIMING=0 stays off, extra env injected" "0" "$?"
ARC_HEIGHT_TIMING=yes CL_EXTRA_ENV=""; render_manifest "$EFF/t/fleet4-lean.toml"
eq "ARC_HEIGHT_TIMING=yes injects" "1" "$(grep -c '^ARC_HEIGHT_TIMING = "1"' "$EFF/t/fleet4-lean.toml")"
ARC_HEIGHT_TIMING=""

echo "selftest: lean node launch (v0.3 CLI)"
LEAN_STATS=1 LEAN_NO_FSYNC=0 LEAN_EXTRA_ARGS=""
cmd=$(lean_launch_cmd 2)
has "flags: --stats" " --stats" "$cmd"
has "peers exclude self" "--peers http://192.0.2.11:8560,http://192.0.2.13:8560,http://192.0.2.14:8560 " "$cmd"
case "$cmd" in *--shim*|*--block-ms*|*--budget-gas*|*LEAN_STATS=*|*--no-fsync*) no "no removed flags / env" "absent" "$cmd";; *) ok "no removed flags / env";; esac
LEAN_NO_FSYNC=1; has "LEAN_NO_FSYNC=1 -> --no-fsync" "--no-fsync" "$(lean_launch_cmd 2)"; LEAN_NO_FSYNC=0

echo "selftest: lane guards + budget"
out=$(env -u FLEET_SOURCE_ONLY LANE=bogus bash "$HERE/fleet" status 2>&1)
has "an unknown LANE is refused" "LANE must be 'lean' or 'evm'" "$out"
out=$(env -u FLEET_SOURCE_ONLY LANE=evm bash "$HERE/fleet" kill-lean 2 5 2>&1)
has "kill-lean is refused on the EVM lane" "runs no lean node" "$out"
eq "LANE=evm: BUDGET_TXS at 21000 gas/transfer" "7142" \
   "$(env -u BUDGET_TXS LANE=evm BUDGET_GAS=150000000 FLEET_SOURCE_ONLY=1 bash -c ". $HERE/fleet; echo \$BUDGET_TXS")"
eq "LANE=lean: fan-out denominator" "553" \
   "$(env -u BUDGET_TXS LANE=lean BUDGET_GAS=150000000 FANOUT=50 FLEET_SOURCE_ONLY=1 bash -c ". $HERE/fleet; echo \$BUDGET_TXS")"

echo "selftest: dry run"
out=$(env -u FLEET_SOURCE_ONLY RUNS_DIR="$EFF/runs" bash "$HERE/fleet" --dry-run up 2>&1); rc=$?
eq "fleet --dry-run up exits 0" "0" "$rc"
has "dry run renders with quake -f" "setup --force" "$out"
has "dry run launches lean nodes before the CLs" "lean-lane-node run" "$out"
[ ! -e "$EFF/runs" ] && ok "dry run writes no run dir" || no "dry run writes no run dir" "absent" "$(ls "$EFF/runs")"
out=$(env -u FLEET_SOURCE_ONLY LANE=evm bash "$HERE/fleet" --dry-run up 2>&1)
has "evm dry run adds the genesis gas limit" "--block-gas-limit 150000000" "$out"
case "$out" in *lean-lane-node\ run*) no "evm dry run starts no lean node" "absent" "present";; *) ok "evm dry run starts no lean node";; esac
out=$(env -u FLEET_SOURCE_ONLY bash "$HERE/fleet" --dry-run load 2>&1)
has "dry-run load: one spammer per machine" "v4: (setsid nohup" "$out"

echo "selftest: sampler.py"
python3 "$HERE/sampler.py" --selftest >/dev/null && ok "sampler.py --selftest" || no "sampler.py --selftest" "pass" "fail"

echo
printf 'selftest: %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" = 0 ]
