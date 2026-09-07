#!/usr/bin/env python3
"""Engine-bench analog for the lean lane: replay real canonical lean blocks
from a live node into a FRESH lean node via arc_newBlock (no consensus) and
report pure-EL import throughput in Mgas/s + outputs/s (gas = 21000+5000N/tx).
Usage: lean-replay-bench.py SRC_RPC TGT_RPC   (target: fresh datadir, same
chain-id + fund-file so genesis matches; blocks are pre-fetched so fetch time
never pollutes import timing; correctness = final blocks byte-identical)."""
import json, sys, time, urllib.request, base64
SRC, TGT = sys.argv[1], sys.argv[2]
def rpc(url, m, p, t=120):
    r=urllib.request.Request(url,data=json.dumps({"jsonrpc":"2.0","id":1,"method":m,"params":p}).encode(),headers={"content-type":"application/json"})
    return json.load(urllib.request.urlopen(r,timeout=t))["result"]
head = rpc(SRC,"arc_getHead",{})["number"]
blocks=[base64.b64decode(rpc(SRC,"arc_getBlockBytes",{"number":n})["blockBytes"]) for n in range(1,head+1)]
print(f"fetched {len(blocks)} blocks, {sum(map(len,blocks))/1e6:.1f} MB")
def stats(bb):
    ntx=int.from_bytes(bb[48:52],'little'); off=52; outs=gas=0
    for _ in range(ntx):
        l=int.from_bytes(bb[off:off+4],'little'); off+=4
        k=int.from_bytes(bb[off+5:off+7],'little'); outs+=k; gas+=21000+5000*k; off+=l
    return ntx,outs,gas
rows=[]; t0=time.time()
for bb in blocks:
    b64=base64.b64encode(bb).decode(); s=time.time()
    rpc(TGT,"arc_newBlock",{"blockBytes":b64}); rows.append((time.time()-s,)+stats(bb))
T=time.time()-t0
tx=sum(r[1] for r in rows); out=sum(r[2] for r in rows); gas=sum(r[3] for r in rows)
print(f"replayed {len(rows)} blocks in {T:.1f}s: {tx} txs, {out} outputs, {gas/1e9:.2f} Ggas")
print(f"AGGREGATE {gas/T/1e6:.0f} Mgas/s | {out/T:.0f} outputs/s | {tx/T:.0f} tx/s")
for name,lo,hi in (("FULL-300M",250e6,1e18),("FULL-50M",40e6,250e6)):
    cls=[r for r in rows if lo<r[3]<=hi]
    if cls:
        g=sum(r[3] for r in cls); t=sum(r[0] for r in cls); o=sum(r[2] for r in cls)
        print(f"{name}: n={len(cls)} {t/len(cls)*1000:.0f} ms/blk | {g/t/1e6:.0f} Mgas/s | {o/t:.0f} outputs/s | {t/o*1e6:.2f} us/output")
n=rpc(TGT,"arc_getHead",{})["number"]
same = rpc(SRC,"arc_getBlockBytes",{"number":n})["blockBytes"]==rpc(TGT,"arc_getBlockBytes",{"number":n})["blockBytes"]
print(f"correctness: block {n} byte-identical = {same}")
