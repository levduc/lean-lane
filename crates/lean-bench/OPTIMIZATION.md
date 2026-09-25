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
