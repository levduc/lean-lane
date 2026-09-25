# lean-core hand optimization — log

Branch `engine-opt` off `engine` @ `75b1efe` (tag `gate-engine-safetynet-2026-09-24`).
Host: i7-11700F, 8 physical cores / 16 threads, idle.

**Protocol.** Every attempt: `scripts/engine-gate.sh` must print `"correct":true` and
`cargo test --workspace` must pass. Speed: the baseline binary is kept; each candidate
is compared by K=5 invocations per binary, **alternating** base/candidate, each
invocation `lean-bench --scenario ... --json --no-serial --runs 5 --warmup 1 --blocks 3`
(so each point is itself a median of 5 runs × 3 blocks). Two modes: **p8** =
`taskset -c 0-7 --threads 8` (headline), **t16** = `--threads 16`, unpinned. Reported:
median of the 5 invocation medians (payments/s), spread = (max−min)/median over the 5,
delta = candidate/base − 1. Accept iff the median improves by more than the observed
spread. N=1 is bimodal on this host (invocations land at ~156k or ~191k, run to run,
same binary) — its spread is ~20 %, so N=1 deltas under that are not claimed.

Scenarios: the three gate scenarios (1e6 accounts, 450M gas, N=1 / N=100 / mix) plus
N=100 at 1e7 accounts.

## Attempts

### A1 — `execute`: sender-partitioned sort/merge instead of two per-block HashMaps — ACCEPTED

