# Lean lane integration — Malachite CL driving the lean lane node

Branch: `lean-lane-integration` (off `builder-separation`). Counterpart: `~/reth-fork`
branch `lean-native-transfer` (`crates/lean-lane-node`). Goal: replace the payment-lane
reth EL with the lean lane node (687k outputs/s standalone; block = {parent, number,
ts_ms, txs}, commitment = keccak chain — no Ethereum header), behind one experiment flag,
consensus-carried end to end on the 4-validator fleet.

## Shim contract v1.1 (2026-08-22, post-hardening — the plug-and-play boundary)

The CL is a thin client of FOUR verbs; ALL recovery machinery lives in the node
(user directive: robust EL, minimal CL, swappable):

- `arc_buildBlock{parentCommitment,number,timestampMs,budgetGas}` -> `{commitment, blockBytes}`
- `arc_newBlock{blockBytes}` -> `{"status":"VALID","commitment"}` (applied or already known)
  or `{"status":"SYNCING","number":<local head>}` (queued; node backfills itself from
  --peers and applies when the gap closes). Malformed bytes / conflicting block at a
  known height = JSON-RPC error (protocol violation, not lag).
- `arc_getHead` -> `{commitment,number,timestampMs}`
- `arc_getBlockBytes{number}` -> `{blockBytes|null}`

Node-side robustness (v1.1, fork `50f0039`): push-on-append gossip to peers,
SYNCING queue (bounded 512), background peer backfill, append-only log + snapshots
(restart = snapshot + replay, ~10s at 200k txs).

CL-side semantics (arc `da922f2..7f18196`), the FULL lean delta, flag-gated
ARC_PAYMENT_LEAN_LANE (default off = stock byte-identical):
1. Transport errors retried ~15s; past that = NO VERDICT (Err), never Invalid —
   a recorded Invalid sticks to a certified value forever (valid-round rule).
2. Validation tip test (parent==head && number==head+1) applies ONLY when
   number==head+1. number<=head = HISTORIC (sync replay while our consensus lags
   a self-healed node): valid iff byte-identical to our canonical block at that
   number. number>head+1 = we are behind: bounded peer catch-up, else Nil.
3. Decide anchor: wait-and-poll (30s deadline, SYNCING-aware); historic no-op
   when the cert binds an already-canonical block; stash removes only the
   decided entry (a full clear wiped next-height stashes = anchor races).
4. Sync serving verifies the offset-mapped lean block against the height's
   certificate and scans outward; EVM-only heights detected via
   commit_lanes(evm,None)==value_id.

CANDIDATE REMOVALS once v1.1 soaks clean (shrink the CL delta further): the
validation-time catch-up loop and the anchor's CL-side peer-fetch fallback —
both exist only to compensate for a node that could not heal itself.

## Why this composes cleanly with what exists

