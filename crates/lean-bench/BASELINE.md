# lean-bench baseline — 2026-09-24

The lean lane engine's standalone ceiling before any optimization: `lean-core` at
commit `76d881a` (branch `engine-core`), which is the pre-refactor node's execution
logic moved without changes (proof: identical replay digest, see the step-2 commit).

**Host.** i7-11700F, 8 cores / 16 threads (CPUs 0-7 are distinct physical cores),
62 GB RAM, Linux 7.0, rustc 1.96, `--release` (thin LTO). The host was otherwise idle.
**One run of the whole suite, no repeat (n=1)**: each number is the median of 5
measured runs (plus 1 warm-up) of 3 consecutive blocks, not a mean over separate
invocations. Treat differences under ~5 % as noise.

Commands (raw JSON in `baseline/`):

```bash
taskset -c 0-7 lean-bench --all --json --threads 8           > baseline/engine-8t.json
lean-bench --all --json --threads 16 --no-serial             > baseline/engine-16t.json
taskset -c 0-7 lean-bench --scenario s1e6-n1-g450m --scenario s1e6-n100-g450m \
    --threads 8 --runs 3 --micro --json                      > baseline/micro-s1e6-g450m.json
lean-bench e2e --seconds 10 [--no-fsync] --json              > baseline/e2e-*.json
```

## What is measured

Engine only: `lean_core::process_block` + `lean_core::apply` on one thread driving a
rayon pool, against an in-memory pre-funded `FlatState`. Blocks are pre-signed (signing
is never timed), cached under `target/lean-bench-cache/`, and **full**: not even the
smallest drawable tx fits, so fullness is 99.8-100 %. One tx per sender per block (40k
keyed senders); recipients uniform over all accounts. Zero no-ops (asserted). No I/O,
no pool, no consensus: this is the ceiling of the block-processing code alone, not of
the lane.

- **payments/s 8t**: headline, `taskset -c 0-7 --threads 8`.
- **payments/s 16t**: same, 16 rayon threads (SMT), no pinning.
- **serial-rec**: frame+decode + serial recovery + execute + apply (recovery on the
  calling thread, as with `LEAN_PARALLEL_RECOVERY=0` before it was removed).
- phase columns: ms per block, 8 threads; `blk ms` = process_block + apply.
- peak RSS: VmHWM during that scenario, the state included (1e7 accounts ≈ 1.04 GB of
  `HashMap<Address, Acct>`, built in 1.2 s; 1e6 ≈ 130 MB).

## Engine results

