# Lean payment lane — normative specification

Status: normative for `lean-lane` branch `engine` (derived from code at `39804fe`).
Scope: the byte formats and the state transition function (STF) — everything that
determines a block commitment or a post-state. Networking, pool admission, RPC and
consensus integration are out of scope except where noted as non-normative.

Key words MUST / MUST NOT / SHOULD are as in RFC 2119. Where this document and any
comment or other doc in the repo disagree, this document follows the code; the
disagreements are listed in Appendix B.

## 0. Conventions and primitives

- `keccak256` = Keccak-256 as used by Ethereum (pre-NIST padding). `keccak256("")` =
  `0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470`.
- `‖` = byte concatenation. `uN_le(x)` / `uN_be(x)` = x as an N-bit unsigned integer,
  little- / big-endian, exactly N/8 bytes.
- **All integers in lane formats are little-endian**, except the chain id inside the two
  domain strings (§1.4, §2.3), which is **big-endian**.
- Address = 20 raw bytes. Address order (for sorting) = lexicographic on the 20 bytes.
- secp256k1 group order `n = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141`.

## 1. Transaction (type `0x50`, "fan-out transfer")

### 1.1 Constants

| name | value | meaning |
|---|---|---|
| `LEAN_TX_TYPE` | `0x50` | first byte of every canonical tx |
| `MAX_OUTPUTS` | `10000` | max `n_outputs` |
| `HEADER_LEN` | `7` | type + nonce + n_outputs |
| `OUTPUT_LEN` | `28` | address + amount |
| `SIG_LEN` | `65` | r ‖ s ‖ v |
| `AMOUNT_UNIT` | `1_000_000_000` (1 gwei) | wei per wire amount unit |
| `GAS_BASE` | `21000` | |
| `GAS_PER_OUTPUT` | `5000` | |
| `GAS_PRICE` | `1_000_000_000` wei | protocol-fixed |

### 1.2 Canonical bytes

```
offset        size  field
0             1     type       = 0x50
1             4     nonce      u32 LE
5             2     n_outputs  u16 LE, MUST satisfy 1 <= n_outputs <= 10000
7 + 28*i      20    to_i       address          (i = 0 .. n_outputs-1)
27 + 28*i     8     amount_i   u64 LE, in AMOUNT_UNIT (wei value = amount_i * 1e9)
7 + 28*N      65    sig        r (32, BE) ‖ s (32, BE) ‖ v (1)
```

Total length = `72 + 28*N`. `body` = the first `7 + 28*N` bytes (everything before
`sig`). Outputs are an ordered list; duplicates, zero amounts, the zero address and the
sender itself are all permitted as recipients.

### 1.3 Decoding (`decode`)

Given a byte string `b`, a tx is **well-formed** iff, in this order:
1. `len(b) >= 1` and `b[0] == 0x50`;
2. `len(b) - 1 >= 71` (6 header bytes after the type + 65 sig bytes);
3. `N = u16_le(b[5..7])` satisfies `1 <= N <= 10000`;
4. `len(b) >= 72 + 28*N`.

**Strict** decode (used for the standalone form, `LeanTx::decode`) additionally requires
`len(b) == 72 + 28*N` (no trailing bytes). **Prefix** decode (used for block entries and
for pool admission, see §2.4 and Appendix B.1) ignores bytes beyond `72 + 28*N`; the
parsed tx and its canonical bytes are then `b[0 .. 72+28*N]`.

### 1.4 Signing domain and signing hash

```
lane_domain(chain_id) = keccak256( "ARC_LEAN_NATIVE_TRANSFER" (24 ASCII bytes) ‖ u64_be(chain_id) )   -- 32-byte preimage
signing_hash          = keccak256( lane_domain(chain_id) ‖ body )
```

The chain id is not on the wire; it is bound only through the domain. Default chain id
is **1338** (node flag `--chain-id`).

### 1.5 Signature and sender recovery

- `r = sig[0..32]`, `s = sig[32..64]` (big-endian scalars), `v = sig[64]`.
- `v` is the raw secp256k1 recovery id. Values `0,1,2,3` are syntactically accepted
  (libsecp256k1 semantics: bit 0 = y parity, bit 1 = x = r + n). **`v >= 4` fails —
  in particular Ethereum-style `27/28` fail.** In practice `v ∈ {2,3}` also fails
  (recovery with x = r + n is only possible when r < p − n, ~2^-128 odds).
