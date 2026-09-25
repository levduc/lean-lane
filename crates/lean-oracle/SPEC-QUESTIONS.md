# SPEC-QUESTIONS — lean-oracle

Places where `SPEC.md` is silent or ambiguous, and what this oracle chose. The oracle was
written from `SPEC.md` alone; none of these were resolved by reading the engine's code.
Differential tests against the engine should probe each one.

All Appendix A vectors (V1–V8) reproduce exactly: **no SPEC/CODE CONFLICTS** found.

| # | Section | Question | Oracle's choice |
|---|---|---|---|
| Q1 | §1.5 | The message is "the 32-byte `signing_hash` (no further hashing)". When that hash, read as a big-endian integer, is ≥ n, is it reduced mod n (standard ECDSA / libsecp256k1 `scalar_set_b32`) or rejected? | Reduced mod n (standard ECDSA). |
| Q2 | §1.5 | Is a recovered public key equal to the point at infinity a failure? (Not mentioned; only reachable with a crafted `(r, s)` for a known message.) | Failure ⇒ unrecoverable ⇒ entry skipped. |
| Q3 | §3.2 | "`0x` optional, case-insensitive": does case-insensitivity also cover the prefix, i.e. is `0X…` accepted? | Only lowercase `0x` is stripped; `0X…` is a fatal line. Hex digits are case-insensitive. |
| Q4 | §3.2 | "whitespace-trimmed": ASCII whitespace or Unicode White_Space? And are line endings `\n` only or also `\r\n`? | Rust `str::lines()` (splits on `\n`, strips a trailing `\r`) then `str::trim()` (Unicode White_Space). |
| Q5 | §3.2 | With `--fund-balance 0`, does each listed address still get an explicit `(0, 0)` entry (digest-visible)? | Yes — "each listed address is set" is applied literally, regardless of value. |
| Q6 | §3.2 | Is an interior-whitespace line (e.g. two addresses on one line) or a line of 40 hex digits with a `0x` prefix and trailing garbage fatal? | Fatal: after trimming, the line must be exactly `[0x]` + 40 hex digits. |
| Q7 | §4.4 | Balance/credit `+=` overflow: the spec says release wraps, debug panics, and conforming implementations SHOULD treat such inputs as out of domain. | The oracle panics with "§4.4 … out of domain" (checked add) on both credit accumulation and balance credit — never wraps. |
| Q8 | §4.1 | `credits[o.to] += …` accumulates in a u128 before being added to the balance; the spec does not say whether overflow of the *accumulator* (distinct from the balance) is possible or how it behaves. | Treated like Q7 (out of domain, panic). Unreachable in practice. |
| Q9 | §2.1 / §2.5 | Does framing/decoding ever consider chain linkage, i.e. is a block with a wrong `parent`/`number` still "well-framed" and does it still have a commitment? | Yes: framing and commitment are independent of linkage. `apply_block` does not check linkage (the state carries no head); `Chain::append` checks §2.5 and refuses without touching state. |
| Q10 | §4.1 | `outcome` has four fields; the spec does not say whether skipped entries (§2.4) are reported. | `Outcome` carries the four §4.1 counters plus informational `items` and `skipped`; differential tests should compare only the four §4.1 fields unless the engine exposes the others. |
| Q11 | §1.3 | Step 2 (`len(b) - 1 >= 71`) is subsumed by step 4 for every valid N; is the ordering of failures observable (distinct error kinds)? | Checks are performed in the listed order with distinct error variants (`Empty`, `WrongType`, `TooShort`, `BadOutputCount`, `Truncated`, `TrailingBytes`); only well-formed vs not is normative. |
| Q12 | §1.5 | For `v ∈ {2,3}` the spec says recovery "with x = r + n" is possible only when `r < p − n`. It does not state what happens if `r + n ≥ p` but `< 2^256` vs `≥ 2^256`. | Either case fails: `r + n` overflowing 256 bits ⇒ fail; `r + n ≥ p` ⇒ not a field element ⇒ fail. |

## API deviations from the requested shape

- `apply_block` returns `Result<Outcome, BlockError>` rather than a bare `Outcome`: a block
  that is not well-framed (§2.1) "is rejected as a whole", and the oracle leaves the state
  untouched in that case instead of inventing an outcome.
