# shellcheck shell=bash
# ops.sh — status, fault legs, log capture, teardown. Sourced by bench/fleet/fleet.

# ---------------------------------------------------------------- status
# Lean agreement: hash every node's block BYTES at the lowest common height.
status(){
  [ "$LANE" = evm ] && { evm_status; return $?; }
  echo "validator  host             EL height   lean height   head txs   lean head"
  local n nums=() el h c txs min distinct
  for n in $(seq 1 "$N"); do
    el=$(el_height "$n"); h=$(lean_head "$n"); c=$(echo "$h" | jget result.commitment); h=$(echo "$h" | jget result.number)
    nums+=("${h:-0}"); txs=$(block_txs "$n" "${h:-0}")
    printf 'validator%-1s  %-15s  %-10s  %-12s  %-9s  %s\n' "$n" "$(name_of "$n")" "$el" "${h:-?}" "${txs:-?}" "${c:0:18}..."
  done
  min=$(printf '%s\n' "${nums[@]}" | sort -n | head -1)
  if [ "${min:-0}" -gt 0 ] 2>/dev/null; then
    distinct=$(for n in $(seq 1 "$N"); do
      rpc "$(lean_url "$n")" arc_getBlockBytes "{\"number\":$min}" | jget result.blockBytes | sha256sum | cut -c1-16; done | sort -u | wc -l)
    [ "$distinct" = 1 ] && echo "agreement: all $N lean nodes byte-identical at height $min" \
                        || echo "DIVERGENCE: $distinct distinct lean blocks at height $min"
  fi
}

# EVM agreement: one head hash at the lowest common height, and the height
# spread printed with it — agreeing hashes with one node 200 blocks behind is
# a sync-follower, not agreement.
evm_status(){
  echo "validator  host             EL height   head txs   gasUsed/gasLimit   head hash"
  local n nums=() num gu glim ntx hash pct min max distinct
  for n in $(seq 1 "$N"); do
    read -r num gu glim ntx hash <<< "$(el_block_stats "$n" latest)"
    nums+=("${num:-0}")
    pct=$(python3 -c "print(f'{100*${gu:-0}/${glim:-1}:.0f}' if ${glim:-0} else '?')" 2>/dev/null)
    printf 'validator%-1s  %-15s  %-10s  %-9s  %-18s %s\n' \
      "$n" "$(name_of "$n")" "${num:-?}" "${ntx:-?}" "${gu:-?}/${glim:-?} (${pct}%)" "${hash:0:18}..."
  done
  min=$(printf '%s\n' "${nums[@]}" | sort -n | head -1)
  if [ "${min:-0}" -gt 0 ] 2>/dev/null; then
    distinct=$(for n in $(seq 1 "$N"); do
      el_block_stats "$n" "$(printf '0x%x' "$min")" | awk '{print $5}'; done | sort -u | wc -l)
    [ "$distinct" = 1 ] && echo "agreement: all $N ELs report one block hash at height $min" \
                        || echo "DIVERGENCE: $distinct distinct EL block hashes at height $min"
    max=$(printf '%s\n' "${nums[@]}" | sort -n | tail -1)
    echo "heights: min $min, max $max (spread $((max - min)))"
  fi
}

# ---------------------------------------------------------------- fault legs
# _tip <n> -> "<validator n lean height> <max lean height over the fleet>"
_tip(){ local n=$1 j h max=0 me=0
  for j in $(seq 1 "$N"); do h=$(lean_height "$j"); [ "${h:-0}" -gt "$max" ] && max=$h; [ "$j" = "$n" ] && me=${h:-0}; done
  echo "$me $max"; }

restart_cl(){ # restart-cl <n>: bounce the CL container; the lean node stays up
  local n=${1:?validator index} i me max before started0
  before=$(cl_restarts "$n"); started0=$(cl_started "$n")
  say "leg restart-cl $n (restarts=${before:-?}, StartedAt=$started0)"
  rsh "$n" "docker restart validator${n}_cl" >/dev/null 2>&1
  dry && return 0
  for i in $(seq 1 60); do
    read -r me max <<< "$(_tip "$n")"
    if [ $((max - me)) -le 3 ] && [ "$me" -gt 0 ]; then
      say "validator$n rejoined ($me/$max) after $((i*2)) s; parked=$(cl_parked "$n"); restarts $before -> $(cl_restarts "$n"); StartedAt $started0 -> $(cl_started "$n")"
      status; return 0; fi
    sleep 2
  done
  rsh "$n" "docker logs validator${n}_cl --tail 40 2>&1"; die "validator$n did not rejoin within 120 s"
}

kill_lean(){ # kill-lean <n> <secs>: the lean node dies; its CL must survive and re-sign
  [ "$LANE" = evm ] && die "kill-lean is a lean-lane leg; LANE=evm runs no lean node"
  local n=${1:?validator index} secs=${2:-60} i me max r0 h0 hmax0 hmax1
  r0=$(cl_restarts "$n"); h0=$(lean_height "$n"); hmax0=$(lean_height 1)
  say "leg kill-lean $n for ${secs}s (lean h=$h0, val1 h=$hmax0, CL restarts=$r0)"
  lean_kill "$n"
  dry || [ -z "$(lean_pid "$n")" ] || die "lean node $n still alive after kill"
  dry || sleep "$secs"
  hmax1=$(lean_height 1)
  say "  during the outage val1 lean went $hmax0 -> $hmax1 (+$((hmax1 - hmax0)))"
  rsh "$n" "$(lean_launch_cmd "$n")"
  lean_wait "$n" 90 || { rsh "$n" "tail -10 $ROOT/logs/lean.log"; die "lean node $n did not come back"; }
  dry && return 0
  for i in $(seq 1 90); do
    read -r me max <<< "$(_tip "$n")"
    if [ $((max - me)) -le 3 ] && [ "$me" -gt "$h0" ]; then
      say "validator$n lean caught up ($me/$max) after $((i*2)) s; CL restarts $r0 -> $(cl_restarts "$n"); parked=$(cl_parked "$n")"
      status; return 0; fi
    sleep 2
  done
  rsh "$n" "tail -20 $ROOT/logs/lean.log; docker logs validator${n}_cl --tail 30 2>&1"
  die "validator$n lean did not catch up after the outage"
}

