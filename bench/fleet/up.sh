# shellcheck shell=bash
# up.sh — `fleet up`: render, ship, boot, gate. Also the health gate and the
# EVM-lane gas-limit gate / governance tx. Sourced by bench/fleet/fleet.

# The effective manifest: a per-run copy of the tracked scenario with this
# run's knobs templated in (the tracked toml is never edited). quake names the
# testnet after the manifest's FILE STEM, so the copy keeps the basename.
render_manifest(){ # render_manifest <out>
  local out=$1 kv k v
  mkdir -p "$(dirname "$out")"
  if [ "$LANE" = evm ]; then
    # The EVM arm runs a STOCK CL: the lean lane is off because the flag is
    # ABSENT, not set to 0. The emptied [cl.env] header stays as the anchor
    # for the env injections below.
    sed -E '/^#/d; /^ARC_PAYMENT_LEAN_[A-Z_]+ *=/d; /^cl\.env *= *\{ *ARC_PAYMENT_LEAN_RPC/d' "$MANIFEST" > "$out"
    ! grep -q 'ARC_PAYMENT_LEAN' "$out" || die "LANE=evm: ARC_PAYMENT_LEAN_* survived the strip in $out"
    grep -q '^\[cl\.env\]' "$out" || die "LANE=evm: [cl.env] header lost from $out"
    cat >> "$out" <<EOF

# fleet LANE=evm: lift reth's per-sender slot cap (nonce-poisons deep senders)
[el.config.txpool]
max_account_slots = $EVM_MAX_ACCOUNT_SLOTS
pending_max_count = $EVM_PENDING_MAX_COUNT
pending_max_size = $EVM_PENDING_MAX_SIZE
queued_max_count = $EVM_PENDING_MAX_COUNT
queued_max_size = $EVM_PENDING_MAX_SIZE
EOF
  else
    local peers=""; for k in $(seq 1 "$N"); do peers="${peers}http://$(ip_of "$k"):$LEAN_PORT,"; done
    sed -E -e "s/(ARC_PAYMENT_LEAN_BUDGET_GAS = \")[0-9]+(\")/\1${BUDGET_GAS}\2/" \
           -e "s#@PEER_RPCS@#${peers%,}#" "$MANIFEST" > "$out"
    grep -q "ARC_PAYMENT_LEAN_BUDGET_GAS = \"$BUDGET_GAS\"" "$out" || die "failed to template BUDGET_GAS into $out"
    ! grep -q '@PEER_RPCS@' "$out" || die "failed to template the lean peer list into $out"
  fi
  # ARC_HEIGHT_TIMING (only the arc `lean-lane-instrumented` branch emits it):
  # truthiness as the CL parses it — 1/true/yes; "0" must stay OFF.
  case "$(printf %s "${ARC_HEIGHT_TIMING:-}" | tr '[:upper:]' '[:lower:]')" in
    1|true|yes) sed -i '/^\[cl\.env\]/a ARC_HEIGHT_TIMING = "1"' "$out"
                grep -q '^ARC_HEIGHT_TIMING = "1"' "$out" || die "failed to inject ARC_HEIGHT_TIMING" ;;
  esac
  # CL_EXTRA_ENV="K=V K2=V2": extra CL env for A/B knobs (no spaces in values)
  for kv in ${CL_EXTRA_ENV:-}; do
    k=${kv%%=*}; v=${kv#*=}
    sed -i "/^\[cl\.env\]/a $k = \"$v\"" "$out"
    grep -q "^$k = \"$v\"" "$out" || die "failed to inject $k into $out"
  done
  local nodes; nodes=$(grep -c '^\[nodes\.validator' "$out")
  [ "$nodes" = "$N" ] || die "manifest has $nodes validators but FLEET_IPS has $N"
}

