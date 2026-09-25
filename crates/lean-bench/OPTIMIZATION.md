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

### A11 — `execute`: range-parallel merge into a pre-sized, parallel-first-touched write set — ACCEPTED

Idea: after A10 the tail was a serial merge (0.5 ms) pushing into a fresh 5.5 MB `writes`
(≈1,300 first-touch page faults on the critical path) plus a separate flagged read pass.
The 256 first-byte ranges of the key space are independent: count each range's rows in
parallel (the same `merge_range` walk with a counting sink), allocate `writes` at the
exact total and first-touch it with `par_extend(repeat_n(..))` (the page faults land on
every thread), split it into per-range slices and fill all ranges in parallel, reading
the recipient-only block-start values inside the fill. A6 failed on per-shard buffers +
concat; this writes each row once, in place. Gate correct. Base = A10.

| mode | scenario | base median | spread | cand median | spread | delta |
|---|---|--:|--:|--:|--:|--:|
| p8 | s1e6-n1-g450m | 178,854 | 28.9 % | 179,202 | 19.6 % | +0.2 % (noise) |
| p8 | s1e6-n100-g450m | 8,510,700 | 4.9 % | 8,811,700 | 5.7 % | +3.5 % (≤ spread) |
| p8 | s1e6-nmix-g450m | 4,246,374 | 9.8 % | 4,436,476 | 1.3 % | +4.5 % (≤ spread) |
| p8 | s1e7-n100-g450m | 7,764,232 | 1.7 % | 8,270,465 | 5.8 % | **+6.5 %** |
| t16 | s1e6-n1-g450m | 188,611 | 0.2 % | 189,224 | 0.9 % | +0.3 % (noise) |
| t16 | s1e6-n100-g450m | 7,889,119 | 3.3 % | 8,344,368 | 2.6 % | **+5.8 %** |
| t16 | s1e6-nmix-g450m | 4,587,963 | 0.8 % | 4,878,323 | 2.1 % | **+6.3 %** |
| t16 | s1e7-n100-g450m | 7,476,074 | 1.8 % | 7,973,003 | 1.7 % | **+6.6 %** |

Preliminary K=3 p8: N=100 +5.4 % (0.8/0.9 %), 1e7 +4.4 % (1.2/0.7 %), mix +7.1 %
(0.6/1.1 %), every candidate above every baseline.

### A12 — in-place compaction of the recovered-items `Vec<Option<_>>` (`filter_map` instead of `flatten`) — REJECTED (neutral)

Idea: std collects `into_iter().filter_map(|x| x)` in place but `flatten()` not
(checked with a pointer-equality probe), so the per-block copy of the 17k-entry
(2.6 MB at N=1) item list and its fresh pages go away. Expected < 1 % (a serial
~0.5 ms of a 75 ms N=1 block). Base = A11, K=7.

| mode | scenario | base median | spread | cand median | spread | delta |
|---|---|--:|--:|--:|--:|--:|
| p8 | s1e6-n1-g450m | 179,332 | 29.8 % | 201,954 | 25.3 % | +12.6 % (bimodal noise) |
| p8 | s1e6-nmix-g450m | 4,542,055 | 11.1 % | 4,861,571 | 10.7 % | +7.0 % (noise) |
| t16 | s1e6-n1-g450m | 189,445 | 0.5 % | 189,572 | 0.2 % | +0.1 % |
| t16 | s1e6-nmix-g450m | 4,891,168 | 2.3 % | 4,895,471 | 0.8 % | +0.1 % |

The p8 medians moved only because more candidate invocations landed in the host's fast
N=1 mode; the stable t16 arm shows nothing. Reverted.


## Final results (branch head)

Correctness: `scripts/engine-gate.sh` twice — `"correct":true` both (golden 83 assertions,
diff 4000 chains × 20 blocks, fuzz 1e6, restart 32 chains); gate geomean
1,940,010 / 1,939,521 payments/s (baseline gate: 799,506).
`cargo test --workspace` passes. `scripts/lean-smoke.sh`: PASS (40 heights, 9,856 txs,
98,560 payments, 3/3 nodes at one commitment, V4 replay identical); `--mixed`: PASS
(4,147 txs, 122,146 payments, avg N=29.5).