The deferred-exec CL already votes on STRUCTURE and executes in the vote gap. The lean
lane makes that stance total:
- Structural validation = recompute `commitment` from the lane bytes (pure hashing, no EL
  round-trip at all — cheaper than the EVM lane's `into_block_raw`).
- Execution = `arc_newBlock` on the local lean node (total STF: invalid tx = no-op), fired
  in the vote gap, anchored at decide. There is NO invalid-block halt path on this lane —
  a byzantine proposer can waste bytes, never stall a height.
- The builder pattern carries over unchanged conceptually; increment 1 uses local
  `arc_buildBlock` (the lean build is microseconds — remote builders become unnecessary
  for the experiment).

## Wire contract (shim RPC on the lean node, no JWT for the experiment)

- `arc_buildBlock {parentCommitment, number, timestampMs, budgetGas}` →
  `{commitment, blockBytes(base64)}` — pull best txs from pool, execute against a staged
  copy, DO NOT append. Deterministic for given inputs + pool snapshot.
- `arc_newBlock {blockBytes(base64)}` → `{commitment}` — decode, verify commitment +
  parent link, total-STF execute, append+fsync. IDEMPOTENT by commitment (re-feeding the
  head or an ancestor returns its commitment, no-op). This is the decide anchor AND the
  value-sync feed.
- `arc_getHead` → `{commitment, number, timestampMs}` — restart/reconnect anchor.
- Existing: `eth_sendRawTransaction`, `arc_sendRawTxBatch`, `txpool_status`,
  `eth_getTransactionCount`, recent-window `eth_getTransactionReceipt`.

`blockBytes` encoding (canonical, also the SSZ-carried consensus form):
`[parent 32B][number u64 LE][timestamp_ms u64 LE][n_txs u32 LE]([len u32 LE][tx bytes])*`

**Commitment formula v2 (SECURITY — supersedes the lean node's v1):**
`txs_hash = keccak256(bytes[48..])` — the ENTIRE framed tx section (count + per-tx
lengths + bodies), NOT the concatenated bodies. v1's concatenation-only binding let two
different boundary-framings of the same bytes share a commitment while decoding to
different tx lists; under total-STF both execute → same commitment, divergent state
(exploitable by a malicious sync peer). `commitment = keccak256(parent ‖ number LE ‖
timestamp_ms LE ‖ txs_hash)` unchanged. The lean node MUST adopt v2 (chain.rs).

## CL changes (arc repo), flag `ARC_PAYMENT_LEAN_LANE=1` (default off = byte-identical)

1. **types/block.rs**: `PaymentLane { Evm(ExecutionPayloadV3), Lean(Bytes) }` internal to
   the CL; lane framing gains `LEAN_LANE_BIT` (bit 62 of the length prefix — bit 63 is
   compact's). `commit_lanes` for the lean arm uses the lean COMMITMENT (recomputed from
   bytes, never trusted from the proposer). `value_id = keccak(evm_hash ‖ lean_commitment)`.
2. **eth-engine**: a small `LeanShim` client (reqwest JSON-RPC: buildBlock/newBlock/getHead)
   — NOT the Engine API; the payment `Engine` becomes an enum/either at the call sites the
   payment lane uses.
3. **get_value**: lean arm calls `arc_buildBlock(head, ts_from_evm_payload)` on the LOCAL
   lean node; assembles ConsensusBlock with the lean bytes.
4. **received_proposal_part / process_synced_value**: structural validation = decode
   blockBytes, recompute commitment, check parent-link against local head or pending
   chain; spawn `arc_newBlock` in the vote gap (idempotent).
5. **decided**: anchor = `arc_newBlock(decided bytes)` before commit; compare returned
   commitment against the certificate's lean_commitment (mismatch = loud halt — should be
   impossible given commitment is recomputed locally, kept as the invariant check).
6. **Sync**: same bytes, same path — `get_decided_values` fetches block bytes from the
   local lean node by number (needs `arc_getBlockBytes {number}` — shim method #4).

## Increments (each gated, committed separately)

- **I1 (fork, agent)**: shim server methods + `arc_getBlockBytes` + idempotency/parent-link
  tests + a two-node lockstep determinism test (node A builds, node B newBlocks, commitments
  equal over 10k blocks incl. crash-restart of B). ALSO the confidence gates from the
  review: decode fuzzing on arbitrary bytes.
- **I2 (arc)**: types/PaymentLane + framing + value_id + unit tests (round-trips, the
  equivocation guard analog for the lean commitment).
- **I3 (arc)**: LeanShim client + get_value/validation/decided/sync wiring behind the flag.
- **I4**: single-machine 4-validator run (demo scenario, lean node per validator, spammer
  `--mix fanout`), gates: 200+ heights all-4 identical lean commitment per height, CL
  restart leg, kill-a-lean-node leg (value-sync + newBlock idempotency recovery).
- **I5**: fleet run + the number (outputs/s consensus-carried), vs the 9.1k/21k records.

## Landmines to respect (from the ledger)

- SSZ/consensus wire change = the highest-risk class (dual-EL step 3). Everything behind
  the flag; flag-off byte-identical (framing bit untouched); fresh chains only.
- Never assume what bytes are — decode fully, recompute commitments, reject trailing bytes.
- A/B against unmodified peers is impossible for a NEW lane type — substitute: two-node
  determinism gate (I1) + all-4 lockstep (I4) + kill/restart legs.
- Spammer accounts derive at m/44'/60'/1'/0/i; lean node funds via --fund-file (wei-scale
  balances; fee/tx = (21000+5000N) gwei).
