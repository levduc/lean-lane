# Integration contract — driving a lean-lane node from a BFT consensus layer

This is the plug-and-play boundary. A consensus layer (CL) that implements the
client side of this document can carry the lean lane beside its existing
execution lane under one certificate. The reference client is Arc's Malachite CL
(`crates/eth-engine/src/lean_shim.rs` there, ~240 lines).

Design stance: **robust node, thin CL.** All recovery machinery — gossip,
backfill, snapshots, the SYNCING queue — lives in the node. The CL is a client
of five idempotent verbs and never needs a forkchoice, payload IDs, or a JWT.

## 1. Block bytes (canonical, also the consensus-carried form)

```
[parent 32B][number u64 LE][timestamp_ms u64 LE][n_txs u32 LE] ( [len u32 LE][tx bytes] )*
```

Transactions are canonical EIP-2718 bytes (type `0x50` fan-out, see
`crates/lean-native`). The CL never has to decode a transaction; it decodes the
header and walks the length-prefixed tx section to check for truncation and
trailing bytes.

**Commitment** (recomputed by every party from the bytes, never trusted from the
wire):

```
txs_hash   = keccak256( bytes[48..] )                       -- the whole framed tx section
commitment = keccak256( parent ‖ number LE ‖ timestamp_ms LE ‖ txs_hash )
genesis    = keccak256( "ARC_LEAN_LANE_GENESIS" ‖ chain_id u64 BE )
```

`txs_hash` binds the framing (count and per-tx lengths), not just concatenated
bodies — two different boundary-framings of the same bytes must not share a
commitment.

Genesis state is a pure function of `--chain-id` and `--fund-file`; every node in
a fleet must start with the same fund file or they will disagree at height 1.

## 2. The shim (JSON-RPC 2.0 over HTTP, by-name params, base64 block bytes)

| verb | params | result |
|---|---|---|
| `arc_buildBlock` | `{parentCommitment, number, timestampMs, budgetGas}` | `{commitment, blockBytes}` — pulls best txs from the pool, executes against a staged copy, **does not append** |
| `arc_stageBlock` | `{blockBytes}` | `{"status":"STAGED", commitment}` or `{"status":"SYNCING", number}` — validate + execute into a staged entry without appending, so a later `arc_newBlock` of the same bytes promotes instead of executing. Speculative; safe to fire-and-forget |
| `arc_newBlock` | `{blockBytes}` **or** `{commitment}` | `{"status":"VALID", commitment}` (appended, or already known — idempotent by commitment) or `{"status":"SYNCING", number:<local head>}`. By commitment: promotes a staged copy, else applies a queued copy, else fetches the bytes from `--peers` by commitment and verifies them, else SYNCING |
| `arc_getHead` | `{}` | `{commitment, number, timestampMs}` |
| `arc_getBlockBytes` | `{number}` **or** `{commitment}` | `{blockBytes}` or `{blockBytes: null}` — canonical, staged or queued block |

Malformed bytes, a wrong commitment, or a *conflicting* block at a known height
are JSON-RPC **errors** (protocol violations); lag is never an error, it is
`SYNCING`.

Also served, not part of the CL contract: `eth_sendRawTransaction`,
`arc_sendRawTxBatch`, `txpool_status`, `eth_getTransactionCount`
(**pool-inclusive**), a recent-window `eth_getTransactionReceipt`, and
`arc_announceBlock` (node-to-node gossip).

Total-STF semantics: an invalid transaction inside a block is a no-op, never a
block rejection. A byzantine proposer can waste bytes; it cannot fork or halt the
lane.

## 3. What the reference CL does with it (Arc, `ARC_PAYMENT_LEAN_LANE=1`)

| phase | call | rule |
|---|---|---|
| boot | `arc_getHead` | retried without bound — a CL must not park because its lean node came up later |
| propose | `arc_buildBlock(head.commitment, head.number+1, evm_timestamp*1000, budget)` | timestamp is taken from the EVM payload so both lanes stay in lockstep; the bytes ride in the proposal beside the EVM payload |
| validate (vote) | `arc_getHead`, `arc_getBlockBytes` when behind, then `arc_stageBlock` | **structural only**: recompute commitment; `timestamp_ms == evm_ts*1000`; `parent == head.commitment && number == head.number+1`. `number <= head` is a historic replay and is valid iff byte-equal to our canonical block; `number > head+1` means *we* are behind — pull from peer nodes first, then judge. No execution verdict at vote time |
| decide (anchor) | `arc_newBlock(decided bytes)` | wait-and-poll up to 30 s (SYNCING-aware); the returned commitment must equal the certificate's lean commitment (it is recomputed locally, so a mismatch is a bug, not an attack) |
| decide (v0.2) | `arc_newBlock{commitment = EVM header prev_randao}` | the node holds the bytes (staged at validation); the CL carries none |
| sync serve | `arc_getBlockBytes(number)` | re-framed with the EVM payload; verified against the height's certificate before it is sent |
| sync serve (v0.2) | `arc_getBlockBytes{commitment = EVM header prev_randao}` | no number mapping and no scan: the header names the block. The CL recomputes the commitment from the bytes and checks it against the header before framing them; a height this node cannot answer is skipped and a peer serves it |
| sync receive | same as validate, then `arc_newBlock` | |

Certificate: v0.1 `value_id = keccak(evm_block_hash ‖ lean_commitment)`; without
a lean payload `value_id = evm_block_hash` (stock). v0.2 votes the plain EVM
block hash always — the lean block is bound by the header's `prev_randao`
instead, so the value id is upstream's.

**Transient errors.** Every verb retries transport errors for ~15 s (30 × 500 ms,
covers a node restart with snapshot replay). Past that the CL treats the node as
*away*: no verdict for this round (never "Invalid" — an Invalid recorded against
a certified value sticks forever), the anchor keeps polling to its deadline, and
value-sync replies "no value" so the peer re-requests. It is never
process-fatal. Live-tested: lean node killed for 60 s under load, and a CL
restarted while its lean node was down — both recovered without operator action.

## 4. Operating rules learned the hard way

- Start the lean node before the CL when possible (the CL waits, but a parked CL
  costs a validator's proposer turns until someone notices).
- Enable the lane on **all** validators together, from height 1 of a fresh chain.
  The wire format of consensus values changes when the flag is on; a mixed-flag
  fleet fails closed with decode errors.
- Never wipe a lean chain under a live consensus chain: certificates bind the
  destroyed bytes and laggards can never sync that span.
- `eth_getTransactionCount` is pool-inclusive: generate offline corpora only
  against an empty pool.
- Fan-out drains sender balances one way; refund by regenerating genesis.