up(){
  [ -f "$MANIFEST" ] || die "no scenario $MANIFEST"
  if ! dry; then
    [ -n "$ARC_DIR" ] && [ -d "$ARC_DIR" ] || die "ARC_DIR (the arc checkout: images + quake) is not set or missing"
    [ "$LANE" = evm ] || [ -x "$LEAN_SRC" ] || die "no lean node binary at $LEAN_SRC (cargo build --release -p lean-lane-node)"
    [ -x "$SPAM_SRC" ] || die "no spammer at $SPAM_SRC (cargo build --release -p spammer)"
    [ -x "$QUAKE" ]    || die "no quake at $QUAKE (cargo build --release -p quake in ARC_DIR)"
    local img; for img in $IMAGES; do docker image inspect "$img" >/dev/null 2>&1 || die "local image $img missing (make build-docker in ARC_DIR)"; done
  fi
  preflight
  x mkdir -p "$LOCAL" "$RUNS"; dry || echo "$RUN_ID" > "$RUNS/latest"
  say "effective params: LANE=$LANE BUDGET_GAS=$BUDGET_GAS BUDGET_TXS=$BUDGET_TXS FANOUT=$FANOUT FUND_ACCOUNTS=$FUND_ACCOUNTS"

  local eff=$LOCAL/manifest/$(basename "$MANIFEST")
  dry && eff=$(mktemp -d)/$(basename "$MANIFEST")
  render_manifest "$eff"
  say "effective manifest $eff (tracked $MANIFEST untouched)"
  dry && rm -rf "$(dirname "$eff")"

  say "run root on all $N machines"
  local n
  for n in $(seq 1 "$N"); do
    if dry; then rsh "$n" "mkdir -p $ROOT/quake $ROOT/lean $ROOT/logs"; continue; fi
    rsh "$n" "mkdir -p $ROOT/quake $ROOT/lean $ROOT/logs && echo OK" | grep -q OK \
      || die "validator$n ($(ssh_target "$n")) unreachable or cannot write $ROOT"
    echo "    validator$n $(name_of "$n"): $ROOT ready"
  done

  # One fund file for every lean node (genesis = chain id + fund file). The EVM
  # arm prefunds the same derivation (m/44'/60'/1'/0/i) in the genesis instead.
  local fund=$LOCAL/fund.txt
  if [ "$LANE" = lean ]; then
    if dry; then echo "  [dry] local: $LEAN_DIR/scripts/gen-lean-fund.sh $FUND_ACCOUNTS $fund"
    elif [ ! -s "$fund" ]; then
      say "generating $FUND_ACCOUNTS funded accounts (m/44'/60'/1'/0/i)"
      PATH="$PATH:$HOME/.foundry/bin" bash "$LEAN_DIR/scripts/gen-lean-fund.sh" "$FUND_ACCOUNTS" "$fund" >/dev/null 2>&1 \
        || die "fund-file generation failed (foundry's cast on PATH?)"
    fi
    dry || say "fund file: $(wc -l < "$fund") accounts, sha $(sha256sum "$fund" | cut -c1-16)"
  fi

  # quake renders (does not start) the testnet; --block-gas-limit + -e on the
  # EVM arm write the limit and prefund the spammer's accounts at genesis.
  local setup=(setup --force)
  [ "$LANE" = evm ] && setup+=(--block-gas-limit "$BUDGET_GAS" -e "$FUND_ACCOUNTS")
  say "quake -f $eff ${setup[*]} (render only, in $ARC_DIR)"
  # validator1's data is written by the containers as ROOT: a plain rm -rf
  # leaves reth/store.db/wal behind and the CL crash-loops on the handshake
  # ("EL has blocks but CL has no committed state"). Wipe through a root container.
  x docker run --rm -v "$QDIR":/b --user root alpine sh -c \
    'rm -rf /b/validator*/reth /b/validator*/malachite/store.db /b/validator*/malachite/wal /b/full*/reth /b/full*/malachite'
  x rm -rf "$QDIR"
  if dry; then echo "  [dry] local: (cd $ARC_DIR && $QUAKE -f $eff ${setup[*]})"
  else
    [ -d "$QDIR" ] && die "could not remove $QDIR (root-owned leftovers?) — refusing to start on stale data"
    (cd "$ARC_DIR" && "$QUAKE" -f "$eff" "${setup[@]}") > "$LOCAL/quake-setup.log" 2>&1 \
      || { tail -20 "$LOCAL/quake-setup.log"; die "quake setup failed (see $LOCAL/quake-setup.log)"; }
    [ -f "$QDIR/compose.yaml" ] || die "quake setup produced no $QDIR/compose.yaml"
    if [ "$LANE" = evm ]; then
      local gl; gl=$(python3 -c "import json;print(int(json.load(open('$QDIR/assets/genesis.json'))['gasLimit'],16))" 2>/dev/null)
      [ "$gl" = "$BUDGET_GAS" ] || die "genesis gasLimit is ${gl:-unreadable}, want $BUDGET_GAS"
      say "genesis gasLimit = $gl"
    fi
  fi
  local ips; ips=$(for n in $(seq 1 "$N"); do printf '%s,' "$(ip_of "$n")"; done)
  say "splitting the compose: validator1 here, 2..$N on their own hosts"
  python3 "$FLEET_DIR/split-compose.py" --quake-dir "$QDIR" --run-id "$RUN_ID" \
    --fleet-root "$FLEET_ROOT" --ips "${ips%,}" $(dry && echo --dry-run) || die "compose split failed"
  dry || { cp "$QDIR"/compose-val*.yaml "$LOCAL/" 2>/dev/null; cp "$QDIR/compose.yaml" "$LOCAL/compose-val1.yaml"; }

  local rc=0 img gs rgs
  for n in $(seq 2 "$N"); do
    say "shipping to validator$n ($(name_of "$n"))"
    for img in $IMAGES; do ship_image "$n" "$img" || rc=1; done
    if dry; then echo "  [dry] v$n <- tar cz -C $QDIR assets validator$n logs -> $ROOT/quake (genesis sha verified)"
    else
      tar cz -C "$QDIR" assets "validator$n" logs 2>/dev/null | rpipe "$n" 600 "tar xz -C $ROOT/quake"
      gs=$(sha256sum "$QDIR/assets/genesis.json" | cut -c1-16)
      rgs=$(rsh "$n" "sha256sum $ROOT/quake/assets/genesis.json | cut -c1-16" | tail -1 | tr -dc '0-9a-f')
      rsh "$n" "test -f $ROOT/quake/validator$n/malachite/config/priv_validator_key.json && echo OK" | grep -q OK \
        || { echo "    quake/: validator$n keys missing after unpack"; rc=1; }
      [ "$gs" = "$rgs" ] && echo "    quake/: unpacked, genesis $gs" || { echo "    quake/: genesis MISMATCH ($rgs != $gs)"; rc=1; }
    fi
    ship_file "$n" "$QDIR/compose-val$n.yaml" "$ROOT/compose-val$n.yaml" || rc=1
    ship_file "$n" "$SPAM_SRC" "$ROOT/spammer" || rc=1
    if [ "$LANE" = lean ]; then
      ship_file "$n" "$LEAN_SRC" "$ROOT/lean-lane-node" || rc=1
      ship_file "$n" "$fund" "$ROOT/fund.txt" || rc=1
    fi
  done
  say "staging validator1 locally"
  x cp -f "$SPAM_SRC" "$ROOT/spammer"
  [ "$LANE" = lean ] && { x cp -f "$LEAN_SRC" "$ROOT/lean-lane-node"; x cp -f "$fund" "$ROOT/fund.txt"; }
  [ $rc = 0 ] || die "one or more transfers failed — nothing was started"

  if [ "$LANE" = evm ]; then
    say "LANE=evm: no lean nodes (stopping any survivor of an earlier run)"
    for n in $(seq 1 "$N"); do lean_kill "$n"; done
  else
    # lean nodes FIRST: a CL that boots while its lean node is down parks forever
    say "starting lean nodes (fresh datadirs)"
    for n in $(seq 1 "$N"); do lean_kill "$n"; done
    for n in $(seq 1 "$N"); do rsh "$n" "rm -rf $ROOT/lean && mkdir -p $ROOT/lean $ROOT/logs"; done
    for n in $(seq 1 "$N"); do rsh "$n" "$(lean_launch_cmd "$n")"; done
    for n in $(seq 1 "$N"); do
      lean_wait "$n" 90 || { rsh "$n" "tail -5 $ROOT/logs/lean.log"; die "lean node $n did not answer on $(lean_url "$n")"; }
    done
    if ! dry; then
      local g1; g1=$(lean_head 1 | jget result.commitment)
      for n in $(seq 2 "$N"); do
        [ "$(lean_head "$n" | jget result.commitment)" = "$g1" ] || die "lean genesis mismatch on validator$n (different fund file?)"
      done
      say "$N lean nodes up, genesis ${g1:0:18}..."
    fi
  fi

  say "starting CL+EL containers"
  x docker compose -f "$QDIR/compose.yaml" up -d
  for n in $(seq 2 "$N"); do rsh "$n" "docker compose -f $ROOT/compose-val$n.yaml up -d"; done
  if dry; then say "dry run: would now run the health gate$([ "$LANE" = evm ] && echo ' + EVM gas-limit gate') and status"; return 0; fi
  local ct
  for n in $(seq 1 "$N"); do
    ct=$(rsh "$n" "docker ps --format '{{.Names}}' | grep -c '^validator$n'" | tr -dc '0-9')
    [ "${ct:-0}" = 2 ] || die "validator$n: ${ct:-0}/2 containers running after compose up"
  done
  health_gate
  [ "$LANE" = evm ] && evm_gas_gate
  status
  say "up. next: $([ "$LANE" = evm ] && echo 'LANE=evm ')fleet load"
}