- `r >= n` or `s >= n` fails; `r == 0` or `s == 0` fails at recovery.
- **High-s is accepted**: no `s <= n/2` rule. `(r, n − s, v ^ 1)` recovers the same
  public key; it is a second valid encoding of the same tx with a different tx hash
  (Appendix A, V5).
- Recovery: ECDSA public-key recovery over the 32-byte message `signing_hash`
  (no further hashing). Failure ⇒ the tx is **unrecoverable**.
- `sender = keccak256(uncompressed_pubkey[1..65])[12..32]` (standard Ethereum address).

### 1.6 Tx hash, gas, fee

```
tx_hash    = keccak256( canonical bytes, type byte and sig included )
lean_gas(N) = 21000 + 5000 * N
lean_fee(N) = lean_gas(N) * GAS_PRICE      (wei, u128)     e.g. N=1: 26e12, N=2: 31e12, N=3: 36e12
```

Gas is not on the wire. It is the builder's packing weight and determines the fee.
The tx hash affects no state; it is used only for pool eviction.

## 2. Block

### 2.1 Wire form (the consensus-carried bytes)

```
offset  size  field
0       32    parent        commitment of the parent block
32      8     number        u64 LE
40      8     timestamp_ms  u64 LE
48      4     n_txs         u32 LE
52      ...   n_txs × ( len_i u32 LE ‖ entry_i (len_i bytes) )
```

A wire block is **well-framed** iff `len >= 52`, every `len_i` prefix and body fits, and
the last entry ends exactly at the end of the buffer (no trailing bytes). A block that is
not well-framed is rejected as a whole (it has no commitment). Entry contents are NOT
validated by framing: zero-length or garbage entries are well-framed.

### 2.2 Commitment

```
txs_hash   = keccak256( wire[48..] )  =  keccak256( u32_le(n_txs) ‖ ( u32_le(len_i) ‖ entry_i )* )
commitment = keccak256( parent(32) ‖ u64_le(number) ‖ u64_le(timestamp_ms) ‖ txs_hash(32) )   -- 80-byte preimage
```

The commitment binds the exact entry bytes (including any bytes a prefix decode ignores)
and the framing. The commitment is never carried; receivers recompute it.

### 2.3 Genesis

```
GENESIS_COMMITMENT(chain_id) = keccak256( "ARC_LEAN_LANE_GENESIS" (21 ASCII bytes) ‖ u64_be(chain_id) )
```

The genesis head is `(commitment = GENESIS_COMMITMENT, number = 0, timestamp_ms = 0)`.
Block 0 is a sentinel, never a real block.

### 2.4 Entry classification (per entry, in block order)

Each `entry_i` is classified independently:

- **Lean tx**: `entry_i[0] == 0x50`, prefix-decode (§1.3) succeeds, and sender recovery
  (§1.5) succeeds. Yields the item `(sender, tx, tx_hash)`, where `tx_hash` is over the
  canonical prefix only.
- **Skipped** (everything else): empty entry, any other first byte (including valid
  Ethereum legacy/typed txs), a `0x50` entry that fails §1.3 steps 1–4, or an
  unrecoverable signature. Skipped entries have **no effect**: not executed, not counted
  (neither applied nor no-op), no account written.

`items` = the list of lean-tx items in entry order. Only `items` reach the STF.

### 2.5 Chain linkage and limits

- A block is appended iff `number == head.number + 1` and `parent == head.commitment`.
  The node enforces **no timestamp rule** (monotonicity, lockstep with the EVM lane
  `timestamp_ms == evm_ts * 1000` etc. are enforced by the consensus layer only);
  `timestamp_ms` affects only the commitment.
- No protocol limit on `n_txs` (u32), entry size (u32) or total gas of a received block.
  `budget_gas` constrains only the local builder. (Transport: RPC bodies are capped at
  64 MiB — non-normative.)

## 3. State

### 3.1 Account record and default

`state: Address → Acct { nonce: u64, balance: u128 (wei) }`. An address absent from the
map reads as `(0, 0)`. Presence matters: an entry explicitly holding `(0, 0)` is distinct
from absence for the digest (§3.4).