Idea: a sender's in-block balance depends only on its own earlier txs (same-block
credits are unspendable), so group items by sender (sort `(sender, block index)`),
run each group in block order from its block-start value — groups are independent,
run on rayon. Credits of applied txs → one `Vec`, sorted by recipient, summed per run;
senders ∪ recipients ∪ beneficiary merged in address order, so the write set comes out
sorted with no map at all. Recipient-only block-start reads (the cache misses into the
state map) run in parallel. Addresses compared as big-endian `(u64,u64,u32)` keys
(same order as the bytes). Same algorithm on the calling thread when
`parallel_recovery` is off. New test `lean-core/tests/execute_reference.rs`: frozen
copy of the original STF vs `execute` on large random blocks (up to 20k items, heavy
collisions, nonce gaps/dups, insufficient funds, beneficiary as sender/recipient),
both executors; mutation-checked (dropping the beneficiary's fee fails it).

| mode | scenario | base median | spread | cand median | spread | delta |
|---|---|--:|--:|--:|--:|--:|
| p8 | s1e6-n1-g450m | 191,073 | 18.8 % | 212,732 | 21.1 % | +11.3 % (noise) |
| p8 | s1e6-n100-g450m | 1,790,921 | 1.1 % | 4,213,739 | 1.0 % | **+135.3 %** |
| p8 | s1e6-nmix-g450m | 1,547,569 | 0.5 % | 2,833,149 | 4.8 % | **+83.1 %** |
| p8 | s1e7-n100-g450m | 1,470,500 | 1.0 % | 3,469,719 | 1.3 % | **+136.0 %** |
| t16 | s1e6-n1-g450m | 162,379 | 26.1 % | 220,256 | 24.5 % | +35.6 % |
| t16 | s1e6-n100-g450m | 1,831,856 | 0.7 % | 4,352,602 | 2.0 % | **+137.6 %** |
| t16 | s1e6-nmix-g450m | 1,584,524 | 1.5 % | 2,959,279 | 1.0 % | **+86.8 %** |
| t16 | s1e7-n100-g450m | 1,490,354 | 0.7 % | 3,602,021 | 0.5 % | **+141.7 %** |

Phases (p8, ms/block) N=100/1e6: execute 33.05 → 4.49; N=1: 11.53 → 2.41. New N=100
block: frame+dec 5.3 · recover 4.5 · execute 4.5 · apply 6.4 (20.3 ms).

### A2 — fused decode+recover, signing hash straight from the entry's body bytes — REJECTED (neutral)

Idea: one rayon pass per entry (parse → `keccak(domain ‖ body)` over the wire bytes,
no re-encode, no preimage buffer, bypassing alloy's small-input keccak cache → ecrecover
→ tx hash), new `lean_native::{signing_hash_of_body, recover_prehashed}`. Gate correct.
Base = A1.

| mode | scenario | base median | spread | cand median | spread | delta |
|---|---|--:|--:|--:|--:|--:|
| p8 (4-scenario run) | s1e6-n100-g450m | 4,218,702 | 2.0 % | 3,984,624 | 2.8 % | −5.5 % |
| p8 (4-scenario run) | s1e6-nmix-g450m | 2,824,154 | 5.7 % | 2,753,092 | 6.3 % | −2.5 % |
| p8 (4-scenario run) | s1e7-n100-g450m | 3,434,541 | 3.0 % | 3,439,441 | 0.5 % | +0.1 % |
| t16 (4-scenario run) | s1e6-n100-g450m | 4,379,681 | 1.6 % | 4,125,172 | 1.5 % | −5.8 % |
| p8 (re-run, 2 scenarios) | s1e6-n100-g450m | 4,134,335 | 1.7 % | 4,156,444 | 0.9 % | +0.5 % |
| p8 (re-run, 2 scenarios) | s1e6-nmix-g450m | 2,979,389 | 1.3 % | 2,984,898 | 0.6 % | +0.2 % |

The phase timings of single invocations were identical (process_block 14.4 ms both);
the −5.5 % of the first run did not reproduce — a process-layout effect of the
4-scenario invocation, which is a lesson for the protocol: the within-condition spread
understates between-binary layout noise of a few %. A one-shot uncached keccak over a
Vec preimage instead of the streaming sponge: −0.5 % (noise). The expected gain was
~0.5 ms of 20.7 ms (the re-encode is 6 % of the *single-threaded* N=100 pipeline, /8
on the pool) — below the noise floor. Reverted.

### A3 — overlap the commitment keccak with decode + recovery (`rayon::join`) — ACCEPTED

Idea: the commitment is one serial ~3.5 ms keccak over the whole 1.2 MB block, run
before any parallel work. `process_block` now runs it on one pool thread while the
others decode and recover. Serial mode unchanged. Base = A1.

| mode | scenario | base median | spread | cand median | spread | delta |
|---|---|--:|--:|--:|--:|--:|
| p8 | s1e6-n1-g450m | 166,811 | 28.1 % | 169,869 | 27.9 % | +1.8 % (noise) |
| p8 | s1e6-n100-g450m | 4,182,285 | 1.8 % | 4,804,667 | 1.2 % | **+14.9 %** |
| p8 | s1e6-nmix-g450m | 2,829,926 | 5.9 % | 3,169,814 | 6.8 % | **+12.0 %** |
| p8 | s1e7-n100-g450m | 3,436,363 | 1.1 % | 4,038,577 | 2.8 % | **+17.5 %** |
| t16 | s1e6-n1-g450m | 174,140 | 0.7 % | 178,473 | 1.6 % | +2.5 % |
| t16 | s1e6-n100-g450m | 4,377,791 | 0.9 % | 4,594,418 | 1.2 % | **+4.9 %** |
| t16 | s1e6-nmix-g450m | 2,952,937 | 1.3 % | 3,328,469 | 0.8 % | **+12.7 %** |
| t16 | s1e7-n100-g450m | 3,596,963 | 1.3 % | 3,949,884 | 1.3 % | **+9.8 %** |

### A4 — `FlatState` map on foldhash (`alloy_primitives::map::HashMap`) — REJECTED (slower)

Idea: SipHash → foldhash (seeded, already in the dep graph). Needs one node line
(`state.rs` snapshot load: `with_capacity_and_hasher`). Gate correct. Base = A3.

| mode | scenario | base median | spread | cand median | spread | delta |
|---|---|--:|--:|--:|--:|--:|
| p8 | s1e6-n1-g450m | 170,938 | 28.2 % | 167,383 | 27.9 % | −2.1 % (noise) |
| p8 | s1e6-n100-g450m | 4,850,734 | 1.7 % | 3,973,716 | 1.6 % | **−18.1 %** |
| p8 | s1e6-nmix-g450m | 3,158,159 | 6.1 % | 2,792,149 | 4.9 % | **−11.6 %** |
| p8 | s1e7-n100-g450m | 4,041,607 | 1.1 % | 3,108,454 | 1.3 % | **−23.1 %** |
| t16 | s1e6-n100-g450m | 4,624,579 | 1.6 % | 3,823,029 | 1.2 % | **−17.3 %** |
| t16 | s1e7-n100-g450m | 3,935,030 | 3.0 % | 3,049,969 | 1.4 % | **−22.5 %** |

Why (standalone map microbench, 87k sorted-address updates, one core): lookups get
2x faster (1e7: 8.3 → 3.9 ms) but `HashMap::insert` of an EXISTING key gets ~1.7x
SLOWER (8.9 → 14.8 ms; `get_mut` + assign is 5.9 ms) — with both foldhash `fast` and
`quality`, so not hash distribution; an insert-path codegen effect of the std map with
foldhash's outlined >16-byte `write`. `apply` went 10.2 → 16.3 ms at 1e7. Reverted.

### A4b — `FlatState` map on a small keyed address hasher (`AddressHasher`) — ACCEPTED

Idea: a 30-line hasher specialised for 20-byte keys: two folded 64×64→128 multiplies of
key-whitened input + one to finish (foldhash's construction, inlined), keys drawn per
map instance from std's `RandomState`. NOT an unkeyed FxHash: recipient addresses are
attacker-chosen (zero-amount outputs create accounts), so the persistent map must be
keyed or a proposer could grind colliding addresses. Microbench at 1e7 (insert / get,
ms per 87k): SipHash 8.9 / 8.3, foldhash 14.8 / 3.9, unkeyed FxHash (`FbBuildHasher`)
4.4 / 2.9, **this 4.8 / 3.3**. Public type change: `FlatState::accounts` is now
`lean_core::AccountMap` = `HashMap<Address, Acct, AddressHasherBuilder>` (all method
calls unchanged; one node line: snapshot load uses `with_capacity_and_hasher`).
Base = A3.

| mode | scenario | base median | spread | cand median | spread | delta |
|---|---|--:|--:|--:|--:|--:|
| p8 | s1e6-n1-g450m | 170,069 | 29.3 % | 172,318 | 28.4 % | +1.3 % (noise) |
| p8 | s1e6-n100-g450m | 4,809,091 | 2.3 % | 5,884,576 | 2.4 % | **+22.4 %** |
| p8 | s1e6-nmix-g450m | 3,178,949 | 5.8 % | 3,558,130 | 8.5 % | **+11.9 %** |
| p8 | s1e7-n100-g450m | 3,999,932 | 2.3 % | 5,247,523 | 1.1 % | **+31.2 %** |
| t16 | s1e6-n1-g450m | 179,472 | 1.0 % | 180,005 | 1.6 % | +0.3 % (noise) |
| t16 | s1e6-n100-g450m | 4,607,324 | 1.0 % | 5,516,430 | 1.7 % | **+19.7 %** |
| t16 | s1e6-nmix-g450m | 3,361,712 | 2.5 % | 3,751,065 | 0.9 % | **+11.6 %** |
| t16 | s1e7-n100-g450m | 3,948,914 | 1.1 % | 5,069,821 | 0.6 % | **+28.4 %** |

### A5 — `apply`: overwrite in place (`get_mut`), insert only when absent — ACCEPTED

Idea: nearly every write hits an existing account; `get_mut` + assign skips the insert
path (reserve check + insert-slot search). Same map afterwards. Base = A4b.

| mode | scenario | base median | spread | cand median | spread | delta |
|---|---|--:|--:|--:|--:|--:|
| p8 | s1e6-n1-g450m | 173,064 | 25.6 % | 173,713 | 28.3 % | +0.4 % (noise) |
| p8 | s1e6-n100-g450m | 5,982,057 | 1.1 % | 6,307,795 | 3.1 % | **+5.4 %** |
| p8 | s1e6-nmix-g450m | 3,583,968 | 15.7 % | 3,687,812 | 1.2 % | +2.9 % (noise) |
| p8 | s1e7-n100-g450m | 5,271,921 | 1.4 % | 5,649,006 | 1.2 % | **+7.2 %** |
| t16 | s1e6-n1-g450m | 181,676 | 32.0 % | 182,715 | 33.8 % | +0.6 % (noise) |
| t16 | s1e6-n100-g450m | 5,548,001 | 3.8 % | 5,883,347 | 4.3 % | **+6.0 %** |
| t16 | s1e6-nmix-g450m | 3,762,495 | 9.9 % | 3,903,658 | 12.6 % | +3.8 % (noise) |
| t16 | s1e7-n100-g450m | 5,036,549 | 1.0 % | 5,369,959 | 1.0 % | **+6.6 %** |

### A7 — sharded `AccountMap` (64 maps by the address's top 6 bits) + shard-parallel `apply` — ACCEPTED

Idea: `apply` was the last serial phase (2.9 ms N=100/1e6, 4.7 ms at 1e7). Shard the
state map by the leading address bits: shard order = address order, so an `execute`
write set (sorted by address) splits into one contiguous run per shard, and disjoint
shards are written on rayon in parallel. Serial fallback for small (< 2048) or
not-shard-sorted write sets; within a shard writes keep their order (a hand-built
delta with a repeated address still ends at its last value). `AccountMap` is now a
struct mirroring the `HashMap` methods callers use (`insert/get/get_mut/remove/
contains_key/len/is_empty/reserve/iter/&-IntoIterator/with_capacity`) — node, bench,
diff and tests compile unchanged except the node's snapshot loader
(`AccountMap::with_capacity`). Bonus: a growing 1e7 state rehashes 1/64 at a time.
New test `lean-core/tests/apply.rs` (apply vs insert-in-order on sorted, unsorted and
duplicate-carrying deltas; `AccountMap` vs `HashMap`). State build at 1e7 unchanged
(0.80 s, +1040 MB). Base = A5.

| mode | scenario | base median | spread | cand median | spread | delta |
|---|---|--:|--:|--:|--:|--:|
| p8 | s1e6-n1-g450m | 173,724 | 28.4 % | 174,802 | 27.9 % | +0.6 % (noise) |
| p8 | s1e6-n100-g450m | 6,300,482 | 3.6 % | 7,334,876 | 6.0 % | **+16.4 %** |
| p8 | s1e6-nmix-g450m | 3,711,426 | 9.1 % | 4,028,555 | 2.4 % | +8.5 % (≈ spread) |
| p8 | s1e7-n100-g450m | 5,600,630 | 1.7 % | 7,023,215 | 1.5 % | **+25.4 %** |
| t16 | s1e6-n1-g450m | 182,883 | 0.7 % | 183,870 | 1.4 % | +0.5 % (noise) |
| t16 | s1e6-n100-g450m | 5,861,466 | 2.4 % | 6,659,364 | 1.0 % | **+13.6 %** |
| t16 | s1e6-nmix-g450m | 3,902,552 | 0.4 % | 4,238,047 | 1.3 % | **+8.6 %** |
| t16 | s1e7-n100-g450m | 5,383,748 | 1.4 % | 6,765,644 | 0.6 % | **+25.7 %** |

apply at 1e7 (p8): 4.67 → 1.73 ms.

### A6 — `execute`: per-shard parallel credit sort + merge + state reads — REJECTED (inconclusive)

Idea: bucket the applied credits by account-map shard (rayon `fold`), then per shard in
parallel: sort, sum runs, merge with that shard's senders and the beneficiary, read the
recipient-only block-start values; concatenate the 64 per-shard write lists. Gate
correct. Base = A7.

| mode | scenario | base median | spread | cand median | spread | delta |
|---|---|--:|--:|--:|--:|--:|
| p8 | s1e6-n1-g450m | 174,939 | 28.2 % | 178,129 | 28.0 % | +1.8 % (noise) |
| p8 | s1e6-n100-g450m | 7,352,653 | 1.7 % | 7,628,105 | 3.6 % | +3.7 % (≈ spread) |
| p8 | s1e6-nmix-g450m | 4,049,556 | 10.1 % | 3,975,471 | 1.6 % | −1.8 % |
| p8 | s1e7-n100-g450m | 6,983,808 | 2.9 % | 7,090,833 | 4.4 % | +1.5 % |
| t16 | s1e6-n1-g450m | 183,756 | 0.8 % | 185,730 | 0.6 % | +1.1 % |
| t16 | s1e6-n100-g450m | 6,602,710 | 2.8 % | 6,849,149 | 2.7 % | +3.7 % |
| t16 | s1e6-nmix-g450m | 4,258,396 | 1.0 % | 4,136,058 | 1.2 % | **−2.9 %** |
| t16 | s1e7-n100-g450m | 6,765,095 | 1.2 % | 6,548,650 | 3.4 % | −3.2 % |

Why: execute's internal time fell (2.2 ms) but the extra 5.5 MB per-shard buffers +
concat and 64×(fold pieces) allocations cost as much in fresh-page faults and frees. A
side measurement found the allocator matters at this scale: glibc's trim/mmap
thresholds re-fault ~31 MB of fresh pages per block iteration in this bench; with
`GLIBC_TUNABLES=glibc.malloc.trim_threshold=1G:glibc.malloc.mmap_threshold=1G` minor
faults drop 161k → 60k and N=100 gains ~+9 % with no code change (binary-level, not
applied — see concerns). Reverted.

### A8 — `execute`: one merge pass, flagged parallel state reads (fewer, smaller buffers) — ACCEPTED

Idea: A1's tail built a `recv` list of summed recipients (4 MB at N=100), then merged it
with the senders, then built a `fill` index list and a `Vec<&mut>` (0.7 MB each, serial)
for the parallel state reads. Now a single merge walks senders ∪ credit runs ∪
beneficiary straight into `writes`, flagging recipient-only rows in a `Vec<bool>`; the
state reads are one `par_iter_mut().zip(flags)` pass. Gate correct. Base = A7.

| mode | scenario | base median | spread | cand median | spread | delta |
|---|---|--:|--:|--:|--:|--:|
| p8 | s1e6-n1-g450m | 174,677 | 27.6 % | 174,849 | 28.5 % | +0.1 % (noise) |
| p8 | s1e6-n100-g450m | 7,350,943 | 19.6 %¹ | 8,206,338 | 2.3 % | **+11.6 %** |
| p8 | s1e6-nmix-g450m | 4,041,172 | 8.2 % | 4,128,652 | 2.0 % | +2.2 % (noise) |
| p8 | s1e7-n100-g450m | 6,999,920 | 3.4 % | 7,588,016 | 1.4 % | **+8.4 %** |
| t16 | s1e6-n1-g450m | 185,660 | 1.3 % | 184,402 | 0.6 % | −0.7 % (noise) |
| t16 | s1e6-n100-g450m | 6,692,995 | 2.4 % | 7,658,893 | 3.0 % | **+14.4 %** |
| t16 | s1e6-nmix-g450m | 4,244,451 | 0.9 % | 4,483,272 | 1.3 % | **+5.6 %** |
| t16 | s1e7-n100-g450m | 6,734,601 | 1.5 % | 7,236,044 | 2.3 % | **+7.4 %** |

¹ one low base invocation (6.02M); the other four were 7.31–7.46M.

### A9 — A2 retried on top of A8: fused decode+recover, signing hash from the body bytes — ACCEPTED (small)

Same code as A2 (`lean_native::{signing_hash_of_body, recover_prehashed}`;
`process_block` and `recover_txs` run one fused rayon pass). With the block ~2x cheaper
than when A2 was measured, re-measured. Base = A8.

| mode | scenario | base median | spread | cand median | spread | delta |
|---|---|--:|--:|--:|--:|--:|
| p8 | s1e6-n1-g450m | 174,716 | 26.8 % | 178,398 | 29.7 % | +2.1 % |
| p8 | s1e6-n100-g450m | 8,196,153 | 3.0 % | 8,213,514 | 1.9 % | +0.2 % (noise) |
| p8 | s1e6-nmix-g450m | 4,132,814 | 24.7 % | 4,167,752 | 2.7 % | +0.8 % (noise) |
| p8 | s1e7-n100-g450m | 7,513,317 | 1.0 % | 7,545,783 | 1.8 % | +0.4 % (noise) |
| t16 | s1e6-n1-g450m | 184,299 | 0.5 % | 188,457 | 1.1 % | **+2.3 %** |
| t16 | s1e6-n100-g450m | 7,649,778 | 2.0 % | 7,607,587 | 3.2 % | −0.6 % (noise) |
| t16 | s1e6-nmix-g450m | 4,478,552 | 0.9 % | 4,521,223 | 0.5 % | +1.0 % |
| t16 | s1e7-n100-g450m | 7,243,574 | 1.4 % | 7,275,796 | 2.0 % | +0.4 % (noise) |

Focused re-run, K=7 (N=1 this time not bimodal under p8):

| mode | scenario | base median | spread | cand median | spread | delta |
|---|---|--:|--:|--:|--:|--:|
| p8 | s1e6-n1-g450m | 175,701 | 0.6 % | 179,179 | 0.2 % | **+2.0 %** (every cand > every base) |
| p8 | s1e6-nmix-g450m | 4,209,091 | 4.5 % | 4,222,460 | 2.7 % | +0.3 % (noise) |
| t16 | s1e6-n1-g450m | 184,368 | 33.1 %² | 188,706 | 33.5 %² | +2.4 % |
| t16 | s1e6-nmix-g450m | 4,535,280 | 15.2 %² | 4,557,175 | 11.6 %² | +0.5 % (noise) |

² the first alternation pair ran ~30 % fast for both binaries (a host turbo state); the
other six pairs are within 2 %. Accepted for the consistent +2 % at N=1 (one fewer
rayon pass and barrier, no intermediate `Vec<Decoded>`, no re-encode); neutral at N≥10.

### A10 — `execute`: counting sort of the credits on the address's first byte, then per-bucket sorts — ACCEPTED (marginal)

Idea: instrumenting A8's execute at N=100 (p8): collect 0.2 ms, `par_sort_unstable` of
86k 32-byte credits 0.85 ms, merge 0.5 ms, parallel state reads 1.2 ms. Replace the
sort by one counting pass + one placement pass into 256 first-byte buckets (bucket
order = address order), then the buckets sorted independently on rayon. Gate correct.
Base = A9.

| mode | scenario | base median | spread | cand median | spread | delta |
|---|---|--:|--:|--:|--:|--:|
| p8 | s1e6-n1-g450m | 178,743 | 28.6 % | 179,244 | 28.9 % | +0.3 % (noise) |
| p8 | s1e6-n100-g450m | 8,273,468 | 6.1 % | 8,576,918 | 4.1 % | +3.7 % (≤ spread) |
| p8 | s1e6-nmix-g450m | 4,191,162 | 9.7 % | 4,215,076 | 5.3 % | +0.6 % (noise) |
| p8 | s1e7-n100-g450m | 7,567,116 | 2.4 % | 7,724,809 | 2.8 % | +2.1 % (≤ spread) |
| t16 | s1e6-n1-g450m | 188,120 | 0.3 % | 188,273 | 0.6 % | +0.1 % (noise) |
| t16 | s1e6-n100-g450m | 7,660,563 | 2.7 % | 7,897,532 | 1.7 % | **+3.1 %** |
| t16 | s1e6-nmix-g450m | 4,522,780 | 1.7 % | 4,582,837 | 0.9 % | +1.3 % (noise) |
| t16 | s1e7-n100-g450m | 7,275,505 | 0.8 % | 7,540,729 | 1.8 % | **+3.6 %** |

A preliminary K=3 p8 run was tighter: N=100 +3.5 % (spreads 0.6/0.6 %), 1e7 +2.7 %
(1.1/0.4 %), mix +2.0 % (0.2/0.3 %) — every candidate above every baseline. Accepted
on t16 N=100 and 1e7 clearing their spreads and the K=3 run; the gain is ~3 %, at the
edge of what this protocol resolves.
