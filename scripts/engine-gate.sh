#!/usr/bin/env bash
# engine-gate.sh — the one call an optimization loop (by hand or automated)
# makes to judge a lean-core engine change: correctness first, then speed.
#
#   scripts/engine-gate.sh                # full gate, ~60-90 s on the 16-thread host (warm build)
#   GATE_QUICK=1 scripts/engine-gate.sh   # small budgets (smoke the gate itself)
#
# Steps (all release builds):
#   1. golden   SPEC.md Appendix A vectors against the engine
#   2. diff     differential test engine (parallel + serial recovery) vs the
#               SPEC-only oracle: GATE_DIFF_CHAINS x 20 blocks of adversarial
#               content (default 4000 chains, 40-45 s here; tuned for <= 60 s)
#   3. fuzz     mutational decode fuzzer, GATE_FUZZ_ITERS iterations (1e6, ~8 s)
#   4. restart  straight vs replay vs snapshot-restart digests (32 chains)
#   5. bench    lean-bench --json: s1e6-n1-g450m, s1e6-n100-g450m,
#               s1e6-nmix-g450m (1e6 accounts, 450M gas, N=1 / N=100 / mix)
#               — skipped when a correctness step failed (GATE_BENCH_ALWAYS=1
#               runs it anyway; `correct` stays false).
#
# stdout: exactly ONE JSON line
#   {"correct": true|false, "payments_per_s_geomean": X|null,
#    "scenarios": {name: {payments_per_s, tx_per_s, ms_per_block, fullness}},
#    "failure": "" | "<first failure + reproducer>", "checks": {...}}
# Everything else (build output, progress) goes to stderr.
# Exit status: 0 iff correct and the bench produced numbers.
#
# Fixed seeds make the correctness budget identical run to run; change
# GATE_SEED to explore new cases (a failure prints its own reproducer).
set -uo pipefail
cd "$(cd "$(dirname "$0")/.." && pwd)" || exit 2

SEED=${GATE_SEED:-20260924}
DIFF_CHAINS=${GATE_DIFF_CHAINS:-4000}
DIFF_BLOCKS=${GATE_DIFF_BLOCKS:-20}
FUZZ_ITERS=${GATE_FUZZ_ITERS:-1000000}
RESTART_CHAINS=${GATE_RESTART_CHAINS:-32}
BENCH_RUNS=${GATE_BENCH_RUNS:-5}
JOBS=${GATE_JOBS:-8}
SCENARIOS=(s1e6-n1-g450m s1e6-n100-g450m s1e6-nmix-g450m)
if [ "${GATE_QUICK:-0}" = 1 ]; then
  DIFF_CHAINS=200; FUZZ_ITERS=50000; RESTART_CHAINS=4; BENCH_RUNS=1
fi
TARGET=${CARGO_TARGET_DIR:-target}
OUT=$(mktemp -d "${TMPDIR:-/tmp}/engine-gate.XXXXXX")
trap 'rm -rf "$OUT"' EXIT
log(){ echo "[engine-gate $(date +%H:%M:%S)] $*" >&2; }

emit_build_failure(){
  python3 - "$1" <<'PY'
import json, sys
print(json.dumps({"correct": False, "payments_per_s_geomean": None, "scenarios": {},
                  "failure": sys.argv[1], "checks": {}}))
PY
  exit 1
}

log "building release (nice, -j $JOBS)"
if ! nice -n 19 cargo build --release -j "$JOBS" -p lean-diff -p lean-bench >&2; then
  emit_build_failure "cargo build --release failed"
fi
LD=$TARGET/release/lean-diff
LB=$TARGET/release/lean-bench

# Each check writes its JSON line to $OUT/<name>.json (stderr passes through).
check(){
  local name=$1; shift
  local t0=$SECONDS
  log "$name: $*"
  "$LD" "$@" >"$OUT/$name.json"
  local rc=$?
  log "$name: exit $rc in $((SECONDS - t0)) s"
  [ -s "$OUT/$name.json" ] || echo "{\"check\":\"$name\",\"ok\":false,\"failure\":\"$name produced no output (exit $rc, crashed?)\"}" >"$OUT/$name.json"
  echo "$((SECONDS - t0))" >"$OUT/$name.secs"
  return $rc
}