### 3.2 Genesis state

The node funds addresses from `--fund-file`: UTF-8 text, one address per line; each line
is whitespace-trimmed, blank lines skipped, every other line MUST parse as a 20-byte hex
address (`0x` optional, case-insensitive, checksum not enforced; any other line, including
`#` comments, is a fatal error). Each listed address is set (not added) to
`(nonce 0, balance = --fund-balance)`; duplicates collapse. `--fund-balance` is in
**wei**, u128, default `1_000_000_000_000_000` (1e15). No file ⇒ empty genesis state.
Funding is applied only on a datadir with no replayed blocks and no snapshot above 0.
Genesis state is therefore a function of (fund file set, `--fund-balance`); the genesis
commitment is a function of chain id only (state is not committed).

### 3.3 Beneficiary

`BENEFICIARY = 0x00000000000000000000000000000000000000be` — a fixed constant
(`Address::with_last_byte(0xbe)`); no flag changes it.

### 3.4 State digest (`FlatState::digest`)

```
entries = all (address, acct) in the map, sorted ascending by address
digest  = keccak256( ‖ over entries of ( address(20) ‖ u64_le(nonce) ‖ u128_le(balance) ) )   -- 44 bytes/entry
```

There is no explicit count field or separator; the count is implicit in the preimage
length. The empty state's digest is `keccak256("")`. The digest is not part of any
commitment; it is the equality check for recovery and differential testing, and it is
sensitive to the **write set** (§4.3), not just to non-zero values.

## 4. State transition function

### 4.1 Definition

`execute(state, items, BENEFICIARY) → writes` is pure; `apply` then sets
`state[a] = acct` for every `(a, acct)` in `writes`. Normative pseudo-code
(all arithmetic on u128 unless stated; `get(a)` = `state[a]` or `(0,0)`):

```
post    := empty map Address → Acct          # accounts touched this block
credits := empty map Address → u128
fees := 0; applied := 0; outputs := 0
for (sender, tx, _) in items:                # block order
    e := post.get_or_insert(sender, get(sender))   # sender is written EVEN IF no-op
    fee := lean_fee(len(tx.outputs))
    ok := (u64(tx.nonce) == e.nonce)               # exact nonce
    total := fee
    if ok:
        for o in tx.outputs:
            total := checked_add(total, u128(o.amount) * 1e9)
            if overflow: ok := false; break
    if not ok or e.balance < total: continue       # NO-OP: nothing else happens
    e.balance -= total                             # debit visible to later txs of this sender
    e.nonce   += 1
    for o in tx.outputs: credits[o.to] += u128(o.amount) * 1e9   # deferred
    outputs += len(tx.outputs); fees += fee; applied += 1
for (a, w) in credits:  post.get_or_insert(a, get(a)).balance += w
post.get_or_insert(BENEFICIARY, get(BENEFICIARY)).balance += fees   # ALWAYS, even fees == 0
writes := post as a list sorted by address
outcome := { applied, noops = len(items) - applied, outputs, fees_to_beneficiary = fees }
```

### 4.2 Rules (consequences of 4.1, stated for clarity)

1. **Order**: items execute strictly in entry order.
2. **Exact nonce**: a tx applies only if its nonce equals the sender's current in-block
   nonce. A sender may apply consecutive nonces `k, k+1, …` in one block.
3. **Total**: `fee + Σ amount_i·1e9`. Checked overflow ⇒ no-op (unreachable in
   practice: N ≤ 10000 bounds total below ~1.9e32).
4. **Insufficient funds**: `balance < total` ⇒ no-op (equality applies).
5. **Atomicity**: a tx either applies fully (debit, nonce+1, all credits, fee) or is a
   no-op. A no-op changes no balance or nonce and credits no recipient.
6. **Same-block credits are not spendable**: a sender's spendable balance is its
   block-start balance minus its own earlier debits in the block. Credits (including
   self-transfers and fee income) land only after all txs are processed.
7. **Duplicate recipients** (within a tx or across txs) sum. **Self-transfer** debits
   `total` and later credits the amounts back (net effect: −fee, nonce+1).
8. **Fees**: only applied txs pay; the sum is credited to `BENEFICIARY` once, at end of
   block. If the beneficiary is also a sender, its fee income is not spendable in-block.
