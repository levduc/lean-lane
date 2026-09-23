# shellcheck shell=bash
# lib.sh — shared helpers for bench/fleet/fleet: config, logging, remote exec,
# shipping, RPC readers, lean-node lifecycle, preflight. Sourced, never run.
#
# Every JSON-RPC body is decoded by sampler.py (`sampler.py parse <kind>`), so
# the status/gate code here and the per-minute sampler share one parser.

FLEET_DIR=${FLEET_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}
SAMPLER=$FLEET_DIR/sampler.py
DRY=${DRY:-0}

say(){ echo "[$(date +%H:%M:%S)] fleet(${RUN_ID:-?}/${LANE:-?}): $*"; }
die(){ echo "fleet: FAIL: $*" >&2; exit 1; }
dry(){ [ "$DRY" = 1 ]; }

# ---------------------------------------------------------------- config
# fleet.env is sourced as plain assignments, which would clobber anything the
# caller exported — so the load-shape knobs the caller set are snapshotted
# before sourcing and restored after: the process environment always wins.
FLEET_OVERRIDABLE="LANE FANOUT LOAD_SECS LOAD_RATE POOL_TARGET GENERATORS DISTRIBUTED
  BUDGET_GAS BUDGET_TXS FUND_ACCOUNTS ARC_HEIGHT_TIMING LEAN_STATS LEAN_NO_FSYNC
  LEAN_EXTRA_ARGS CL_EXTRA_ENV ARC_DIR LEAN_DIR FLEET_ROOT SSH_CMD SCENARIO"

