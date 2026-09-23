#!/usr/bin/env python3
"""sampler.py — the fleet's per-minute sampler and its one set of RPC parsers.

    sampler.py run --lane lean|evm --hosts local,user@ip2,... --ssh "ssh ..." \
        --root <run root> --lean-urls u1,u2,.. --el-urls u1,u2,.. \
        --budget-txs 553 --fanout 50 --duration 600 [--pid <spammer pid>] \
        --out run.jsonl [--meta k=v ...]
    sampler.py parse block|gaslimit|elpool|leanpool|blocktxs   < json-rpc body
    sampler.py metrics <dh> <dt> <mean_txs> <mean_gas> <budget_txs> <gas_limit> <pay_per_tx>
    sampler.py --selftest

Every tick (default 60 s) it prints one row per the lane being measured:

    t  height  blk/min  headTxs  full%  tx/s  pay/s  Mgas/s  pool1/2/..  restarts1/2/..

plus a cpu line (CL/EL container %, lean node %), and — on the lean lane, when
the nodes run with `--stats` — one line from each node's latest `lean_stats`:
the share of this interval's anchors that took the `staged` route (the healthy
validator's path; a node that keeps pulling blocks from peers is losing the
staging race), peer pulls, and RSS. Fullness is by gas on the EVM lane (header
gasLimit) and by tx count against --budget-txs on the lean lane. Mgas/s on the
lean lane is derived (txs x (21000 + 5000 N)), not gas anyone charged.

One ssh round-trip per remote machine per tick, in parallel. Each row is also
appended to --out as JSON.
"""
from __future__ import annotations

import argparse
import base64
import json
import os
import shlex
import subprocess
import sys
import threading
import time
import urllib.request

# ------------------------------------------------------------------ parsers
# Each takes a JSON-RPC response body (str) and never raises: an unreadable
# value is a sentinel that cannot be mistaken for a real one (-1 for depths,
# "0 0 0 0 -" for a block) — a 0 % fullness from a broken reader looks like a
# starved chain.


def _result(body: str):
    return json.loads(body)["result"]


def _num(v) -> int:
    return int(v, 16) if isinstance(v, str) else int(v)


def parse_block(body: str) -> str:
    """EL block -> 'number gasUsed gasLimit ntx hash'."""
    try:
        b = _result(body)
        return f'{_num(b["number"])} {_num(b["gasUsed"])} {_num(b["gasLimit"])} {len(b["transactions"])} {b["hash"]}'
    except Exception:
        return "0 0 0 0 -"


def parse_gaslimit(body: str) -> str:
    try:
        return str(_num(_result(body)["gasLimit"]))
    except Exception:
        return "0"


def parse_elpool(body: str) -> str:
    """reth txpool_status: pending + queued (hex or decimal); -1 unreadable."""
    try:
        r = _result(body)
        return str(_num(r.get("pending", 0)) + _num(r.get("queued", 0)))
    except Exception:
        return "-1"


def parse_leanpool(body: str) -> str:
    """lean txpool_status: pending only (what a proposer can pack)."""
    try:
        return str(_num(_result(body).get("pending", 0)))
    except Exception:
        return "-1"


def parse_blocktxs(body: str) -> str:
    """lean block wire bytes: n_txs is u32 LE at bytes 48..52."""
    try:
        b = base64.b64decode(_result(body)["blockBytes"] or "")
        return str(int.from_bytes(b[48:52], "little") if len(b) >= 52 else 0)
    except Exception:
        return "0"


PARSERS = {
    "block": parse_block,
    "gaslimit": parse_gaslimit,
    "elpool": parse_elpool,
    "leanpool": parse_leanpool,
    "blocktxs": parse_blocktxs,
}


def lane_metrics(dh, dt, mt, mg, bt, gl, ppt) -> str:
    """-> 'blk_per_min tx_per_s pay_per_s Mgas_per_s full_pct'. The one place
    either lane's rates are computed. dt=0 yields zero rates."""
    r = dh / dt if dt > 0 else 0.0
    full = 100.0 * mg / gl if gl > 0 else (100.0 * mt / bt if bt > 0 else 0.0)
    return f"{r * 60:.2f} {r * mt:.0f} {r * mt * ppt:.0f} {r * mg / 1e6:.1f} {full:.0f}"


def parse_lean_stats(line: str) -> dict[str, int]:
    """One `lean_stats k=v ...` line (any log prefix) -> {k: int}."""
    i = line.find("lean_stats ")
    if i < 0:
        return {}
    out = {}
    for tok in line[i + len("lean_stats "):].split():
        k, sep, v = tok.partition("=")
        if sep and v.isdigit():
            out[k] = int(v)
    return out


