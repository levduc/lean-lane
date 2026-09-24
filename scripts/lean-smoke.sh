#!/usr/bin/env bash
# lean-smoke.sh [heights] — prove this repo can run the lean payment lane ALONE.
#
# No docker, no fleet, no reth fork, no tailscale: builds nothing but the two
# crates in this workspace, boots three lean nodes on loopback, and drives them
# with a minimal stand-in for the CL (round-robin proposer: arc_buildBlock on the
# proposer, arc_newBlock on everyone, then check all three agree). Also feeds
# real signed fan-out transactions so the blocks are not empty.
#
#   ./lean-smoke.sh          # 40 heights, ~1 min
#   ./lean-smoke.sh 200      # longer
#
# Exit 0 = the lane works from a clean checkout on this machine.
set -uo pipefail
cd "$(cd "$(dirname "$0")/.." && pwd)" || exit 1
MIXED=0
if [ "${1:-}" = "--mixed" ]; then MIXED=1; shift; fi
HEIGHTS=${1:-40}
PORTS=(8571 8572 8573)
MIX_SPEC="1:20,5:20,10:20,50:20,100:20"   # equal-by-tx (roadmap §8)
RUN=$(mktemp -d /tmp/lean-smoke.XXXXXX)
FUND=${FUND:-$RUN/fund.txt}
TARGET=${CARGO_TARGET_DIR:-target}   # cargo build below honours the same variable
BIN=$TARGET/release/lean-lane-node
SPAM=$TARGET/release/spammer
CHAIN=1338
BUDGET=${BUDGET:-50000000}
N=${N:-10}
say(){ echo "[$(date +%H:%M:%S)] smoke: $*"; }
cleanup(){ for p in "${PORTS[@]}"; do
    pid=$(ss -ltnp 2>/dev/null | grep ":$p " | grep -oP 'pid=\K[0-9]+' | head -1)
    [ -n "${pid:-}" ] && kill -9 "$pid" 2>/dev/null
  done; rm -rf "$RUN"; }
trap cleanup EXIT
. scripts/standin-cl.sh   # rpc, rpc_file, drive_heights, collect_heads

# ---------------------------------------------------------------- build
for t in "$BIN" "$SPAM"; do
  [ -x "$t" ] || { say "building $(basename $t) (first run is slow)"; cargo build --release -p "$(basename $t)" >/dev/null 2>&1 || cargo build --release -p lean-lane-node >/dev/null 2>&1; }
done
[ -x "$BIN" ] || { echo "FAIL: cargo build --release -p lean-lane-node"; exit 1; }
[ -x "$SPAM" ] || { echo "FAIL: cargo build --release -p spammer"; exit 1; }

# ------------------------------------------------------------ fund file
if [ ! -s "$FUND" ]; then
  say "generating 200-account fund file"
  bash scripts/gen-lean-fund.sh 200 "$FUND" >/dev/null 2>&1 \
    || { echo "FAIL: fund generation (is foundry/cast installed?)"; exit 1; }
fi

# --------------------------------------------------------------- boot
peers_for(){ local self=$1 out=""; for p in "${PORTS[@]}"; do
    [ "$p" = "$self" ] || out="${out}http://127.0.0.1:$p,"; done; echo "${out%,}"; }
for p in "${PORTS[@]}"; do
  setsid "$BIN" run --datadir "$RUN/n$p" --port "$p" --bind 127.0.0.1 --chain-id $CHAIN \
    --peers "$(peers_for "$p")" --fund-file "$FUND" \
    --fund-balance 10000000000000000000 > "$RUN/n$p.log" 2>&1 &
  disown
done
for p in "${PORTS[@]}"; do
  ok=0; for _ in $(seq 1 30); do
    rpc "$p" arc_getHead '{}' | grep -q commitment && { ok=1; break; }; sleep 1; done
  [ $ok = 1 ] || { echo "FAIL: node on :$p did not start"; tail -5 "$RUN/n$p.log"; exit 1; }
done
say "3 nodes up on ${PORTS[*]}"

# genesis must be identical everywhere (it is a pure function of chain id)
g=$(rpc "${PORTS[0]}" arc_getHead '{}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["commitment"])')
for p in "${PORTS[@]}"; do
  gg=$(rpc "$p" arc_getHead '{}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["commitment"])')
  [ "$gg" = "$g" ] || { echo "FAIL: genesis mismatch on :$p"; exit 1; }