# ---------------------------------------------------------------- logs
# `compose down` throws a container's log buffer away, so CL/EL logs are
# written to $ROOT/logs while the containers still exist. Idempotent.
capture_logs(){
  say "capturing CL/EL container logs on all $N machines (before any compose down)"
  local n report
  for n in $(seq 1 "$N"); do
    report=$(rsh "$n" "mkdir -p $ROOT/logs
      for kind in cl el; do
        c=validator${n}_\$kind
        if docker inspect \$c >/dev/null 2>&1; then
          if docker logs \$c > $ROOT/logs/\$c.log 2>&1; then echo \"\$kind:ok \$(wc -c < $ROOT/logs/\$c.log) bytes\"
          else echo \"\$kind:capture-failed\"; fi
        else echo \"\$kind:no-container\"; fi
      done")
    dry || echo "    validator$n: $(echo "$report" | tr '\n' ' ')"
  done
}

# Pull every machine's $ROOT/logs/*.log into $LOCAL/logs/validatorN/, verified
# by hashing the bytes back (not by an ssh exit code).
pull_logs(){
  say "pulling logs/ from all $N machines into $LOCAL/logs/validatorN/"
  if dry; then echo "  [dry] v2..v$N: tar cz -C $ROOT logs | tar xz (sha-verified per file)"; return 0; fi
  mkdir -p "$LOCAL/logs"
  local n dest manifest f want have tmp
  for n in $(seq 1 "$N"); do
    dest=$LOCAL/logs/validator$n; mkdir -p "$dest"
    manifest=$(rsh "$n" "cd $ROOT/logs 2>/dev/null && for f in *.log; do [ -f \"\$f\" ] && printf '%s %s\n' \"\$f\" \$(sha256sum \"\$f\" | cut -c1-16); done")
    [ -n "$manifest" ] || { echo "    validator$n: no logs/*.log (nothing captured yet, or unreachable)"; continue; }
    if [ "$n" = 1 ]; then cp -f "$ROOT"/logs/*.log "$dest/" 2>/dev/null
    else
      tmp=$(mktemp -d)
      # shellcheck disable=SC2086
      timeout 300 $SSH_CMD "$(ssh_target "$n")" "tar cz -C $ROOT logs" 2>/dev/null | tar xzf - -C "$tmp" 2>/dev/null
      cp -f "$tmp"/logs/*.log "$dest/" 2>/dev/null; rm -rf "$tmp"
    fi
    while read -r f want; do
      [ -n "$f" ] || continue
      have=$(sha256sum "$dest/$f" 2>/dev/null | cut -c1-16)
      if [ "$have" = "$want" ]; then echo "    validator$n/$f: $(wc -c < "$dest/$f") bytes (sha matches)"
      else echo "    validator$n/$f: MISMATCH (local ${have:-none} != remote $want) — re-run \`fleet logs\`"; fi
    done <<< "$manifest"
  done
}

# ---------------------------------------------------------------- down / purge
down(){
  local n left
  capture_logs
  pull_logs
  for n in $(seq 1 "$N"); do
    say "validator$n: compose down$([ "$LANE" = lean ] && echo ' + lean node stop')"
    rsh "$n" "docker compose -f $(compose_of "$n") down" >/dev/null 2>&1
    [ "$LANE" = lean ] && lean_kill "$n"
    dry && continue
    left=$(rsh "$n" "docker ps --format '{{.Names}}' | grep -c '^validator$n'" | tr -dc '0-9')
    echo "    containers left: ${left:-?}  lean pid left: '$(lean_pid "$n")'"
  done
  say "down. data kept under $ROOT on every machine (write the run record, then: fleet purge $RUN_ID --yes)"
}

# purge <run-id> --yes — delete $FLEET_ROOT/<run-id> on every machine. The
# containers wrote parts of it as root, so the removal runs in a root
# container. Refuses while any validator container is still running.
purge(){
  local id=${1:-} yes=${2:-} n ct
  [ -n "$id" ] && [ "$yes" = --yes ] || die "usage: fleet purge <run-id> --yes (deletes $FLEET_ROOT/<run-id> on all $N machines)"
  case "$id" in */*|.|..) die "bad run id '$id'";; esac
  for n in $(seq 1 "$N"); do
    dry && break
    ct=$(rsh "$n" "docker ps --format '{{.Names}}' | grep -c '^validator$n'" | tr -dc '0-9')
    [ "${ct:-0}" = 0 ] || die "validator$n still runs ${ct} validator containers — fleet down first"
  done
  for n in $(seq 1 "$N"); do
    rsh "$n" "docker run --rm -v $FLEET_ROOT:/r --user root alpine rm -rf /r/$id"
    dry || { rsh "$n" "test -e $FLEET_ROOT/$id && echo LEFT" | grep -q LEFT && die "validator$n: $FLEET_ROOT/$id survived"; }
    dry || echo "    validator$n: $FLEET_ROOT/$id removed"
  done
}
