#!/usr/bin/env python3
"""Batch feeder for the lean lane: arc_sendRawTxBatch (length-framed base64),
pool-governed. Usage: lean-feeder.py TARGET CORPUS POOL_TARGET RATE DURATION"""
import base64, json, sys, time, urllib.request

target, corpus, pool_target, rate, duration = (
    sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]))
BATCH = 4000  # 4k/batch measured ~29k tx/s offered from one thread

def rpc(method, params):
    req = urllib.request.Request(
        target, json.dumps({"jsonrpc": "2.0", "id": 1, "method": method,
                            "params": params}).encode(),
        {"Content-Type": "application/json"})
    out = json.load(urllib.request.urlopen(req, timeout=30))
    if "error" in out:
        raise RuntimeError(out["error"])
    return out["result"]

def pool_depth():
    """PENDING only. Queued txs are ones whose nonces run ahead of the chain;
    counting them made the governor throttle the feed long before the lane was
    saturated (measured 2026-08-24: N=1 capped at ~4.5k tx/s landed while one
    thread can offer 29k). The builder only draws from pending, so pending is
    the depth that matters."""
    try:
        s = rpc("txpool_status", [])
        return int(s["pending"])
    except Exception:
        return -1

deadline = time.time() + duration
sent = acc = rej = 0
t0 = time.time()
batch = []

def flush(batch):
    global acc, rej
    blob = bytearray()
    for tx in batch:
        b = bytes.fromhex(tx[2:] if tx.startswith("0x") else tx)
        blob += len(b).to_bytes(4, "little") + b
    r = rpc("arc_sendRawTxBatch",
            [base64.b64encode(bytes(blob)).decode()])
    acc += int(r["accepted"]); rej += int(r["rejected"])

with open(corpus) as fh:
    for line in fh:
        line = line.strip()
        if not line:
            continue
        if time.time() > deadline:
            break
        batch.append(line)
        if len(batch) >= BATCH:
            while pool_depth() >= pool_target and time.time() < deadline:
                time.sleep(0.5)
            due = t0 + sent / max(rate, 1)
            if due > time.time():
                time.sleep(due - time.time())
            flush(batch)
            sent += len(batch)
            batch = []
if batch and time.time() <= deadline:
    flush(batch); sent += len(batch)
print(f"FEEDER_DONE sent={sent} accepted={acc} rejected={rej} "
      f"elapsed={time.time()-t0:.0f}s", flush=True)