### Before → after: the gate scenarios + N=100 at 1e7 (K=5 alternating invocations, median of medians)

| mode | scenario | before pay/s | spread | after pay/s | spread | speedup |
|---|---|--:|--:|--:|--:|--:|
| p8 | s1e6-n1-g450m | 191,904 | 0.4 % | 230,070 | 0.5 % | 1.20x |
| p8 | s1e6-n100-g450m | 1,793,186 | 1.5 % | 9,045,787 | 4.4 % | **5.04x** |
| p8 | s1e6-nmix-g450m | 1,552,652 | 0.3 % | 4,915,746 | 1.7 % | **3.17x** |
| p8 | s1e7-n100-g450m | 1,468,397 | 1.4 % | 8,282,018 | 0.8 % | **5.64x** |
| t16 | s1e6-n1-g450m | 204,073 | 20.4 %¹ | 220,584 | 19.3 %¹ | 1.08x |
| t16 | s1e6-n100-g450m | 1,827,056 | 1.1 % | 8,105,186 | 3.0 % | **4.44x** |
| t16 | s1e6-nmix-g450m | 1,585,005 | 2.8 % | 4,866,342 | 0.8 % | **3.07x** |
| t16 | s1e7-n100-g450m | 1,491,399 | 0.7 % | 7,992,550 | 0.8 % | **5.36x** |

¹ bimodal host (two invocations per binary in the slow mode, ~164k/187k). All blocks
99.92–100 % full. At 16 threads N=100 is slightly *slower* than at 8 pinned: the
remaining memory-bound pieces do not gain from SMT and 16 unpinned threads contend.

### e2e (`lean-bench e2e --seconds 10`: two real nodes in-process, N=10, 225M, 1e5 accounts, 16 threads; both nodes end at the same head and state digest — asserted)

| log | run | blocks | full % | blk/s | tx/s | payments/s | build ms | follower stage ms | proposer anchor ms | follower anchor ms | append ms |
|---|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| fsync | before #1 | 117 | 100 | 15.32 | 48,552 | 485,523 | 12.8 | 24.2 | 19.3 | 8.9 | 1.53 |
| fsync | before #2 | 117 | 100 | 15.27 | 48,397 | 483,968 | 12.8 | 24.2 | 19.5 | 8.9 | 1.83 |
| fsync | after #1 | 117 | 100 | 19.59 | 62,086 | 620,856 | 6.9 | 18.2 | 18.4 | 7.5 | 1.72 |
| fsync | after #2 | 117 | 100 | 19.27 | 61,076 | 610,759 | 7.0 | 18.4 | 18.7 | 7.7 | 1.75 |
| no-fsync | before #1 | 117 | 100 | 16.03 | 50,802 | 508,023 | 12.8 | 24.4 | 17.8 | 7.2 | 0.24 |
| no-fsync | before #2 | 117 | 100 | 16.07 | 50,926 | 509,258 | 12.8 | 24.3 | 17.8 | 7.2 | 0.23 |
| no-fsync | after #1 | 117 | 100 | 20.79 | 65,893 | 658,930 | 6.8 | 18.0 | 17.3 | 5.9 | 0.23 |
| no-fsync | after #2 | 117 | 100 | 20.68 | 65,526 | 655,264 | 7.0 | 18.1 | 17.1 | 6.1 | 0.23 |

fsync ~485k → ~616k payments/s (+27 %), no-fsync ~509k → ~657k (+29 %): build 12.8 →
6.9 ms and follower stage 24.2 → 18.2 ms (engine); the proposer/follower anchor
(promote + pool update, node code outside lean-core) is unchanged at ~18/7 ms and is now
the largest node-side step.

### Full suite (`taskset -c 0-7 lean-bench --all --json --threads 8`, one invocation per binary, same session)

