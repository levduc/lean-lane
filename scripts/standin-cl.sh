# standin-cl.sh — sourced, not run. The minimal stand-in for the consensus
# layer shared by lean-smoke.sh (host processes) and image-smoke.sh
# (containers): round-robin proposer, arc_buildBlock on the proposer,
# arc_newBlock on every node.
#
# Caller sets: PORTS=(...) host ports of the node RPCs on 127.0.0.1,
#              RUN (scratch dir), BUDGET (gas per block).
# Needs: curl, python3.

rpc(){ # rpc <port> <method> <json-params>
  # body goes through a FILE, not argv: a budget-full block's base64 is ~1 MB and
  # blows the exec argument limit when passed inline.
  local body="$RUN/.req"
  printf '{"jsonrpc":"2.0","id":1,"method":"%s","params":%s}' "$2" "$3" > "$body"
  curl -s -m 30 -X POST "http://127.0.0.1:$1" -H 'content-type: application/json' --data-binary "@$body"; }

rpc_file(){ # rpc_file <port> <method> <params-file>
  local body="$RUN/.reqf"
  { printf '{"jsonrpc":"2.0","id":1,"method":"%s","params":' "$2"; cat "$3"; printf '}'; } > "$body"
  curl -s -m 60 -X POST "http://127.0.0.1:$1" -H 'content-type: application/json' --data-binary "@$body"; }

head_field(){ # head_field <port> <number|commitment>
  rpc "$1" arc_getHead '{}' | python3 -c "import sys,json;print(json.load(sys.stdin)['result']['$2'])"; }

# drive_heights <count> — sets txs, outs, gas (totals over the driven blocks).
# Returns 1 (after printing FAIL) if a build or an import is rejected.
drive_heights(){
  local h prop head pc pn ts built bytes p r stat
  txs=0; outs=0; gas=0
  for h in $(seq 1 "$1"); do
    prop=${PORTS[$(( (h-1) % ${#PORTS[@]} ))]}
    head=$(rpc "$prop" arc_getHead '{}')
    pc=$(echo "$head" | python3 -c 'import sys,json;r=json.load(sys.stdin)["result"];print(r["commitment"])')
    pn=$(echo "$head" | python3 -c 'import sys,json;r=json.load(sys.stdin)["result"];print(r["number"])')
    ts=$(( $(date +%s) * 1000 ))
    built=$(rpc "$prop" arc_buildBlock "{\"parentCommitment\":\"$pc\",\"number\":$((pn+1)),\"timestampMs\":$ts,\"budgetGas\":$BUDGET}")
    bytes=$(echo "$built" | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["blockBytes"])' 2>/dev/null)
    [ -n "${bytes:-}" ] || { echo "FAIL: buildBlock at height $h"; echo "$built" | head -c 200; return 1; }
    printf '{"blockBytes":"%s"}' "$bytes" > "$RUN/.blk"
    for p in "${PORTS[@]}"; do
      r=$(rpc_file "$p" arc_newBlock "$RUN/.blk")
      echo "$r" | grep -qE '"commitment"|SYNCING' || { echo "FAIL: newBlock rejected on :$p at height $h"; echo "$r" | head -c 200; return 1; }
    done
    stat=$(python3 - "$RUN/.blk" <<'PY'
import base64,sys
import json
bb=base64.b64decode(json.load(open(sys.argv[1]))['blockBytes']); ntx=int.from_bytes(bb[48:52],'little'); off=52; o=0; g=0
for _ in range(ntx):
    l=int.from_bytes(bb[off:off+4],'little'); off+=4
    n=int.from_bytes(bb[off+5:off+7],'little'); o+=n; g+=21000+5000*n; off+=l
print(f"{ntx} {o} {g}")
PY
)
    txs=$((txs + $(echo $stat | cut -d' ' -f1))); outs=$((outs + $(echo $stat | cut -d' ' -f2)))
    gas=$((gas + $(echo $stat | cut -d' ' -f3)))
  done
}

# collect_heads — sets heads=(...) comms=(...) and uniq_c (distinct commitments).
collect_heads(){
  local p r
  heads=(); comms=()
  for p in "${PORTS[@]}"; do
    r=$(rpc "$p" arc_getHead '{}')
    heads+=("$(echo "$r" | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["number"])')")
    comms+=("$(echo "$r" | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["commitment"])')")
  done
  uniq_c=$(printf '%s\n' "${comms[@]}" | sort -u | wc -l)
}