| scenario | tx/blk | full % | payments/s 8t | tx/s 8t | payments/s 16t | payments/s serial-rec | blk ms 8t | frame+dec | recover | execute | apply | peak RSS MB |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| s1e5-n1-g225m | 8653 | 99.99 | 202,633 | 202,633 | 214,230 | 33,221 | 42.7 | 2.3 | 36.2 | 4.1 | 0.4 | 49 |
| s1e5-n1-g450m | 17307 | 100.00 | 203,067 | 203,067 | 216,609 | 33,180 | 85.2 | 4.6 | 72.0 | 8.3 | 0.8 | 70 |
| s1e5-n1-g1g | 38461 | 100.00 | 204,659 | 204,659 | 219,172 | 33,100 | 187.9 | 10.0 | 159.8 | 17.2 | 1.6 | 108 |
| s1e5-n10-g225m | 3169 | 100.00 | 1,258,990 | 125,899 | 1,343,247 | 299,740 | 25.2 | 2.6 | 13.6 | 7.9 | 0.8 | 65 |
| s1e5-n10-g450m | 6338 | 100.00 | 1,338,803 | 133,880 | 1,410,873 | 303,107 | 47.3 | 5.1 | 26.9 | 13.6 | 1.4 | 76 |
| s1e5-n10-g1g | 14084 | 100.00 | 1,400,517 | 140,052 | 1,491,314 | 306,218 | 100.6 | 11.2 | 59.2 | 26.0 | 2.2 | 113 |
| s1e5-n100-g225m | 431 | 99.80 | 2,696,593 | 26,966 | 2,852,391 | 1,529,616 | 16.0 | 2.7 | 2.5 | 9.7 | 0.9 | 74 |
| s1e5-n100-g450m | 863 | 99.92 | 2,877,547 | 28,775 | 2,906,370 | 1,530,294 | 30.0 | 5.3 | 4.6 | 19.8 | 1.6 | 99 |
| s1e5-n100-g1g | 1919 | 99.98 | 3,649,755 | 36,498 | 3,722,766 | 1,759,310 | 52.6 | 11.7 | 9.6 | 28.5 | 2.4 | 120 |
| s1e5-nmix-g225m | 1271 | 100.00 | 2,089,312 | 66,957 | 2,177,946 | 757,305 | 19.0 | 2.7 | 5.9 | 9.5 | 0.9 | 76 |
| s1e5-nmix-g450m | 2449 | 100.00 | 2,328,036 | 71,524 | 2,507,576 | 811,769 | 34.2 | 5.3 | 10.9 | 15.3 | 1.5 | 86 |
| s1e5-nmix-g1g | 5408 | 100.00 | 2,670,053 | 81,444 | 2,825,600 | 842,556 | 66.4 | 11.6 | 23.9 | 28.9 | 2.3 | 113 |
| s1e6-n1-g225m | 8653 | 99.99 | 193,789 | 193,789 | 211,227 | 33,152 | 44.7 | 2.3 | 35.9 | 5.6 | 0.7 | 200 |
| s1e6-n1-g450m | 17307 | 100.00 | 193,282 | 193,282 | 208,167 | 33,014 | 89.5 | 4.5 | 71.5 | 11.0 | 2.4 | 210 |
| s1e6-n1-g1g | 38461 | 100.00 | 192,391 | 192,391 | 162,508 | 32,862 | 199.9 | 10.1 | 158.7 | 23.7 | 5.6 | 244 |
| s1e6-n10-g225m | 3169 | 100.00 | 1,056,214 | 105,621 | 1,018,820 | 284,716 | 30.0 | 2.6 | 13.7 | 11.6 | 2.2 | 196 |
| s1e6-n10-g450m | 6338 | 100.00 | 1,038,958 | 103,896 | 1,018,212 | 284,003 | 61.0 | 5.1 | 26.9 | 23.3 | 5.0 | 224 |
| s1e6-n10-g1g | 14084 | 100.00 | 1,026,285 | 102,628 | 1,001,340 | 281,295 | 137.2 | 11.1 | 59.2 | 56.7 | 10.9 | 280 |
| s1e6-n100-g225m | 431 | 99.80 | 2,004,794 | 20,048 | 2,063,596 | 1,264,703 | 21.5 | 2.7 | 2.5 | 13.6 | 2.8 | 236 |
| s1e6-n100-g450m | 863 | 99.92 | 1,964,651 | 19,647 | 1,996,014 | 1,252,867 | 43.9 | 5.3 | 4.6 | 27.4 | 6.3 | 236 |
| s1e6-n100-g1g | 1919 | 99.98 | 1,971,972 | 19,720 | 1,991,865 | 1,240,300 | 97.3 | 11.6 | 9.8 | 62.7 | 13.4 | 283 |
| s1e6-nmix-g225m | 1271 | 100.00 | 1,634,324 | 52,376 | 1,660,591 | 687,939 | 24.3 | 2.7 | 5.9 | 13.0 | 2.7 | 207 |
| s1e6-nmix-g450m | 2449 | 100.00 | 1,624,032 | 49,895 | 1,674,837 | 701,419 | 49.1 | 5.3 | 11.1 | 26.2 | 6.0 | 232 |
| s1e6-nmix-g1g | 5408 | 100.00 | 1,597,242 | 48,720 | 1,629,776 | 695,070 | 111.0 | 11.5 | 23.9 | 62.9 | 12.9 | 291 |
| s1e7-n1-g225m | 8653 | 99.99 | 185,743 | 185,743 | 202,639 | 32,841 | 46.6 | 2.3 | 35.8 | 7.0 | 1.2 | 1117 |
| s1e7-n1-g450m | 17307 | 100.00 | 184,537 | 184,537 | 186,190 | 32,668 | 93.8 | 4.5 | 71.4 | 13.9 | 3.9 | 1121 |
| s1e7-n1-g1g | 38461 | 100.00 | 183,096 | 183,096 | 158,352 | 32,541 | 210.1 | 9.8 | 158.8 | 30.6 | 9.0 | 1155 |
| s1e7-n10-g225m | 3169 | 100.00 | 916,832 | 91,683 | 911,907 | 274,264 | 34.6 | 2.6 | 13.6 | 14.5 | 3.7 | 1117 |
| s1e7-n10-g450m | 6338 | 100.00 | 901,535 | 90,154 | 901,993 | 272,641 | 70.3 | 5.1 | 27.1 | 29.5 | 8.0 | 1139 |
| s1e7-n10-g1g | 14084 | 100.00 | 886,491 | 88,649 | 888,399 | 270,988 | 158.9 | 11.1 | 59.2 | 69.5 | 17.6 | 1197 |
| s1e7-n100-g225m | 431 | 99.80 | 1,566,372 | 15,664 | 1,574,484 | 1,084,256 | 27.5 | 2.7 | 2.5 | 17.4 | 4.8 | 1148 |
| s1e7-n100-g450m | 863 | 99.92 | 1,554,492 | 15,545 | 1,571,924 | 1,066,822 | 55.5 | 5.3 | 4.5 | 35.6 | 10.0 | 1148 |
| s1e7-n100-g1g | 1919 | 99.98 | 1,529,150 | 15,291 | 1,552,001 | 1,050,822 | 125.5 | 11.5 | 9.7 | 83.2 | 21.9 | 1198 |
| s1e7-nmix-g225m | 1271 | 100.00 | 1,333,228 | 42,727 | 1,342,909 | 622,325 | 29.7 | 2.7 | 5.9 | 17.0 | 4.5 | 1118 |
| s1e7-nmix-g450m | 2449 | 100.00 | 1,327,739 | 40,792 | 1,353,900 | 638,655 | 60.0 | 5.3 | 11.1 | 34.2 | 9.5 | 1143 |
| s1e7-nmix-g1g | 5408 | 100.00 | 1,323,323 | 40,365 | 1,338,786 | 633,746 | 134.0 | 11.6 | 23.8 | 79.6 | 20.8 | 1206 |