ANCHOR_KEYS = ("anchor_index", "anchor_staged", "anchor_queued", "anchor_peer",
               "anchor_syncing", "anchor_bytes")


def stats_delta(prev: dict, cur: dict) -> dict:
    """Interval view of two lean_stats snapshots: staged share of anchors,
    pulls, grace misses; plus the gauges from `cur`."""
    if not cur:
        return {}
    d = lambda k: cur.get(k, 0) - prev.get(k, 0) if prev else cur.get(k, 0)
    anchors = sum(d(k) for k in ANCHOR_KEYS)
    return {
        "anchors": anchors,
        "staged_pct": round(100.0 * d("anchor_staged") / anchors, 1) if anchors > 0 else None,
        "peer_anchors": d("anchor_peer"),
        "block_pulls": d("block_pulls"),
        "grace_misses": d("anchor_grace_misses"),
        "rejected": d("rejected"),
        "rss_mb": cur.get("rss_mb"),
        "pending": cur.get("pending"),
        "sync_queue": cur.get("sync_queue"),
        "head": cur.get("head"),
    }


# ------------------------------------------------------------------ transport
def rpc(url: str, method: str, params, timeout: float = 25.0) -> str:
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(url, data=body, headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.read().decode()
    except Exception:
        return ""


def remote(host: str, ssh: list[str], script: str, timeout: float = 60.0) -> str:
    cmd = ["bash", "-c", script] if host == "local" else ssh + [host, script]
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout,
                              stdin=subprocess.DEVNULL).stdout
    except Exception:
        return ""


def machine_script(n: int, root: str, lane: str) -> str:
    """One round-trip: CL restart count, CL/EL docker stats, lean %cpu/RSS by
    BINARY PATH (never pkill -f), load1, latest lean_stats line."""
    s = f"""
echo "RST|$(docker inspect --format '{{{{.RestartCount}}}}' validator{n}_cl 2>/dev/null)"
docker stats --no-stream --format 'CT|{{{{.Name}}}}|{{{{.CPUPerc}}}}' validator{n}_cl validator{n}_el 2>/dev/null
p=$(ps -eo pid=,args= 2>/dev/null | awk -v b={root}/lean-lane-node '$2==b{{print $1}}' | head -1)
if [ -n "$p" ]; then ps -o %cpu=,rss= -p "$p" 2>/dev/null | awk '{{print "LEAN|"$1"|"$2}}'; else echo 'LEAN|0|0'; fi
awk '{{print "LOAD1|"$1}}' /proc/loadavg
"""
    if lane == "lean":
        s += f"tail -n 400 {root}/logs/lean.log 2>/dev/null | grep lean_stats | tail -1 | sed 's/^/STATS|/'\n"
    return s


def parse_machine(n: int, raw: str) -> dict:
    out = {"restarts": -1, "cl_cpu": 0.0, "el_cpu": 0.0, "lean_cpu": 0.0,
           "lean_rss_mb": 0.0, "load1": 0.0, "stats": {}}
    for line in raw.splitlines():
        f = line.split("|")
        try:
            if f[0] == "RST":
                out["restarts"] = int(f[1]) if f[1].strip().isdigit() else -1
            elif f[0] == "CT" and len(f) >= 3:
                key = "cl_cpu" if f[1] == f"validator{n}_cl" else "el_cpu" if f[1] == f"validator{n}_el" else None
                if key:
                    out[key] = float(f[2].rstrip("%") or 0)
            elif f[0] == "LEAN" and len(f) >= 3:
                out["lean_cpu"] = float(f[1] or 0)
                out["lean_rss_mb"] = round(float(f[2] or 0) / 1024, 1)
            elif f[0] == "LOAD1":
                out["load1"] = float(f[1])
            elif f[0] == "STATS":
                out["stats"] = parse_lean_stats(line)
        except (ValueError, IndexError):
            continue
    return out