# ---------------------------------------------------------------- health gate
# Catches what agreement checks cannot: a PARKED CL ("Manual intervention",
# booted while its lean node was down), a crash-looping CL (RestartCount), a
# STALE EL still on an old certified chain (height too high right after boot),
# and a chain that is up but not advancing.
health_gate(){
  say "health gate"
  local bad=0 n ct parked i ok eh rc
  for n in $(seq 1 "$N"); do
    ct=$(rsh "$n" "docker ps --format '{{.Names}}' | grep -c '^validator$n'" | tr -dc '0-9')
    parked=$(cl_parked "$n")
    printf '    validator%s  containers %s/2  "Manual intervention" %s\n' "$n" "${ct:-0}" "${parked:-?}"
    [ "${ct:-0}" = 2 ] || bad=1
    [ "${parked:-1}" = 0 ] || bad=1
  done
  [ $bad = 0 ] || { for n in $(seq 1 "$N"); do echo "--- validator${n}_cl"; rsh "$n" "docker logs validator${n}_cl --tail 30 2>&1"; done; die "census/park gate failed"; }
  say "waiting for the chain(s) to reach height 3 on all $N (up to 180 s)"
  for i in $(seq 1 60); do
    ok=1
    for n in $(seq 1 "$N"); do
      [ "$LANE" = evm ] || [ "$(lean_height "$n")" -ge 3 ] 2>/dev/null || ok=0
      [ "$(el_height "$n")" -ge 3 ] 2>/dev/null || ok=0
    done
    [ $ok = 1 ] && break; sleep 3
  done
  [ $ok = 1 ] || { for n in $(seq 1 "$N"); do echo "validator$n lean=$(lean_height "$n") el=$(el_height "$n")"; done; die "chains did not reach height 3 on all $N"; }
  # freshness + stability
  for n in $(seq 1 "$N"); do
    eh=$(el_height "$n"); [ "$eh" -lt 60 ] || die "validator$n EL is NOT fresh (height $eh): stale data under an old certified chain"
    rc=$(cl_restarts "$n"); [ "${rc:-0}" = 0 ] || die "validator$n CL restarted $rc times during boot (see its log)"
  done
  # advancing, not merely non-zero — on the lane this run measures
  local -a a b
  local get=lean_height; [ "$LANE" = evm ] && get=el_height
  for n in $(seq 1 "$N"); do a[n]=$($get "$n"); done
  sleep 15
  for n in $(seq 1 "$N"); do b[n]=$($get "$n")
    printf '    validator%s %s %s -> %s\n' "$n" "$LANE" "${a[n]}" "${b[n]}"
    [ "${b[n]}" -gt "${a[n]}" ] || bad=1; done
  [ $bad = 0 ] || die "a $LANE chain is not advancing"
  say "health gate PASS"
}