fleet_init(){ # fleet_init <action>
  local action=${1:-} v
  CFG=${FLEET_ENV:-$FLEET_DIR/fleet.env}
  if [ ! -f "$CFG" ]; then
    dry || die "no $CFG (cp $FLEET_DIR/fleet.env.example $FLEET_DIR/fleet.env and edit it)"
    CFG=$FLEET_DIR/fleet.env.example
  fi
  for v in $FLEET_OVERRIDABLE; do
    [ -n "${!v+x}" ] && eval "_had_$v=1; _val_$v=\${$v}"
  done
  # shellcheck disable=SC1090
  source "$CFG"
  for v in $FLEET_OVERRIDABLE; do
    eval "[ -n \"\${_had_$v:-}\" ]" && eval "$v=\${_val_$v}"
  done

  LANE=${LANE:-lean}
  case "$LANE" in lean|evm) ;; *) die "LANE must be 'lean' or 'evm' (got '$LANE')" ;; esac
  LEAN_DIR=${LEAN_DIR:-$(cd "$FLEET_DIR/../.." && pwd)}
  ARC_DIR=${ARC_DIR:-}
  FLEET_ROOT=${FLEET_ROOT:-$HOME/arc-runs}
  REMOTE_USER=${REMOTE_USER:-$USER}
  SSH_CMD=${SSH_CMD:-ssh -o BatchMode=yes -o ConnectTimeout=10}
  SSH_TMO=${SSH_TMO:-120}
  N=$(( ${#FLEET_IPS[@]} - 1 ))
  [ "$N" -ge 2 ] || die "FLEET_IPS needs at least 2 validators (index 0 is a placeholder)"

  SCEN=${SCENARIO:-fleet4-lean}
  MANIFEST=${MANIFEST:-$FLEET_DIR/$SCEN.toml}
  QDIR=$ARC_DIR/.quake/${SCEN//_/-}       # quake: <cwd>/.quake/<manifest stem>
  QUAKE=${QUAKE:-$ARC_DIR/target/release/quake}
  RUNS=${RUNS_DIR:-$LEAN_DIR/bench/runs}   # this machine's artefacts (git-ignored)
  CHAIN=${LEAN_CHAIN_ID:-1338}
  EVM_CHAIN=${EVM_CHAIN_ID:-1337}
  LEAN_PORT=${LEAN_PORT:-8560}
  LEAN_SRC=${LEAN_BIN:-$LEAN_DIR/target/release/lean-lane-node}
  SPAM_SRC=${SPAMMER_BIN:-$LEAN_DIR/target/release/spammer}
  IMAGES=${IMAGES:-"arc_consensus:latest arc_execution:latest"}

  FANOUT=${FANOUT:-50}
  LOAD_SECS=${LOAD_SECS:-600}
  LOAD_RATE=${LOAD_RATE:-3000}
  POOL_TARGET=${POOL_TARGET:-1500}
  FUND_ACCOUNTS=${FUND_ACCOUNTS:-800}
  FUND_BALANCE=${FUND_BALANCE:-10000000000000000000}
  GENERATORS=${GENERATORS:-2}
  DISTRIBUTED=${DISTRIBUTED:-1}
  LEAN_STATS=${LEAN_STATS:-1}
  LEAN_NO_FSYNC=${LEAN_NO_FSYNC:-0}
  SAMPLE_SECS=${SAMPLE_SECS:-60}

  # BUDGET_TXS = the tx count that fills a block: a number without fullness is
  # not a result. Lean: budget / (21000 + 5000 * outputs); EVM: 21000 / transfer.
  BUDGET_GAS=${BUDGET_GAS:-150000000}
  if [ "$LANE" = evm ]; then BUDGET_TXS=${BUDGET_TXS:-$((BUDGET_GAS / 21000))}
  else BUDGET_TXS=${BUDGET_TXS:-$((BUDGET_GAS / (21000 + 5000 * FANOUT)))}; fi

  # EVM arm: reth's per-sender slot cap nonce-poisons a saturating spammer's
  # senders (one dropped tx strands the stream) — lift count AND size caps.
  EVM_MAX_ACCOUNT_SLOTS=${EVM_MAX_ACCOUNT_SLOTS:-4096}
  EVM_PENDING_MAX_COUNT=${EVM_PENDING_MAX_COUNT:-400000}
  EVM_PENDING_MAX_SIZE=${EVM_PENDING_MAX_SIZE:-2048}
  # ProtocolConfig precompile + its localdev controller key (set-gas-limit only).
  PROTOCOL_CONFIG=${PROTOCOL_CONFIG:-0x3600000000000000000000000000000000000001}
  CTRL_KEY=${CTRL_KEY:-0xdbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97}

  if [ -z "${RUN_ID:-}" ]; then
    if [ "$action" = up ] || [ ! -s "$RUNS/latest" ]; then RUN_ID=run-$(date +%m%d-%H%M)
    else RUN_ID=$(cat "$RUNS/latest"); fi
  fi
  ROOT=$FLEET_ROOT/$RUN_ID       # the same absolute path on every machine
  LOCAL=$RUNS/$RUN_ID
}

# ---------------------------------------------------------------- remote exec
ip_of(){ echo "${FLEET_IPS[$1]}"; }
name_of(){ echo "${FLEET_NAMES[$1]:-validator$1}"; }
# ssh target: FLEET_SSH[n] if given (an ssh alias), else REMOTE_USER@<ip>.
ssh_target(){ echo "${FLEET_SSH[$1]:-$REMOTE_USER@$(ip_of "$1")}"; }
# rsh <n> <command-string> — validator 1 is this machine. Exit codes of some
# ssh transports lie (tailscale ssh after its session check expires), so
# callers verify remote EFFECTS by reading them back, never by $?.
rsh(){ local n=$1; shift
  if dry; then echo "  [dry] v$n: $*"; return 0; fi
  if [ "$n" = 1 ]; then bash -c "$*"
  # shellcheck disable=SC2086
  else timeout "$SSH_TMO" $SSH_CMD "$(ssh_target "$n")" "$*" 2>/dev/null; fi; }
# rpipe <n> <secs> <remote-command> — stdin streamed to a remote command
rpipe(){ local n=$1 t=$2; shift 2
  if dry; then cat >/dev/null; echo "  [dry] v$n <stdin: $*"; return 0; fi
  # shellcheck disable=SC2086
  timeout "$t" $SSH_CMD "$(ssh_target "$n")" "$*" >/dev/null 2>&1; }
# x <cmd...> — a LOCAL side effect (printed instead of run under --dry-run)
x(){ if dry; then echo "  [dry] local: $*"; return 0; fi; "$@"; }

# ---------------------------------------------------------------- RPC readers
rpc(){ # rpc <url> <method> <json-params> — body through a FILE (a full block is ~1 MB)
  dry && return 0
  local body; body=$(mktemp)
  printf '{"jsonrpc":"2.0","id":1,"method":"%s","params":%s}' "$2" "$3" > "$body"
  curl -s -m "${RPC_TMO:-25}" -X POST "$1" -H 'content-type: application/json' --data-binary "@$body"
  rm -f "$body"; }
parse(){ python3 "$SAMPLER" parse "$@" 2>/dev/null; }
jget(){ python3 -c 'import sys,json
d=json.load(sys.stdin)
for k in sys.argv[1].split("."):
    d=d[k]
print(d)' "$1" 2>/dev/null; }

lean_url(){ echo "http://$(ip_of "$1"):$LEAN_PORT"; }
# quake gives validator n the port offset (n-1)*100 off reth's 8545 (http) /
# 8546 (ws), published on that machine's host.
el_url(){ echo "http://$(ip_of "$1"):$((8545 + ($1 - 1) * 100))"; }
el_ws_port(){ echo $((8546 + ($1 - 1) * 100)); }

lean_head(){ rpc "$(lean_url "$1")" arc_getHead '{}'; }
lean_height(){ local h; h=$(lean_head "$1" | jget result.number); echo "${h:-0}"; }
el_height(){ local h; h=$(rpc "$(el_url "$1")" eth_blockNumber '[]' | jget result); [ -n "$h" ] && echo $((h)) || echo 0; }
block_txs(){ rpc "$(lean_url "$1")" arc_getBlockBytes "{\"number\":$2}" | parse blocktxs; }
pool_depth(){ rpc "$(lean_url "$1")" txpool_status '{}' | parse leanpool; }
el_pool_depth(){ rpc "$(el_url "$1")" txpool_status '[]' | parse elpool; }
# "number gasUsed gasLimit ntx hash"; "0 0 0 0 -" when unreadable
el_block_stats(){ rpc "$(el_url "$1")" eth_getBlockByNumber "[\"$2\",false]" | parse block; }
el_gas_limit(){ rpc "$(el_url "$1")" eth_getBlockByNumber '["latest",false]' | parse gaslimit; }
# lane_metrics <dh> <dt> <mean_txs> <mean_gas> <budget_txs> <gas_limit> <pay_per_tx>
#   -> "blk_per_min tx_per_s pay_per_s Mgas_per_s full_pct"
lane_metrics(){ python3 "$SAMPLER" metrics "$@"; }

cl_restarts(){ rsh "$1" "docker inspect --format '{{.RestartCount}}' validator$1_cl 2>/dev/null" | tr -dc '0-9'; }
cl_parked(){ rsh "$1" "docker logs validator$1_cl 2>&1 | grep -c 'Manual intervention'" | tr -dc '0-9'; }
# a MANUAL `docker restart` does not bump RestartCount; StartedAt proves the bounce
cl_started(){ rsh "$1" "docker inspect --format '{{.State.StartedAt}}' validator$1_cl 2>/dev/null" | tr -d ' \r'; }
compose_of(){ if [ "$1" = 1 ]; then echo "$QDIR/compose.yaml"; else echo "$ROOT/compose-val$1.yaml"; fi; }

# ---------------------------------------------------------------- lean node
peers_for(){ local n=$1 p="" j; for j in $(seq 1 "$N"); do
    [ "$j" = "$n" ] || p="${p}http://$(ip_of "$j"):$LEAN_PORT,"; done; echo "${p%,}"; }
lean_flags(){ local n=$1 f
  f="run --datadir $ROOT/lean --port $LEAN_PORT --bind 0.0.0.0 --chain-id $CHAIN"
  f="$f --peers $(peers_for "$n") --fund-file $ROOT/fund.txt --fund-balance $FUND_BALANCE"
  [ "$LEAN_STATS" = 1 ] && f="$f --stats"
  [ "$LEAN_NO_FSYNC" = 1 ] && f="$f --no-fsync"
  echo "$f${LEAN_EXTRA_ARGS:+ $LEAN_EXTRA_ARGS}"; }
# remote daemons: (setsid nohup .. &) </dev/null — an ssh teardown kills a plain &
lean_launch_cmd(){
  echo "(setsid nohup $ROOT/lean-lane-node $(lean_flags "$1") >> $ROOT/logs/lean.log 2>&1 < /dev/null &)"; }
# the lean pid, matched by the BINARY PATH in argv[1] — never `pkill -f`, which
# also matches the shell carrying the pattern
lean_pid(){ dry && return 0
  rsh "$1" "ps -eo pid=,args= | awk -v b=$ROOT/lean-lane-node '\$2==b{print \$1}'" | tr -dc '0-9 ' | awk '{print $1}'; }
lean_kill(){ local n=$1 p
  if dry; then rsh "$n" "kill <pid of $ROOT/lean-lane-node>"; return 0; fi
  p=$(lean_pid "$n"); [ -n "$p" ] || return 0
  rsh "$n" "kill $p 2>/dev/null; true"; sleep 2
  p=$(lean_pid "$n"); [ -n "$p" ] && rsh "$n" "kill -9 $p 2>/dev/null; true"; return 0; }
lean_wait(){ local n=$1 t=${2:-90} i
  dry && return 0
  for i in $(seq 1 "$t"); do lean_head "$n" | grep -q commitment && return 0; sleep 1; done; return 1; }

# ---------------------------------------------------------------- shipping
ship_file(){ # ship_file <n> <local> <remote> — sha-verified AFTER transfer
  local n=$1 src=$2 dst=$3 want have
  if dry; then echo "  [dry] v$n <- $src -> $dst (gzip over ssh, sha-verified)"; return 0; fi
  want=$(sha256sum "$src" | cut -c1-16)
  have=$(rsh "$n" "sha256sum $dst 2>/dev/null | cut -c1-16" | tail -1 | tr -dc '0-9a-f')
  if [ "$have" = "$want" ]; then echo "    $(basename "$dst"): current"; return 0; fi
  gzip -c "$src" | rpipe "$n" 900 "cat > $dst.gz && gunzip -f $dst.gz && chmod +x $dst 2>/dev/null; true"
  have=$(rsh "$n" "sha256sum $dst 2>/dev/null | cut -c1-16" | tail -1 | tr -dc '0-9a-f')
  [ "$have" = "$want" ] && { echo "    $(basename "$dst"): shipped ok"; return 0; }
  echo "    $(basename "$dst"): FAILED (have ${have:-none}, want $want)"; return 1; }

img_probe(){ case "$1" in arc_execution*) echo /usr/local/bin/arc-node-execution;; *) echo /usr/local/bin/arc-node-consensus;; esac; }
ship_image(){ # ship_image <n> <image> — verified by the sha of the BINARY INSIDE the image
  local n=$1 img=$2 probe want have
  if dry; then echo "  [dry] v$n <- docker save $img | gzip | docker load (sha of the binary inside verified)"; return 0; fi
  probe=$(img_probe "$img")
  want=$(docker run --rm --entrypoint sha256sum "$img" "$probe" 2>/dev/null | awk '{print $1}')
  [ -n "$want" ] || { echo "    $img: cannot hash locally"; return 1; }
  have=$(rsh "$n" "docker run --rm --entrypoint sha256sum $img $probe 2>/dev/null | awk '{print \$1}'" | tr -dc '0-9a-f')
  if [ "$have" = "$want" ]; then echo "    $img: current (${want:0:12})"; return 0; fi
  echo "    $img: shipping (local ${want:0:12}, remote ${have:0:12})..."
  docker save "$img" | gzip -1 | rpipe "$n" 3600 "gunzip | docker load"
  have=$(rsh "$n" "docker run --rm --entrypoint sha256sum $img $probe 2>/dev/null | awk '{print \$1}'" | tr -dc '0-9a-f')
  [ "$have" = "$want" ] && { echo "    $img: shipped ok (${want:0:12})"; return 0; }
  echo "    $img: CONTENT MISMATCH (remote ${have:0:12} != local ${want:0:12}) — truncated pipe?"; return 1; }

# ---------------------------------------------------------------- preflight
# Disk on every machine (a full disk truncates a datadir mid-run), reachability,
# and — only when the transport is tailscale (PREFLIGHT_TAILSCALE=1, default
# when SSH_CMD starts with "tailscale") — that each path is DIRECT, not a DERP
# relay: a relayed path adds tens of ms to every RPC and value-sync round trip
# and looks exactly like a slow validator.
preflight(){
  local n ip out avail_kb gb ts=${PREFLIGHT_TAILSCALE:-}
  [ -n "$ts" ] || case "$SSH_CMD" in tailscale*) ts=1;; *) ts=0;; esac
  say "preflight: reachability + disk on all $N machines (tailscale direct-path check: $ts)"
  if [ "$ts" = 1 ]; then
    for n in $(seq 2 "$N"); do
      ip=$(ip_of "$n")
      if dry; then echo "  [dry] local: tailscale ping -c 1 $ip (must be direct, not DERP)"; continue; fi
      out=$(timeout 15 tailscale ping -c 1 "$ip" 2>&1)
      echo "    validator$n ($ip): $(echo "$out" | grep -m1 '^pong\|^no reply' || echo "$out" | tail -1)"
      echo "$out" | grep -q 'via DERP' && die "validator$n ($ip): tailscale path is RELAYED (DERP): $out"
      echo "$out" | grep -qE 'via [0-9]+\.[0-9]+\.[0-9]+\.[0-9]+:[0-9]+' \
        || die "validator$n ($ip): tailscale ping reported no direct path: $out"
    done
  fi
  for n in $(seq 1 "$N"); do
    dry && { rsh "$n" "mkdir -p $FLEET_ROOT && df -Pk $FLEET_ROOT  (need >= 5 GB)"; continue; }
    avail_kb=$(rsh "$n" "mkdir -p $FLEET_ROOT && df -Pk $FLEET_ROOT 2>/dev/null | tail -1 | awk '{print \$4}'" | tr -dc '0-9')
    [ -n "$avail_kb" ] || die "validator$n ($(ssh_target "$n")): unreachable, or $FLEET_ROOT unwritable"
    gb=$((avail_kb / 1024 / 1024))
    echo "    validator$n $(name_of "$n"): $FLEET_ROOT has ${gb} GB free"
    [ "$avail_kb" -ge $((5 * 1024 * 1024)) ] || die "validator$n: only ${gb} GB free under $FLEET_ROOT (need >= 5 GB)"
  done
  say "preflight PASS"
}