def parallel(fn, items):
    res = [None] * len(items)

    def work(i, it):
        res[i] = fn(it)

    ts = [threading.Thread(target=work, args=(i, it)) for i, it in enumerate(items)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    return res


# ------------------------------------------------------------------ run
def _dash(v) -> str:
    return "-" if v is None else str(v)


def alive(pid: int | None) -> bool:
    if pid is None:
        return True
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def sample_chain(a) -> tuple[int, int, int, int, int]:
    """(height, head_txs, mean_txs_5blk, mean_gas_5blk, gas_limit) on validator1."""
    if a.lane == "evm":
        url = a.el_urls[0]
        h, _, _, txs, _ = parse_block(rpc(url, "eth_getBlockByNumber", ["latest", False])).split()
        h, txs = int(h), int(txs)
        rows = []
        for i in range(5):
            if h - i < 0:
                continue
            _, g, gl, t, _ = parse_block(rpc(url, "eth_getBlockByNumber", [hex(h - i), False])).split()
            if int(gl) > 0:
                rows.append((int(t), int(g), int(gl)))
        if not rows:
            return h, txs, 0, 0, 0
        k = len(rows)
        return h, txs, sum(r[0] for r in rows) // k, sum(r[1] for r in rows) // k, sum(r[2] for r in rows) // k
    url = a.lean_urls[0]
    try:
        h = int(json.loads(rpc(url, "arc_getHead", {}))["result"]["number"])
    except Exception:
        h = 0
    counts = [int(parse_blocktxs(rpc(url, "arc_getBlockBytes", {"number": h - i}))) for i in range(5) if h - i > 0]
    counts = [c for c in counts if c > 0]
    mean = sum(counts) // len(counts) if counts else 0
    return h, counts[0] if counts else 0, mean, mean * (21000 + 5000 * a.fanout), 0


def run(a) -> int:
    ssh = shlex.split(a.ssh)
    n_nodes = len(a.hosts)
    meta = dict(kv.split("=", 1) for kv in a.meta)
    nproc = parallel(lambda h: remote(h, ssh, "nproc").strip() or "0", a.hosts)
    print(f"nproc: {' '.join(f'v{i + 1}={p}' for i, p in enumerate(nproc))}")
    hcol = "elH" if a.lane == "evm" else "leanH"
    npad = 4 * n_nodes + 4
    print(f'{"t(s)":<6} {hcol:<7} {"blk/min":<9} {"headTxs":<9} {"full%":<7} {"tx/s":<9} '
          f'{"pay/s":<9} {"Mgas/s":<9} {"pool" + "/".join(str(i + 1) for i in range(n_nodes)):<{npad}} restarts')
    t0 = time.time()
    prev_h, prev_t, s = -1, 0.0, 0
    prev_stats = [{} for _ in a.hosts]
    ppt = 1 if a.lane == "evm" else a.fanout
    with open(a.out, "a") as jf:
        while True:
            deadline = t0 + (s + 1) * a.interval
            while time.time() < deadline and alive(a.pid):
                time.sleep(min(1.0, max(0.0, deadline - time.time())))
            if not alive(a.pid):
                break
            s += 1
            t = round(time.time() - t0)
            h, txs, mean, mean_gas, gl = sample_chain(a)
            if a.lane == "evm":
                pools = parallel(lambda u: int(parse_elpool(rpc(u, "txpool_status", []))), a.el_urls)
            else:
                pools = parallel(lambda u: int(parse_leanpool(rpc(u, "txpool_status", {}))), a.lean_urls)
            machines = parallel(
                lambda it: parse_machine(it[0] + 1, remote(it[1], ssh, machine_script(it[0] + 1, a.root, a.lane))),
                list(enumerate(a.hosts)))
            dh = h - prev_h if prev_h >= 0 else 0
            dt = t - prev_t if prev_h >= 0 else 0
            bpm, tps, pps, mgs, full = lane_metrics(dh, dt, mean, mean_gas, a.budget_txs, gl, ppt).split()
            rst = [m["restarts"] for m in machines]
            print(f'{t:<6} {h:<7} {bpm:<9} {txs:<9} {full:<7} {tps:<9} {pps:<9} {mgs:<9} '
                  f'{"/".join(map(str, pools)):<{npad}} {"/".join(map(str, rst))}')
            print("       cpu cl/el/lean " + "  ".join(
                f'v{i + 1} {m["cl_cpu"]:.0f}/{m["el_cpu"]:.0f}/{m["lean_cpu"]:.0f}' for i, m in enumerate(machines)))
            deltas = [stats_delta(prev_stats[i], m["stats"]) for i, m in enumerate(machines)]
            if any(deltas):
                print("       lean staged%/peer-pulls/rss_mb " + "  ".join(
                    f'v{i + 1} {_dash(d.get("staged_pct"))}/{_dash(d.get("block_pulls"))}/{_dash(d.get("rss_mb"))}'
                    if d else f"v{i + 1} -" for i, d in enumerate(deltas)))
            prev_stats = [m["stats"] or prev_stats[i] for i, m in enumerate(machines)]
            row = {
                "t": t, "sample": s, "lane": a.lane, "height": h, "blk_per_min": bpm, "head_txs": txs,
                "mean_txs_5blk": mean, "mean_gas_5blk": mean_gas, "gas_limit": gl, "full_pct": full,
                "tx_per_s": tps, "payments_per_s": pps, "mgas_per_s": mgs, "pool": pools, "restarts": rst,
                "cl_cpu": [m["cl_cpu"] for m in machines], "el_cpu": [m["el_cpu"] for m in machines],
                "lean_cpu": [m["lean_cpu"] for m in machines], "lean_rss_mb": [m["lean_rss_mb"] for m in machines],
                "load1": [m["load1"] for m in machines], "nproc": [int(p) if p.isdigit() else 0 for p in nproc],
                "lean_stats": deltas, "budget_txs": a.budget_txs, "fanout": a.fanout, **meta,
            }
            if a.lane == "lean":
                row["lean_height"] = h
            jf.write(json.dumps(row) + "\n")
            jf.flush()
            prev_h, prev_t = h, t
            if t >= a.duration:
                break
    print(f"samples -> {a.out}")
    return 0


# ------------------------------------------------------------------ selftest
def selftest() -> int:
    fails = []

    def eq(what, want, got):
        print(f'  {"ok  " if want == got else "FAIL"} {what}')
        if want != got:
            fails.append(f"{what}: want {want!r} got {got!r}")

    eq("lean_stats parse with a tracing prefix",
       {"head": 812, "anchor_staged": 790, "rss_mb": 1842, "block_pulls": 10},
       {k: v for k, v in parse_lean_stats(
           "2026-09-22T10:00:00.000Z  INFO lean_lane_node::node: lean_stats head=812 accounts=44913 staged=1 "
           "rss_mb=1842 anchor_staged=790 block_pulls=10").items() if k in ("head", "anchor_staged", "rss_mb", "block_pulls")})
    eq("no lean_stats -> {}", {}, parse_lean_stats("INFO something else"))
    prev = {"anchor_staged": 100, "anchor_peer": 2, "anchor_index": 0, "block_pulls": 2}
    cur = {"anchor_staged": 195, "anchor_peer": 7, "anchor_index": 0, "block_pulls": 7, "rss_mb": 900}
    d = stats_delta(prev, cur)
    eq("staged share of this interval's anchors", 95.0, d["staged_pct"])
    eq("peer pulls in the interval", 5, d["block_pulls"])
    eq("no anchors -> staged_pct None", None, stats_delta(cur, cur)["staged_pct"])
    raw = ("RST|0\nCT|validator2_cl|153.2%\nCT|validator2_el|20.5%\nLEAN|87.0|1048576\nLOAD1|3.10\n"
           "STATS|2026 INFO lean_stats head=5 anchor_staged=4\n")
    m = parse_machine(2, raw)
    eq("machine: restarts/cpu/rss/load/stats",
       (0, 153.2, 20.5, 87.0, 1024.0, 3.1, 5),
       (m["restarts"], m["cl_cpu"], m["el_cpu"], m["lean_cpu"], m["lean_rss_mb"], m["load1"], m["stats"]["head"]))
    eq("machine: unreachable -> restarts -1", -1, parse_machine(1, "")["restarts"])
    eq("blocktxs from wire bytes", "7",
       parse_blocktxs(json.dumps({"result": {"blockBytes": base64.b64encode(b"\0" * 48 + (7).to_bytes(4, "little")).decode()}})))
    eq("blocktxs on null bytes", "0", parse_blocktxs('{"result":{"blockBytes":null}}'))
    eq("leanpool", "1974", parse_leanpool('{"result":{"pending":1974,"queued":0}}'))
    eq("leanpool error -> -1", "-1", parse_leanpool('{"error":{}}'))
    if fails:
        print("\n".join(fails), file=sys.stderr)
        return 1
    print("sampler.py selftest: all checks passed")
    return 0


def main(argv) -> int:
    if argv[:1] == ["--selftest"]:
        return selftest()
    if argv[:1] == ["parse"] and len(argv) == 2 and argv[1] in PARSERS:
        print(PARSERS[argv[1]](sys.stdin.read()))
        return 0
    if argv[:1] == ["metrics"] and len(argv) == 8:
        v = [float(x) for x in argv[1:]]
        print(lane_metrics(*v))
        return 0
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--lane", choices=("lean", "evm"), required=True)
    r.add_argument("--hosts", required=True, type=lambda s: s.split(","),
                   help="validator1..N ssh targets, 'local' for this machine")
    r.add_argument("--ssh", default="ssh -o BatchMode=yes")
    r.add_argument("--root", required=True)
    r.add_argument("--lean-urls", required=True, type=lambda s: s.split(","))
    r.add_argument("--el-urls", required=True, type=lambda s: s.split(","))
    r.add_argument("--budget-txs", type=int, required=True)
    r.add_argument("--fanout", type=int, default=1)
    r.add_argument("--duration", type=int, required=True)
    r.add_argument("--interval", type=int, default=60)
    r.add_argument("--pid", type=int)
    r.add_argument("--out", required=True)
    r.add_argument("--meta", action="append", default=[])
    a = ap.parse_args(argv)
    return run(a)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