# ---------------------------------------------------------------- EVM gas limit
# Read the limit back off a block the running chain PRODUCED (the genesis file
# is checked separately in up); on a mismatch fall back to governance.
evm_gas_gate(){
  local got h0
  h0=$(el_height 1); got=$(el_gas_limit 1)
  if [ "$got" = "$BUDGET_GAS" ]; then say "EVM gas-limit gate: block $h0 gasLimit=$got == BUDGET_GAS  PASS"; return 0; fi
  say "EVM gas-limit gate: block $h0 gasLimit=${got:-?}, want $BUDGET_GAS — submitting the governance tx"
  set_gas_limit "$BUDGET_GAS"
}

# set-gas-limit [gas] — ProtocolConfig.updateFeeParams on a LIVE chain (no
# restarts). Priced 4x over base fee, verified on a LATER block.
set_gas_limit(){
  local gas=${1:-$BUDGET_GAS} bf mx got i h
  export PATH="$PATH:$HOME/.foundry/bin"
  command -v cast >/dev/null 2>&1 || die "cast not on PATH (foundry) — needed for the governance tx"
  bf=$(rpc "$(el_url 1)" eth_getBlockByNumber '["latest",false]' | jget result.baseFeePerGas)
  [ -n "$bf" ] || die "could not read baseFeePerGas from $(el_url 1)"
  mx=$(( bf * 4 ))
  say "governance: updateFeeParams(20,200,5000,1,1e12,blockGasLimit=$gas) at gasPrice $mx"
  cast send "$PROTOCOL_CONFIG" "updateFeeParams((uint64,uint64,uint64,uint256,uint256,uint256))" \
    "(20,200,5000,1,1000000000000,$gas)" --private-key "$CTRL_KEY" --rpc-url "$(el_url 1)" \
    --timeout 90 --gas-price "$mx" --priority-gas-price $((mx / 2)) >/dev/null 2>&1
  for i in $(seq 1 20); do
    sleep 3; got=$(el_gas_limit 1); h=$(el_height 1)
    [ "$got" = "$gas" ] && { say "gas limit now $got, verified on block $h  PASS"; return 0; }
  done
  die "gas limit still ${got:-?} after the governance tx (want $gas)"
}