| scenario | tx/blk | full % | before pay/s | after pay/s | speedup | before blk ms | after blk ms | after fr+dec | recover | execute | apply | after serial-rec pay/s | peak RSS MB |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| s1e5-n1-g225m | 8653 | 99.99 | 202,279 | 224,165 | 1.11x | 42.8 | 38.6 | 2.4 | 36.3 | 1.1 | 0.2 | 33,476 | 52 |
| s1e5-n1-g450m | 17307 | 100.00 | 201,766 | 227,852 | 1.13x | 85.8 | 76.0 | 4.8 | 71.9 | 1.9 | 0.3 | 33,538 | 73 |
| s1e5-n1-g1g | 38461 | 100.00 | 205,458 | 229,196 | 1.12x | 187.2 | 167.8 | 10.5 | 159.4 | 3.7 | 0.6 | 33,483 | 123 |
| s1e5-n10-g225m | 3169 | 100.00 | 1,274,787 | 2,038,633 | 1.60x | 24.9 | 15.5 | 2.6 | 13.7 | 0.9 | 0.2 | 322,776 | 74 |
| s1e5-n10-g450m | 6338 | 100.00 | 1,337,649 | 2,069,681 | 1.55x | 47.4 | 30.6 | 5.1 | 26.9 | 1.9 | 0.4 | 321,281 | 80 |
| s1e5-n10-g1g | 14084 | 100.00 | 1,414,730 | 2,106,288 | 1.49x | 99.6 | 66.9 | 11.4 | 59.3 | 3.6 | 0.6 | 322,086 | 111 |
| s1e5-n100-g225m | 431 | 99.80 | 2,750,515 | 10,292,958 | 3.74x | 15.7 | 4.2 | 2.7 | 2.4 | 0.9 | 0.2 | 2,299,087 | 79 |
| s1e5-n100-g450m | 863 | 99.92 | 2,900,834 | 10,793,059 | 3.72x | 29.8 | 8.0 | 5.3 | 4.6 | 1.6 | 0.5 | 2,327,818 | 93 |
| s1e5-n100-g1g | 1919 | 99.98 | 3,739,321 | 11,449,440 | 3.06x | 51.3 | 16.8 | 11.6 | 9.6 | 3.6 | 0.6 | 2,343,213 | 128 |
| s1e5-nmix-g225m | 1271 | 100.00 | 2,082,417 | 5,084,813 | 2.44x | 19.0 | 7.8 | 2.7 | 5.9 | 0.9 | 0.2 | 918,410 | 83 |
| s1e5-nmix-g450m | 2449 | 100.00 | 2,385,318 | 5,421,726 | 2.27x | 33.4 | 14.7 | 5.4 | 11.0 | 1.7 | 0.5 | 956,540 | 94 |
| s1e5-nmix-g1g | 5408 | 100.00 | 2,710,299 | 5,619,741 | 2.07x | 65.4 | 31.5 | 11.6 | 23.8 | 3.7 | 0.6 | 965,341 | 128 |
| s1e6-n1-g225m | 8653 | 99.99 | 195,416 | 228,355 | 1.17x | 44.3 | 37.9 | 2.3 | 35.9 | 1.3 | 0.2 | 33,553 | 195 |
| s1e6-n1-g450m | 17307 | 100.00 | 193,122 | 230,160 | 1.19x | 89.6 | 75.2 | 4.8 | 71.7 | 2.2 | 0.5 | 33,468 | 218 |
| s1e6-n1-g1g | 38461 | 100.00 | 192,281 | 227,229 | 1.18x | 200.0 | 169.3 | 10.0 | 158.6 | 4.3 | 1.0 | 33,522 | 246 |
| s1e6-n10-g225m | 3169 | 100.00 | 1,056,723 | 1,981,090 | 1.87x | 30.0 | 16.0 | 2.6 | 13.5 | 1.2 | 0.3 | 318,338 | 191 |
| s1e6-n10-g450m | 6338 | 100.00 | 1,044,721 | 1,995,165 | 1.91x | 60.7 | 31.8 | 5.1 | 26.8 | 2.5 | 0.9 | 319,519 | 211 |
| s1e6-n10-g1g | 14084 | 100.00 | 1,027,239 | 2,021,703 | 1.97x | 137.1 | 69.7 | 11.2 | 59.3 | 5.1 | 1.8 | 320,637 | 270 |
| s1e6-n100-g225m | 431 | 99.80 | 2,004,158 | 8,929,133 | 4.46x | 21.5 | 4.8 | 2.7 | 2.5 | 1.3 | 0.5 | 2,209,606 | 205 |
| s1e6-n100-g450m | 863 | 99.92 | 1,939,614 | 9,217,024 | 4.75x | 44.5 | 9.4 | 5.3 | 4.5 | 2.4 | 1.1 | 2,245,804 | 231 |
| s1e6-n100-g1g | 1919 | 99.98 | 1,999,186 | 9,516,253 | 4.76x | 96.0 | 20.2 | 11.6 | 9.6 | 5.5 | 2.1 | 2,251,444 | 271 |
| s1e6-nmix-g225m | 1271 | 100.00 | 1,629,418 | 4,695,758 | 2.88x | 24.3 | 8.4 | 2.7 | 5.9 | 1.3 | 0.4 | 902,515 | 205 |
| s1e6-nmix-g450m | 2449 | 100.00 | 1,623,440 | 4,956,140 | 3.05x | 49.1 | 16.1 | 5.3 | 11.0 | 2.4 | 1.0 | 940,993 | 230 |
| s1e6-nmix-g1g | 5408 | 100.00 | 1,635,534 | 5,145,161 | 3.15x | 108.4 | 34.5 | 11.6 | 23.9 | 5.4 | 2.0 | 943,790 | 279 |
| s1e7-n1-g225m | 8653 | 99.99 | 186,902 | 226,811 | 1.21x | 46.3 | 38.2 | 2.3 | 35.9 | 1.5 | 0.3 | 33,494 | 1138 |
| s1e7-n1-g450m | 17307 | 100.00 | 184,186 | 228,457 | 1.24x | 94.0 | 75.8 | 4.5 | 71.6 | 2.4 | 0.8 | 33,495 | 1141 |
| s1e7-n1-g1g | 38461 | 100.00 | 183,253 | 228,316 | 1.25x | 209.9 | 168.5 | 10.3 | 159.4 | 4.9 | 1.6 | 33,472 | 1188 |
| s1e7-n10-g225m | 3169 | 100.00 | 910,180 | 1,899,297 | 2.09x | 34.8 | 16.7 | 2.6 | 13.7 | 1.4 | 0.8 | 318,248 | 1160 |
| s1e7-n10-g450m | 6338 | 100.00 | 901,182 | 1,939,601 | 2.15x | 70.3 | 32.7 | 5.1 | 26.8 | 3.0 | 1.4 | 318,959 | 1160 |
| s1e7-n10-g1g | 14084 | 100.00 | 893,015 | 1,962,751 | 2.20x | 157.7 | 71.8 | 11.1 | 59.2 | 6.2 | 2.9 | 317,871 | 1222 |
| s1e7-n100-g225m | 431 | 99.80 | 1,534,410 | 7,920,809 | 5.16x | 28.1 | 5.4 | 2.7 | 2.4 | 1.6 | 0.9 | 2,130,478 | 1181 |
| s1e7-n100-g450m | 863 | 99.92 | 1,527,805 | 8,234,824 | 5.39x | 56.5 | 10.5 | 5.3 | 4.5 | 3.0 | 1.7 | 2,165,594 | 1181 |
| s1e7-n100-g1g | 1919 | 99.98 | 1,532,212 | 8,469,676 | 5.53x | 125.2 | 22.7 | 11.6 | 9.6 | 6.5 | 3.6 | 2,189,010 | 1226 |
| s1e7-nmix-g225m | 1271 | 100.00 | 1,318,285 | 4,402,651 | 3.34x | 30.1 | 9.0 | 2.7 | 5.9 | 1.5 | 0.9 | 895,017 | 1160 |
| s1e7-nmix-g450m | 2449 | 100.00 | 1,323,780 | 4,655,969 | 3.52x | 60.2 | 17.1 | 5.3 | 11.0 | 2.9 | 1.6 | 927,739 | 1166 |
| s1e7-nmix-g1g | 5408 | 100.00 | 1,317,635 | 4,766,838 | 3.62x | 134.5 | 37.2 | 11.6 | 23.9 | 6.6 | 3.3 | 938,325 | 1234 |