9. **No receipts, logs, or per-block state root** are produced.

### 4.3 Write set (digest-relevant)

`writes` contains exactly: every `sender` of every item (applied **or no-op**; a no-op
sender is written at its existing value, so an unknown no-op sender acquires an explicit
`(0,0)` entry); every recipient of an **applied** tx (a zero-amount recipient acquires an
entry); and `BENEFICIARY` (every block, including empty blocks). Recipients of a no-op tx
and senders of skipped entries are never written. Entries are never deleted.

### 4.4 Arithmetic domain

`nonce` is u64 in state but u32 on the wire: once an account's nonce reaches 2^32 it can
never send again. `+=` on balances is unchecked: in the release build it wraps mod
2^128, in a debug build it panics. Only reachable if balances approach 2^128 (e.g. an
absurd `--fund-balance`); conforming implementations SHOULD treat such inputs as out of
domain (Appendix B).

## 5. Chain state, recovery and equivalence

- Canonical state at height h = genesis state (§3.2) followed by `apply(execute(…))` for
  blocks 1..h in order, each over its §2.4 `items`. Every path (vote-gap stage + promote,
  direct commit, log replay, snapshot + replay) calls the same `execute`, so all MUST
  yield an identical digest.
- **Snapshot** (non-normative for consensus): file `snapshot-<number>.bin` =
  `commitment(32) ‖ u64_le(number) ‖ u64_le(timestamp_ms) ‖ u64_le(count) ‖ count × 44-byte
  entries` in **unspecified order** (hash-map iteration), so snapshot bytes differ between
  nodes; only the digest is canonical. A genesis snapshot at number 0 is written after
  funding. Snapshot every `--snapshot-every` (default 1024; 0 = never) blocks where
  `number % k == 0`.
- **Block log** (non-normative): record = `u32_le(L) ‖ payload ‖ u32_le(L)`, payload =
  `parent ‖ u64_le(number) ‖ u64_le(ts) ‖ commitment ‖ u32_le(n_txs) ‖ (u32_le(len) ‖
  entry)*`. Replay stops at the first record whose length echo or recomputed commitment
  fails, and truncates there.

## Appendix A — test vectors (chain id 1338, generated by running the code)

Keys are 32-byte big-endian secrets `k1 = 0x00…01`, `k2 = 0x00…02`, `k3 = 0x00…03`,
`k9 = 0x00…09`. Signatures are RFC 6979 deterministic (libsecp256k1 default, no extra
entropy), so they are reproducible.

**V1 — domains and constants**
```
lane_domain(1338)          0xd836bb4e7944843ac5be0c4094de2459090af9911964b0d4f9d1eaab9e79f3d5
GENESIS_COMMITMENT(1338)   0xef24da0138bf37159737c3154c5e0a261b50dc3eea4b490975abc6b1215e5770
GENESIS_COMMITMENT(1337)   0x255809a2b8fb46ec0cc08f722c443211a405366c5fbb0eb700dcbae3c0b3cad9
digest(empty state)        0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470
addr(k1) 0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf   addr(k2) 0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF
addr(k3) 0x6813Eb9362372EEF6200f3b1dbC3f819671cBA69   addr(k9) 0xF7Edc8FA1eCc32967F827C9043FcAe6ba73afA5c
```

**V2 — signed tx** `T0`: key k1, nonce 0, outputs `[(0x1111…11, 5), (0x2222…22, 1_000_000_000)]`
(1e18 wei to 0x22…). 128 bytes; `body` = first 63 bytes.
```
signing_hash  0x486c80d3a44ff800c886164aa71a32af1878e3453849f9f5289e3eb250369095
sender        0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf
tx_hash       0x36964dc5ce2098e75107e8badf4c4f29f17d476e10c1e946eeda83a3ffd3c427
bytes         0x5000000000020011111111111111111111111111111111111111110500000000000000222222222222222222222222222222222222222200ca9a3b00000000886e0c1954937a3673e4c0dbaaa7144708e75e90986e4d8eeb7d410aaa2c20f92c20267719c0ee115e3bd15ac55096489e60a932d936ab51b923e3f29892f84000
```

