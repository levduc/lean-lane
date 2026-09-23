# shellcheck shell=bash
# load.sh — `fleet load`: offered load + the per-minute sampler. Sourced by
# bench/fleet/fleet.
#
# DISTRIBUTED=1 (default): one spammer per machine against ITS OWN node over
# loopback, disjoint account ranges. The lean lane does not propagate
# transactions (a proposer packs what its own pool holds), and a single remote
# spammer is delivery-bound (send rate = generators / RTT) long before the
# chain is — the old "~6k tx/s on both lanes" was exactly that artefact.
# DISTRIBUTED=0 drives every node from this machine (lean lane only).

load(){
  x mkdir -p "$LOCAL"
  say "effective params: LANE=$LANE BUDGET_GAS=$BUDGET_GAS BUDGET_TXS=$BUDGET_TXS FANOUT=$FANOUT LOAD_SECS=$LOAD_SECS LOAD_RATE=$LOAD_RATE POOL_TARGET=$POOL_TARGET GENERATORS=$GENERATORS DISTRIBUTED=$DISTRIBUTED FUND_ACCOUNTS=$FUND_ACCOUNTS"
  say "  full block at this budget = $BUDGET_TXS txs (a number without fullness is not a result)"
  local n per off target chain mix sp=""
  per=$((FUND_ACCOUNTS / N))
  [ "$per" -ge 1 ] || die "FUND_ACCOUNTS=$FUND_ACCOUNTS is fewer than $N accounts"
  [ $((per % GENERATORS)) = 0 ] || say "  WARNING: -a $per is not divisible by -g $GENERATORS"
  if [ "$LANE" = evm ]; then
    [ "$DISTRIBUTED" = 1 ] || die "LANE=evm always runs one spammer per machine (DISTRIBUTED=1)"
    chain=$EVM_CHAIN; mix="--mix transfer=100"
    evm_static_gate
  else
    chain=$CHAIN; mix="--mix fanout=100 --fanout-outputs $FANOUT"
  fi

  if [ "$DISTRIBUTED" = 1 ]; then
    say "load: one spammer per machine, $LOAD_RATE tx/s x ${LOAD_SECS}s each, $per accounts each, -g $GENERATORS"
    for n in $(seq 1 "$N"); do
      off=$(( (n - 1) * per ))
      if [ "$LANE" = evm ]; then target=ws://127.0.0.1:$(el_ws_port "$n"); else target=ws://127.0.0.1:$LEAN_PORT; fi
      rsh "$n" "(setsid nohup $ROOT/spammer ws --targets $target -r $LOAD_RATE -g $GENERATORS -a $per --account-offset $off -t $LOAD_SECS --pool-target $POOL_TARGET --chain-id $chain $mix -l > $ROOT/logs/spammer.log 2>&1 < /dev/null &)"
      dry || echo "    validator$n: accounts [$off,$((off + per - 1))] -> $target"
    done
  else
    local targets=""
    for n in $(seq 1 "$N"); do targets="${targets}ws://$(ip_of "$n"):$LEAN_PORT,"; done
    say "load: one spammer HERE -> all $N nodes, $LOAD_RATE tx/s x ${LOAD_SECS}s, $FUND_ACCOUNTS accounts"
    if dry; then echo "  [dry] local: $SPAM_SRC ws --targets ${targets%,} ... > $LOCAL/spammer.log &"
    else
      "$SPAM_SRC" ws --targets "${targets%,}" -r "$LOAD_RATE" -g "$GENERATORS" -a "$FUND_ACCOUNTS" -t "$LOAD_SECS" \
        --pool-target "$POOL_TARGET" --chain-id "$CHAIN" $mix -l > "$LOCAL/spammer.log" 2>&1 &
      sp=$!
    fi
  fi
  run_sampler "$LOAD_SECS" "$sp"
  [ -n "$sp" ] && { wait "$sp" 2>/dev/null; tail -5 "$LOCAL/spammer.log"; }
  if [ "$DISTRIBUTED" = 1 ]; then
    for n in $(seq 1 "$N"); do dry || echo "--- validator$n spammer"; rsh "$n" "tail -2 $ROOT/logs/spammer.log"; done
  fi
  dry || status
}

# run_sampler <secs> [pid] — the per-minute sampler (sampler.py) in the foreground
run_sampler(){
  local secs=$1 pid=${2:-} hosts="" urls_lean="" urls_el="" n
  for n in $(seq 1 "$N"); do
    if [ "$n" = 1 ]; then hosts="local"; else hosts="$hosts,$(ssh_target "$n")"; fi
    urls_lean="$urls_lean,$(lean_url "$n")"; urls_el="$urls_el,$(el_url "$n")"
  done
  local args=(run --lane "$LANE" --hosts "$hosts" --ssh "$SSH_CMD" --root "$ROOT"
    --lean-urls "${urls_lean#,}" --el-urls "${urls_el#,}" --budget-txs "$BUDGET_TXS"
    --fanout "$FANOUT" --duration "$secs" --interval "$SAMPLE_SECS" --out "$LOCAL/run.jsonl"
    --meta "budget_gas=$BUDGET_GAS" --meta "load_secs=$LOAD_SECS" --meta "load_rate=$LOAD_RATE"
    --meta "pool_target=$POOL_TARGET" --meta "generators=$GENERATORS" --meta "distributed=$DISTRIBUTED")
  [ -n "$pid" ] && args+=(--pid "$pid")
  if dry; then echo "  [dry] local: python3 $SAMPLER ${args[*]}"; return 0; fi
  python3 "$SAMPLER" "${args[@]}"
}

# `-l` reads each account's nonce once at start; a surviving feeder moving the
# nonces makes that stream born stale (queued forever). Warn, don't die: the
# operator may be stacking load deliberately.
evm_static_gate(){
  dry && { echo "  [dry] chain-static gate: validator1 EL pool must stay empty for 10 s"; return 0; }
  local n1 n2
  n1=$(el_pool_depth 1); sleep 10; n2=$(el_pool_depth 1)
  if [ "${n1:--1}" -gt 0 ] 2>/dev/null || [ "${n2:--1}" -gt 0 ] 2>/dev/null; then
    say "  WARNING: validator1 pool is NOT empty ($n1 -> $n2 over 10 s): a feeder is alive; -l nonce reads may be stale"
  else
    say "  chain-static gate: validator1 pool empty ($n1 -> $n2), -l nonce reads are safe"
  fi
}