### Thread scaling (s1e6, 450M; `taskset -c 0-(t-1)` for t ≤ 8, t=16 unpinned)

| threads | N=1 before | N=1 after | N=1 recover ms (after) | N=100 before | N=100 after |
|--:|--:|--:|--:|--:|--:|
| 1 | 31,920 | 32,513 | 518.1 | 1,099,793 | 1,776,194 |
| 2 | 60,973 | 63,461 | 263.0 | 1,408,736 | 3,334,418 |
| 4 | 112,038 | 121,793 | 135.0 | 1,673,484 | 5,685,410 |
| 8 | 191,424 | 229,489 | 71.4 | 1,808,954 | 8,509,049 |
| 16 | 205,094 | 249,665 | 65.5 | 1,816,033 | 8,118,786 |

Recovery scales 7.3x on 8 physical cores (91 % efficiency — rayon's chunking is fine,
per-thread batching has nothing to win) and gains only +9 % from SMT. N=100 went from
flat beyond 4 threads (1.1M → 1.8M) to 4.8x over 1 → 8 threads, and dips at 16.

### Final profile (single-threaded `--micro`, block 1, 1e6 accounts / 450M; the apply row is shard-parallel)

| function | N=1 ms | N=1 % | N=100 ms | N=100 % |
|---|--:|--:|--:|--:|
| ecrecover (`secp256k1` `recover_ecdsa`) | 498.14 | **96.4** | 25.02 | **53.9** |
| execute (`engine::execute`, serial executor) | 3.86 | 0.7 | 6.66 | 14.4 |
| … of which state lookups | 1.04 | 0.2 | 2.88 | 6.2 |
| tx hash keccak | 4.33 | 0.8 | 4.58 | 9.9 |
| commitment keccak (overlapped with recovery in `process_block`) | 3.15 | 0.6 | 4.34 | 9.4 |
| signing hash keccak (`signing_hash_of_body`) | 4.45 | 0.9 | 4.33 | 9.3 |
| apply (shard-parallel) | 0.25 | 0.0 | 1.25 | 2.7 |
| pubkey → address keccak | 2.26 | 0.4 | 0.04 | 0.1 |
| tx decode | 0.27 | 0.1 | 0.18 | 0.4 |