**V3 — block with 3 txs, one no-op.** Pre-state: k1, k2, k3 each `(nonce 0, balance 1e19)`,
pre-digest `0xc1f91687aba143b488b31ddfb40fb4c845583df50cc305a30f1baf0446087ea6`.
`T0` = V2. `T1`: k2, nonce 0, `[(addr(k3), 2_000_000_000), (0x2222…22, 3)]`.
`T2`: k3, nonce 0, `[(0x3333…33, 10_500_000_000)]` — needs 10.5e18 + 26e12 wei; k3 holds
1e19 at block start plus 2e18 credited by T1 in this block ⇒ **no-op** (rule 4.2.6).
```
T1 hash 0xe281aa4b4defd5f63af5f0e4ed2bf397de26ce79520504f352c838d083328362
T1      0x500000000002006813eb9362372eef6200f3b1dbc3f819671cba69009435770000000022222222222222222222222222222222222222220300000000000000798f6562ecbfaffe2ee8d7e92740298b86584dd0595f7a604ef503107e3bdb6f486605530bb042415eae857137e55196081c84da65acafe2485baa4d4e2b1e7601
T2 hash 0x83133719eeb10dea1ebc2a45e93d531efa119418606bcce17caf2b508133c359
T2      0x5000000000010033333333333333333333333333333333333333330049d9710200000046fa19349e7e6053ae80601e15d462bcf7d17af733db4d69e8b43f0a4a02053560e8a3767ce3d88efa788376678f144b90c56c94074d486b7eb2557ba8e3fc9701
```
Block: parent = GENESIS_COMMITMENT(1338), number 1, timestamp_ms 1_700_000_000_000, entries `[T0, T1, T2]`, 420 bytes:
```
wire        0xef24da0138bf37159737c3154c5e0a261b50dc3eea4b490975abc6b1215e577001000000000000000068e5cf8b01000003000000800000005000000000020011111111111111111111111111111111111111110500000000000000222222222222222222222222222222222222222200ca9a3b00000000886e0c1954937a3673e4c0dbaaa7144708e75e90986e4d8eeb7d410aaa2c20f92c20267719c0ee115e3bd15ac55096489e60a932d936ab51b923e3f29892f8400080000000500000000002006813eb9362372eef6200f3b1dbc3f819671cba69009435770000000022222222222222222222222222222222222222220300000000000000798f6562ecbfaffe2ee8d7e92740298b86584dd0595f7a604ef503107e3bdb6f486605530bb042415eae857137e55196081c84da65acafe2485baa4d4e2b1e7601640000005000000000010033333333333333333333333333333333333333330049d9710200000046fa19349e7e6053ae80601e15d462bcf7d17af733db4d69e8b43f0a4a02053560e8a3767ce3d88efa788376678f144b90c56c94074d486b7eb2557ba8e3fc9701
txs_hash    0x020d1e85fb0e4ed45ebee17a6266b7f4b0d2ab04f0f4c1ed00f36016d7f2149d
commitment  0x095ba18fbc5764978ce4c46044de763b1630e0a3f6bbaf1e0786a612214be03b
outcome     applied 2, noops 1, outputs 4, fees_to_beneficiary 62000000000000
post-state (sorted):
  0x00000000000000000000000000000000000000be  nonce 0  balance 62000000000000
  0x1111111111111111111111111111111111111111  nonce 0  balance 5000000000
  0x2222222222222222222222222222222222222222  nonce 0  balance 1000000003000000000
  0x2B5AD5c4795c026514f8317c7a215E218DcCD6cF  nonce 1  balance 7999968997000000000
  0x6813Eb9362372EEF6200f3b1dbC3f819671cBA69  nonce 0  balance 12000000000000000000
  0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf  nonce 1  balance 8999968995000000000
post-digest 0xa17fd987cd27f9aed1cb9704025a008371d93ecfe1191dbdb9d569f03f7f3f8a
```
(0x3333…33 is absent: a no-op's recipient is never written.)

**V4 — empty blocks.** `txs_hash(no entries) = keccak256(00000000) =
0xe8e77626586f73b955364c7b4bbf0bb7f7685ebd40e852b164633a4acbd3244c`.
Block 2 on V3: parent V3 commitment, number 2, timestamp_ms 1_700_000_000_500:
```
wire        0x095ba18fbc5764978ce4c46044de763b1630e0a3f6bbaf1e0786a612214be03b0200000000000000f469e5cf8b01000000000000
commitment  0x0d99c95bba358956556d8b60084682c479195007ad0ef8792acad9746e205a40
```
An empty block on the EMPTY state writes `0x…be = (0,0)`: digest becomes
`0x6e1bd6f9532cfbf5580724858fcedc7089a23d8307c0594ef5dd150f4c1fda9f` (≠ `keccak256("")`).

**V5 — high-s twin of T0** (`s' = n − s`, `v' = 1`): recovers to addr(k1), accepted in a block.
```
bytes    0x5000000000020011111111111111111111111111111111111111110500000000000000222222222222222222222222222222222222222200ca9a3b00000000886e0c1954937a3673e4c0dbaaa7144708e75e90986e4d8eeb7d410aaa2c20f9d3dfd988e63f11eea1c42ea53aaf69b61c4e33b3d611f4ea06ae7a9a37a3490101
tx_hash  0x5b3580c6a86476b5c39c7c368793b22bf7299b3ad51257c73e6be202fe8b340c
```

**V6 — v byte.** T0 with `v` replaced by 2, 3, 4, 27 or 28: recovery fails ⇒ entry skipped.

**V7 — trailing bytes.** Entry = T0 bytes ‖ `dead`: strict decode fails (TrailingBytes),
but as a block entry it is a lean tx with `tx_hash` = T0's
(`0x36964d…c427`); the block commitment still covers all 130 bytes.

**V8 — skipped vs no-op.** Empty state, parent GENESIS(1338), number 1, timestamp_ms 0,
entries `[ "" , 500000 , 0201 , T9 ]` where T9 = k9, nonce 0, `[(0x4444…44, 1)]` (k9 unfunded).
```
commitment  0xf89650c46f7da024f15e6c5530d66bd7e076b350674a808df40f9139ab58f18c
items 1; outcome applied 0, noops 1, outputs 0, fees 0
post-state: 0x…be (0,0); 0xF7Edc8FA1eCc32967F827C9043FcAe6ba73afA5c (0,0)
post-digest 0x162235580f8292ac3bf1e0091420f8a16d448afa47cd4a6c687b8c7061285fff
```

## Appendix B — code-vs-docs disagreements and conscious surprises

**Disagreements (code wins):**
1. `lean-native` crate docs / `exec.rs` header: an invalid tx leaves "state untouched".
   Code writes the no-op sender's entry (value unchanged), creating `(0,0)` for an unknown
   sender — digest-visible (§4.3, V8).
2. `exec.rs` `Delta` doc: "`FlatState::digest` covers the account count". There is no
   count field; it is only implicit in preimage length (§3.4).
3. `docs/integration.md` / README: "genesis state is a pure function of `--chain-id` and
   `--fund-file`". It also depends on `--fund-balance`; the chain id affects only the
   commitment/domain, not the state.
4. arc-node `CLAUDE.md` §6: lean `getTransactionCount` is "pool-inclusive". Code
   (`rpc.rs`) and `docs/integration.md` return the **committed** nonce.
5. `TxError::UnknownSender` / `NonceMismatch` exist but are unused: unknown senders are
   simply `(0,0)`; nonce mismatch is a silent no-op.

**Surprises / implementation-defined (conscious decisions):**
6. High-s signatures accepted (§1.5); each tx has a malleable twin with another tx hash.
   Only one can apply (nonce), but both may occupy block space.
7. `v` is a raw recovery id 0..3; `27/28` are rejected (skipped).
8. Prefix decode: bytes trailing a valid tx inside a block entry (and at pool admission,
   `decode_2718` is not exact) are ignored; the entry executes; tx hash excludes them.
9. u32 wire nonce vs u64 state nonce: accounts freeze at nonce 2^32. Balance `+=` is
   unchecked (wraps in release, panics in debug) — unreachable without ~2^128 balances.
10. Beneficiary is a hard-coded constant `0x…be`, written every block even when no fee
    is paid; same-block credits (incl. fees, self-transfers) are not spendable; the node
    enforces no timestamp rule and no received-block size/gas limit; the checked-add
    output-sum overflow path is unreachable with `MAX_OUTPUTS = 10000`.