### What the table says

1. **N=1 is recovery-bound; the rest is not.** At N=1, parallel recovery is 80 % of a
   block (8 cores give ~5.8× over serial: 193k vs 33k payments/s). The ceiling is flat in
   budget and in state size (~185-205k tx/s): it is ecrecover throughput.
2. **At N≥10, execute + apply dominate and they scale with the state size.** From 1e5
   to 1e7 accounts, s*-n100-g450m falls 2.88M → 1.96M → 1.55M payments/s while framing
   and recovery stay constant. At 1e7, execute + apply are 82 % of an N=100 block. This
   is the `HashMap` path (SipHash, cache misses in a 1 GB map, two maps per block, no
   pre-sizing) — not recovery.
3. **16 threads (SMT) buy 0-8 %** and lose at N=1/1G (162k vs 192k at 1e6): recovery is
   compute-bound per core; the serial phases do not care about threads.
4. **The 1e5 rows run faster at 1G than at 450M for N=100** (3.65M vs 2.88M): with only
   1e5 distinct recipients, 192k payments/block collide and the per-block maps stop
   growing. Real states are large; read the 1e6/1e7 rows.
5. frame+decode is ~2.5 ms per 225M gas regardless of N (two keccaks over every byte:
   the commitment and each tx hash) — 12 % of an N=100 block at 1e6.

## Hot spots (single-threaded CPU attribution, 1e6 accounts, 450M gas)