Baseline N=100: execute 32.09 ms (41.8 %), apply 6.40 ms serial. (The baseline's
2.25 ms N=1 signing-hash row was flattered by alloy's keccak cache answering the micro
loop's repeated inputs; the real per-block path misses it.)

**Where it stands.** N=1 is ecrecover (96 % of CPU, 91 %-efficient parallel scaling).
At N=100/1e6 a block costs ~9.4 ms ≈ 109 ns per payment on 8 cores. Justifiable floor
per payment: ecrecover/100 ≈ 286 ns + three keccak passes over its 28 wire bytes
(commitment, tx hash, signing hash) ≈ 150 ns + sort/merge ≈ 30 ns of CPU → ≈ 60 ns
wall on 8 cores, plus one state read and one state write into a 130 MB–1 GB map (DRAM,
partly overlapped): ~70–90 ns. The engine is within ~1.2–1.6x of that. Each keccak is
required (commitment = consensus, tx hash = `evict`, signing hash = recovery).

## Concerns

- **Public type change.** `FlatState::accounts` is now `lean_core::AccountMap` (64
  sharded maps with a keyed address hasher) instead of `HashMap<Address, Acct>`; every
  call site compiles unchanged except one node line (snapshot load:
  `AccountMap::with_capacity`). Anyone naming the old type must switch.
- **HashDoS.** The hasher is keyed per map from OS randomness (foldhash-strength, not a
  PRF); the shard index (top 6 address bits) is NOT keyed, so a grinder can pile
  accounts into one shard — that only serialises that shard's share of `apply` and
  grows one table; lookups stay keyed-hashed.
- **Allocator.** glibc re-faults ~31 MB/block of fresh pages in the bench; with
  `GLIBC_TUNABLES=glibc.malloc.trim_threshold=1G:glibc.malloc.mmap_threshold=1G` N=100
  is ~+9 % with no code change. The node binary would likely gain from a tuned or
  alternative allocator (binary-level, outside lean-core; not done).
- **Small blocks / host noise.** `execute` always uses rayon in parallel mode (no size
  cut-off) so the gate's differential test exercises the parallel path; the cost is
  tens of µs of fork/join on tiny blocks (gate `diff` step 38 → 42–48 s). The host's
  N=1 numbers are bimodal (~170k vs ~225k per invocation, same binary).
