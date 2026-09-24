#!/usr/bin/env bash
# image-smoke.sh [heights] — prove the container image runs the lane.
#
# Starts three lean-lane-node containers from the image on a private docker
# network (they peer by container name), waits for the image's HEALTHCHECK to
# report healthy, feeds signed fan-out transactions with the image's spammer,
# drives heights with the stand-in consensus layer (scripts/standin-cl.sh, the
# same driver lean-smoke.sh uses) over published loopback ports, and asserts
# 3/3 nodes at one commitment with non-empty blocks. Everything it creates
# (containers, network, volumes — all named leanimg-*) is removed on exit.
#
#   ./scripts/image-smoke.sh              # 40 heights against lean-lane:local
#   IMAGE=lean-lane:abc1234 ./scripts/image-smoke.sh 100
#
# Needs docker, curl, python3, and foundry's `cast` (for the fund file).
set -uo pipefail
cd "$(cd "$(dirname "$0")/.." && pwd)" || exit 1
HEIGHTS=${1:-40}
IMAGE=${IMAGE:-lean-lane:local}
CHAIN=1338
BUDGET=${BUDGET:-50000000}
N=${N:-10}
P=leanimg-$$                      # prefix of every docker object created here
NET=$P-net
NODES=("$P-n1" "$P-n2" "$P-n3")
RUN=$(mktemp -d /tmp/leanimg-smoke.XXXXXX)
say(){ echo "[$(date +%H:%M:%S)] image-smoke: $*"; }
cleanup(){
  docker rm -f "${NODES[@]}" "$P-spam" >/dev/null 2>&1 || true
  for n in "${NODES[@]}"; do docker volume rm -f "$n-data" >/dev/null 2>&1 || true; done
  docker network rm "$NET" >/dev/null 2>&1 || true
  rm -rf "$RUN"; }
trap cleanup EXIT
. scripts/standin-cl.sh   # rpc, rpc_file, drive_heights, collect_heads
PORTS=()

docker image inspect "$IMAGE" >/dev/null 2>&1 \
  || { echo "FAIL: image $IMAGE not found (run scripts/build-image.sh)"; exit 1; }

# ------------------------------------------------------------ fund file
FUND=$RUN/fund.txt
say "generating 200-account fund file"
bash scripts/gen-lean-fund.sh 200 "$FUND" >/dev/null 2>&1 \
  || { echo "FAIL: fund generation (is foundry/cast installed?)"; exit 1; }
chmod 755 "$RUN"; chmod 644 "$FUND"   # readable by the image's nonroot user

# --------------------------------------------------------------- boot
docker network create "$NET" >/dev/null || { echo "FAIL: network create"; exit 1; }
for n in "${NODES[@]}"; do
  peers=""; for m in "${NODES[@]}"; do [ "$m" = "$n" ] || peers="${peers}http://$m:8560,"; done
  docker run -d --name "$n" --network "$NET" -p 127.0.0.1::8560 \
    -v "$n-data:/data" -v "$FUND:/fund.txt:ro" "$IMAGE" \
    run --datadir /data --bind 0.0.0.0 --port 8560 --chain-id $CHAIN \
      --peers "${peers%,}" --fund-file /fund.txt --fund-balance 10000000000000000000 \
    >/dev/null || { echo "FAIL: docker run $n"; exit 1; }
  PORTS+=("$(docker port "$n" 8560/tcp | head -1 | sed 's/.*://')")
done
for n in "${NODES[@]}"; do
  st=""; for _ in $(seq 1 60); do
    st=$(docker inspect -f '{{.State.Health.Status}}' "$n" 2>/dev/null)
    [ "$st" = healthy ] && break; sleep 1; done
  [ "$st" = healthy ] || { echo "FAIL: $n not healthy ($st)"; docker logs --tail 5 "$n"; exit 1; }
done
say "3 containers healthy (HEALTHCHECK), host ports ${PORTS[*]}"

g=$(head_field "${PORTS[0]}" commitment)
for p in "${PORTS[@]}"; do
  [ "$(head_field "$p" commitment)" = "$g" ] || { echo "FAIL: genesis mismatch on :$p"; exit 1; }
done
say "genesis agreed: ${g:0:18}..."

# --------------------------------------------------------------- load
say "feeding fan-out transactions from the image's spammer (--fanout-outputs $N)"
docker run --rm --name "$P-spam" --network "$NET" --entrypoint spammer "$IMAGE" \
  ws --targets "ws://${NODES[0]}:8560" -r 2000 -g 2 -a 200 --account-offset 0 \
  -t 12 --chain-id $CHAIN --mix fanout=100 --fanout-outputs "$N" -l >/dev/null 2>&1
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
for n in "${NODES[@]}"; do
  [ "$(docker inspect -f '{{.State.Running}}' "$n")" = true ] || { echo "FAIL: $n exited"; exit 1; }
done
echo
echo "PASS — image $IMAGE: $HEIGHTS heights · $txs transactions · $outs payments · 3/3 containers at ${comms[0]:0:18}..."