`perf` is unavailable to an unprivileged user on this host (`perf_event_paranoid=4`, no
sudo; no other sampling profiler installed), so this is `lean-bench --micro`: each
function timed as one loop over block 1 on one thread (median of 3). Shares are of the
serial pipeline.

| function | N=1: ms/block | N=1: % | N=100: ms/block | N=100: % |
|---|--:|--:|--:|--:|
| ecrecover (`secp256k1` `recover_ecdsa`) | 499.30 | **95.5** | 24.73 | 32.2 |
| execute (`engine::execute`, whole) | 9.78 | 1.9 | 32.09 | **41.8** |
| … of which state lookups (`FlatState::get`) | 2.34 | 0.4 | 5.87 | 7.6 |
| … of which write-set sort | 0.21 | 0.0 | 0.68 | 0.9 |
| apply (`FlatState` insert) | 1.42 | 0.3 | 6.40 | 8.3 |
| signing hash: body re-encode + keccak (`LeanTx::signing_hash`) | 2.25 | 0.4 | 4.59 | 6.0 |
| tx hash keccak (`engine::decode`) | 4.33 | 0.8 | 4.52 | 5.9 |
| commitment keccak (`framing::commitment_of_wire`) | 3.14 | 0.6 | 4.34 | 5.6 |
| pubkey → address keccak | 2.21 | 0.4 | 0.04 | 0.1 |
| tx decode (`LeanTx::parse_after_type`) | 0.29 | 0.1 | 0.15 | 0.2 |
| framing walk (`engine::scan`) | 0.10 | 0.0 | 0.00 | 0.0 |

Top 5 overall: **(1)** ecrecover — all of N=1, a third of N=100; **(2)** execute's two
per-block `HashMap`s (`post`, `credits`) — ~27 ms of N=100's 32 ms is map building
beyond the state lookups (entry/insert with SipHash on 20-byte keys, growth from empty,
a second hash of every credit when merged into `post`); **(3)** apply — 8 % serial, and
up to 17 % of an N=100 block at 1e7 because it inserts into the 1 GB map; **(4)**
`LeanTx::signing_hash` re-encodes the body the wire already holds verbatim before
hashing it; **(5)** the two whole-block keccaks (commitment + per-tx hash), 11 % at N=100.

**BRAINSTORM** (not measured): a faster hasher and pre-sized maps for `post`/`credits`,
hashing the signing preimage straight from the wire slice, fusing decode+recover into one
rayon pass, and applying writes with the delta already sorted are the obvious first
edits inside `engine.rs`'s EVOLVE block. The `FlatState` hasher itself lives in
`types.rs`, outside it.

## End-to-end (two real nodes in-process, `lean-bench e2e`)

Proposer with a pre-filled pool (25k senders × 15 nonces = 375k txs, N=10, 225M gas,
1e5 accounts) and a follower, driven unpaced build → stage → newBlock{commitment} on
both, steps run one after another (a real CL overlaps the follower's stage with the
proposer), 16 threads, one run each. Both stopped when the pool drained (117 full blocks,
100 % fullness), and both ended at the same head and state digest.

| mode | blk/s | tx/s | payments/s | build ms | follower stage ms | proposer anchor ms | follower anchor ms | `BlockLog::append` alone ms |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| fsync | 15.3 | 48,387 | 483,867 | 12.8 | 24.3 | 19.4 | 8.8 | 1.84 |
| --no-fsync | 16.1 | 51,071 | 510,710 | 12.8 | 24.2 | 17.8 | 7.1 | 0.24 |

The same N=10/225M block costs the bare engine 25-30 ms (1e5-1e6 accounts), about what
the follower's stage costs (24 ms). The rest of the node path adds proposer anchor
(promote + pool update, ~18 ms) and build (~13 ms). fsync costs ~1.6 ms per ~1.1 MB
append, 5 % of blk/s here. Under 1/15 s per height, the node's own work is far below
the 500 ms consensus pacer: the lane is consensus-bound, as the fleet numbers said.