correct=1
check golden golden || correct=0
check diff diff --seed "$SEED" --chains "$DIFF_CHAINS" --blocks "$DIFF_BLOCKS" || correct=0
check fuzz fuzz --seed "$SEED" --iters "$FUZZ_ITERS" || correct=0
check restart restart --seed "$SEED" --chains "$RESTART_CHAINS" --blocks 30 || correct=0

bench_ok=0
if [ $correct = 1 ] || [ "${GATE_BENCH_ALWAYS:-0}" = 1 ]; then
  args=()
  for s in "${SCENARIOS[@]}"; do args+=(--scenario "$s"); done
  log "bench: ${SCENARIOS[*]} ($BENCH_RUNS runs)"
  t0=$SECONDS
  if "$LB" "${args[@]}" --json --no-serial --runs "$BENCH_RUNS" --warmup 1 --blocks 3 \
      >"$OUT/bench.json"; then
    bench_ok=1
  fi
  echo "$((SECONDS - t0))" >"$OUT/bench.secs"
  log "bench: done in $((SECONDS - t0)) s (ok=$bench_ok)"
else
  log "bench: skipped (correctness failed)"
fi

python3 - "$OUT" "$correct" "$bench_ok" "${SCENARIOS[@]}" <<'PY'
import json, math, os, sys
out, correct, bench_ok, want = sys.argv[1], sys.argv[2] == "1", sys.argv[3] == "1", sys.argv[4:]

def load(name):
    p = os.path.join(out, name + ".json")
    try:
        with open(p) as f:
            txt = f.read().strip()
        return json.loads(txt.splitlines()[-1] if name != "bench" else txt)
    except Exception as e:  # missing / garbled
        return {"ok": False, "failure": f"{name}: unreadable output ({e})"}

def secs(name):
    try:
        return int(open(os.path.join(out, name + ".secs")).read())
    except Exception:
        return None

checks, failure = {}, ""
for name in ("golden", "diff", "fuzz", "restart"):
    r = load(name)
    ok = bool(r.get("ok"))
    c = {"ok": ok, "seconds": secs(name)}
    st = r.get("stats") or {}
    if name == "diff":
        c.update({k: st.get(k) for k in ("chains", "blocks", "entries", "items", "applied", "noops")})
    elif name == "fuzz":
        c.update({k: st.get(k) for k in ("iterations", "recoveries_compared", "blocks_well_framed")})
    elif name == "restart":
        c.update({k: st.get(k) for k in ("chains", "blocks", "restarts")})
    elif name == "golden":
        c["assertions"] = r.get("assertions")
    checks[name] = c
    if not ok and not failure:
        failure = f"{name}: " + str(r.get("report") or r.get("failure") or "failed")

scenarios, geo = {}, None
if bench_ok:
    b = load("bench")
    for s in b.get("scenarios", []):
        scenarios[s["scenario"]] = {
            "payments_per_s": round(s["payments_per_s"], 1),
            "tx_per_s": round(s["tx_per_s"], 1),
            "ms_per_block": round(s["ns_per_block"]["total"] / 1e6, 3),
            "fullness": round(s["fullness"], 5),
        }
    vals = [scenarios[n]["payments_per_s"] for n in want if n in scenarios]
    if len(vals) == len(want) and all(v > 0 for v in vals):
        geo = round(math.exp(sum(math.log(v) for v in vals) / len(vals)), 1)
    elif not failure:
        failure = f"bench: expected scenarios {want}, got {sorted(scenarios)}"
    checks["bench"] = {"ok": geo is not None, "seconds": secs("bench")}
elif correct and not failure:
    failure = "bench: lean-bench failed"
    checks["bench"] = {"ok": False, "seconds": secs("bench")}

print(json.dumps({
    "correct": correct and all(c["ok"] for k, c in checks.items() if k != "bench"),
    "payments_per_s_geomean": geo,
    "scenarios": scenarios,
    "failure": failure,
    "checks": checks,
}, separators=(",", ":")))
sys.exit(0 if (correct and geo is not None) else 1)
PY