done
say "genesis agreed: ${g:0:18}..."

# --------------------------------------------------------------- load
FANOUT_ARG=$N
[ "$MIXED" = 1 ] && FANOUT_ARG="$MIX_SPEC"
say "feeding fan-out transactions (--fanout-outputs $FANOUT_ARG)"
"$SPAM" ws --targets "ws://127.0.0.1:${PORTS[0]}" -r 2000 -g 2 -a 200 --account-offset 0 \
  -t 12 --chain-id $CHAIN --mix fanout=100 --fanout-outputs "$FANOUT_ARG" -l >/dev/null 2>&1
pend=$(rpc "${PORTS[0]}" txpool_status '{}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["pending"])')
[ "${pend:-0}" -gt 0 ] || { echo "FAIL: no transactions reached the pool"; exit 1; }
say "pool: $pend pending"

# ------------------------------------------- drive heights (stand-in CL)
say "driving $HEIGHTS heights (round-robin proposer)"
drive_heights "$HEIGHTS" || exit 1

# ------------------------------------------------------------- verdict
collect_heads
say "heads: ${heads[*]}  distinct commitments: $uniq_c"
[ "$uniq_c" = "1" ] || { echo "FAIL: nodes diverged"; exit 1; }
[ "${heads[0]}" = "$HEIGHTS" ] || { echo "FAIL: expected head $HEIGHTS, got ${heads[0]}"; exit 1; }
[ "$txs" -gt 0 ] || { echo "FAIL: all blocks were empty"; exit 1; }
AVG_N=$(python3 -c "print(f'{$outs/$txs:.1f}')")
if [ "$MIXED" = 1 ]; then
  # equal-by-tx over {1,5,10,50,100} => avg N ~= 33.2 (V5 accounting check)
  python3 -c "import sys; a=$outs/$txs; sys.exit(0 if 28 <= a <= 38 else 1)" \
    || { echo "FAIL: avg N=$AVG_N outside [28,38] — mix not realized"; exit 1; }
  say "avg N=$AVG_N (mix realized)"
fi

# ---- V4: replay invariance — a FRESH node fed the same block bytes must reach
# the same head commitment.
RP=8574
say "V4: replaying $HEIGHTS blocks into a fresh node on :$RP"
setsid "$BIN" run --datadir "$RUN/replay" --port $RP --bind 127.0.0.1 \
  --chain-id $CHAIN --fund-file "$FUND" \
  --fund-balance 10000000000000000000 > "$RUN/replay.log" 2>&1 &
disown
ok=0; for _ in $(seq 1 30); do
  rpc $RP arc_getHead '{}' | grep -q commitment && { ok=1; break; }; sleep 1; done
[ $ok = 1 ] || { echo "FAIL: replay node did not start"; exit 1; }
for h in $(seq 1 "$HEIGHTS"); do
  rpc "${PORTS[0]}" arc_getBlockBytes "{\"number\":$h}" \
    | python3 -c 'import sys,json;print(json.dumps({"blockBytes":json.load(sys.stdin)["result"]["blockBytes"]}))' > "$RUN/.rblk"
  r=$(rpc_file $RP arc_newBlock "$RUN/.rblk")
  echo "$r" | grep -q '"commitment"' || { echo "FAIL: V4 replay rejected block $h"; echo "$r" | head -c 200; exit 1; }
done
RC=$(rpc $RP arc_getHead '{}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["commitment"])')
[ "$RC" = "${comms[0]}" ] || { echo "FAIL: V4 DIVERGENCE — replay head $RC vs ${comms[0]}"; exit 1; }
pid=$(ss -ltnp 2>/dev/null | grep ":$RP " | grep -oP 'pid=\K[0-9]+' | head -1)
[ -n "${pid:-}" ] && kill -9 "$pid" 2>/dev/null
say "V4 ok: replay node reached identical commitment"
echo
echo "✅ PASS — lean lane runs from this repo alone"
echo "   $HEIGHTS heights · $txs transactions · $outs payments · avg N=$AVG_N · 3/3 nodes at ${comms[0]:0:18}..."
echo "   (no docker, no fleet, no reth fork)"
