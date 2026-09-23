//! Vote-gap staging (`arc_stageBlock`), the staged map, claims and the two
//! anchor graces that wait for a stage instead of pulling from a peer.

use super::*;

/// How long an anchor that holds none of the block waits for an
/// `arc_stageBlock` which has ARRIVED but not yet registered its commitment
/// (see `await_stage_race`). Only waited while `stage_pending > 0`.
const STAGE_RACE_GRACE: std::time::Duration = std::time::Duration::from_millis(40);

/// Where one `arc_stageBlock` spent its time (the `lean_stage` debug event).
#[derive(Default)]
struct StageTiming {
    decode_ms: f64,
    recover_ms: f64,
    apply_ms: f64,
    /// already staged against this head: nothing was executed
    noop: bool,
}

impl LaneNode {
    /// Publish a staged entry: replace any entry for the same commitment, keep
    /// the map bounded at 32. The ONLY writer of `self.staged` outside
    /// promotion's retention sweep.
    pub(super) async fn insert_staged(&self, commitment: B256, entry: StagedBlock) {
        let mut q = self.staged.lock().await;
        q.retain(|(c, _)| *c != commitment);
        if q.len() >= 32 {
            q.pop_front();
        }
        q.push_back((commitment, entry));
        drop(q);
        // wake an anchor sitting in `await_anchor_grace` for exactly this
        // commitment: a staged entry carries no claim, so the staged map is
        // the only evidence it will ever see.
        self.stage_notify.notify_waiters();
    }

    /// Is `commitment` already staged against `head_c`? Used to make a second
    /// staging of the same bytes (our own `arc_buildBlock` result, then the
    /// CL's `arc_stageBlock` for it) a cheap no-op instead of a re-execution.
    pub(super) async fn already_staged(&self, commitment: B256, head_c: B256) -> bool {
        self.staged
            .lock()
            .await
            .iter()
            .any(|(c, e)| *c == commitment && e.staged_parent == head_c)
    }

    /// Drop OUR claim on `commitment` — and only ours. Returns whether the
    /// entry was removed.
    ///
    /// The id check is the whole point: a holder that stalled past the 2 s
    /// guard and was taken over must not delete the successor's claim when it
    /// eventually finishes. An unchecked `remove` there re-opens the window in
    /// which a third feed of the same bytes sees no claim, still passes the
    /// parent-link check (the head has not moved — the taker-over is still
    /// committing), and appends the same block a second time.
    pub async fn release_claim(&self, commitment: B256, id: u64) -> bool {
        use std::collections::hash_map::Entry;
        let mut fl = self.inflight.lock().await;
        match fl.entry(commitment) {
            Entry::Occupied(o) if o.get().1 == id => {
                o.remove();
                true
            }
            _ => false,
        }
    }

    /// Shim: `arc_stageBlock` — fully validate + EXECUTE against the current
    /// head state into a memory-only staged entry (vote-gap deferred anchor).
    /// Never appends, never queues (speculative). Bounded to 32 entries.
    ///
    /// CLAIM ORDER (2026-09-15, the stage/anchor race). The CL fires
    /// `arc_stageBlock` and then `arc_newBlock{commitment}` ~15 ms later,
    /// while the stage is still running (~18 ms at 2.2 MB: decode 6 + recover
    /// 5 + apply 7, plus the RPC layer's JSON parse and base64 decode of the
    /// ~3 MB body, which is *outside* that 18 ms). Whatever the anchor cannot
    /// see, it peer-pulls for 200-250 ms on the consensus critical path.
    ///
    /// So the commitment is published in `inflight` as the FIRST thing this
    /// function does — computed by `commitment_of_wire`, which hashes the
    /// borrowed wire bytes without materialising the tx list — and BEFORE the
    /// head read and the already-staged probe, both of which take locks that
    /// a concurrent commit can hold. A claim that turns out not to be ours is
    /// withdrawn microseconds later; an anchor that misses the claim pays 200
    /// ms. The claim is keyed on the block's DECLARED parent, which is what
    /// the head must equal for the stage to proceed, so the retention sweeps
    /// (`retain(|_, (p, _)| *p == new_head)`) see exactly what they saw before.
    pub async fn stage_block(&self, bytes: &[u8]) -> Result<StageOutcome, String> {
        let t0 = Instant::now();
        let hdr = LeanBlock::commitment_of_wire(bytes)?;
        let commitment_key = hdr.commitment;
        let number = hdr.number;

        // --- publish the claim before anything slow ---
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);
        let claim_id = next_claim_id();
        let claimed = {
            use std::collections::hash_map::Entry;
            let mut fl = self.inflight.lock().await;
            match fl.entry(commitment_key) {
                // someone already owns this commitment (a duplicate stage, or
                // a by-bytes commit): never clobber their signal
                Entry::Occupied(_) => None,
                Entry::Vacant(v) => {
                    v.insert((hdr.parent, claim_id, done_rx));
                    Some(claim_id)
                }
            }
        };
        if claimed.is_some() {
            self.stage_notify.notify_waiters();
        }

        let r = self.stage_claimed(bytes, hdr, t0).await;

        // --- release it in EVERY outcome: Staged, Syncing, and Err ---
        // A stage that bails on the head check (or on a decode error) must not
        // park the next anchor for the full 2 s guard. A panic drops `done_tx`
        // instead, which closes the channel and wakes waiters the same way.
        // `release_claim` is id-checked: a stage slow enough to have been taken
        // over signals its waiters but leaves the new owner's claim alone.
        if let Some(id) = claimed {
            let _ = done_tx.send(true);
            self.release_claim(commitment_key, id).await;
        }
        self.stage_notify.notify_waiters();

        let (outcome, t) = r?;
        if matches!(outcome, StageOutcome::Staged(_)) {
            tracing::debug!(
                n = number,
                total_ms = t0.elapsed().as_secs_f64() * 1e3,
                decode_ms = t.decode_ms,
                recover_ms = t.recover_ms,
                apply_ms = t.apply_ms,
                noop = t.noop,
                "lean_stage"
            );
        }
        Ok(outcome)
    }

    /// The body of `stage_block`, run with the commitment already claimed in
    /// `inflight`. Split out so that EVERY exit — including `?` on a decode
    /// error — passes through the caller's single release point.
    async fn stage_claimed(
        &self,
        bytes: &[u8],
        hdr: crate::chain::WireHeader,
        t_decode: Instant,
    ) -> Result<(StageOutcome, StageTiming), String> {
        let since = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
        let mut t = StageTiming::default();
        // PRELIMINARY head read: a cheap bail for a block that plainly does
        // not connect. NOT the one the entry is built on — see the atomic read
        // below.
        let (head_c0, head_n0, _) = *self.head.lock().await;
        if hdr.number != head_n0 + 1 || hdr.parent != head_c0 {
            return Ok((StageOutcome::Syncing { head: head_n0 }, t));
        }
        // Already staged against this head — our own `arc_buildBlock` result,
        // or a duplicate `arc_stageBlock`. A cheap no-op: the entry that is
        // there is a post-state of the same parent over the same bytes, so
        // re-executing would only pay ecrecover + an execution to rebuild it.
        if self.already_staged(hdr.commitment, head_c0).await {
            t.decode_ms = since(t_decode);
            t.noop = true;
            return Ok((StageOutcome::Staged(hdr.commitment), t));
        }
        // the expensive half of the decode (tx bodies out of the wire). The
        // commitment is NOT recomputed: `hdr` is the header `commitment_of_wire`
        // produced over THESE bytes at the top of `stage_block` — the trust
        // boundary `from_wire_bytes_with_header` documents (it re-derives and
        // compares the three header fields always, the commitment in debug
        // builds, and takes `hdr.commitment` on faith in release).
        let block = LeanBlock::from_wire_bytes_with_header(bytes, hdr)?;
        t.decode_ms = since(t_decode);
        let t_recover = Instant::now();
        let items = decode_block_txs(&block.txs);
        t.recover_ms = since(t_recover);
        // ATOMIC (head, state). A staged entry is a post-state PLUS the parent
        // it claims to descend from, and promotion trusts that pairing rather
        // than re-executing. Reading the head in one critical section and
        // cloning the state in another let a commit land in between: the entry
        // then carried the OLD head as `staged_parent` and the NEW head's
        // state as its post-state — a block executed against a parent it does
        // not descend from, kept in the staged map and servable by commitment.
        //
        // LOCK ORDER: `head` then `state`, nested here and only here. It is the
        // order every committer already uses sequentially (`commit_gate` ->
        // `link_to_head`'s head read -> `state`), and nothing anywhere takes
        // `state` before `head`, so no cycle exists. The head guard spans
        // exactly one await — acquiring `state` — and nothing else. The
        // execution then runs under `state` alone and READS the committed
        // state in place (`execute` is pure): no copy of the state is made.
        //
        // The re-check is also the authoritative linkage test: the preliminary
        // one above is only a fast bail.
        let t_apply = Instant::now();
        let (head_c, delta) = {
            let head = self.head.lock().await;
            let (head_c, head_n, _) = *head;
            if block.number != head_n + 1 || block.parent != head_c {
                return Ok((StageOutcome::Syncing { head: head_n }, t));
            }
            let state = self.state.lock().await;
            drop(head);
            (head_c, execute(&state, &items, self.cfg.beneficiary))
        };
        let commitment = block.commitment;
        let entry = StagedBlock {
            staged_parent: head_c,
            block,
            wire: bytes.to_vec(),
            delta,
        };
        t.apply_ms = since(t_apply);
        // visible BEFORE the caller signals completion: a waiter woken by that
        // signal must find the entry it was waiting for.
        self.insert_staged(commitment, entry).await;
        Ok((StageOutcome::Staged(commitment), t))
    }

    /// Wait out a concurrent `arc_stageBlock` (or by-bytes commit) of exactly
    /// this commitment, so the anchor never peer-pulls a block that is being
    /// executed in this process right now. Returns whether it waited at all;
    /// the caller re-checks the index and `locate_wire`.
    ///
    /// Two windows, both covered:
    ///
    /// 1. The stage has NAMED itself — `inflight` holds `c`. Wait on its watch
    ///    channel, generously bounded (2 s), exactly as the by-bytes claim
    ///    path does. A dropped owner (panicked stager) closes the channel and
    ///    we fall through, so a dead stage can never wedge an anchor.
    /// 2. The stage has ARRIVED but not yet named itself — it is in the RPC
    ///    layer parsing ~3 MB of JSON/base64, or hashing the wire, and does
    ///    not know its own commitment yet. `stage_pending` is non-zero exactly
    ///    then. Wait on `stage_notify` up to `STAGE_RACE_GRACE`, rechecking
    ///    `inflight` each round. This window is real: the CL issues the anchor
    ///    ~15 ms after the stage, and the stage's pre-registration work is the
    ///    same order of magnitude.
    ///
    /// The grace in (2) is gated on `stage_pending > 0`, so a node doing a
    /// genuine backfill — nobody staging — adds zero latency to its peer
    /// pulls. Worst case when a stage is pending but is for a DIFFERENT block:
    /// `STAGE_RACE_GRACE` on top of a path that already costs 200-250 ms.
    pub(super) async fn await_stage_race(&self, c: B256) -> bool {
        let grace = STAGE_RACE_GRACE;
        let t_wait = Instant::now();
        let mut waited = false;
        loop {
            let held = {
                let fl = self.inflight.lock().await;
                fl.get(&c).map(|(_, _, rx)| rx.clone())
            };
            if let Some(mut rx) = held {
                waited = true;
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while !*rx.borrow() {
                        if rx.changed().await.is_err() {
                            break; // owner dropped (failed/panicked): fall through
                        }
                    }
                })
                .await;
                break;
            }
            if self.stage_pending.load(Ordering::Relaxed) == 0 {
                break;
            }
            let left = grace.checked_sub(t_wait.elapsed()).unwrap_or_default();
            if left.is_zero() {
                break;
            }
            waited = true;
            // Bounded poll rather than a bare `notified()`: a notification
            // that fires between the `inflight` peek and this await would
            // otherwise be lost, and the cost of missing one is a 200 ms pull.
            let _ = tokio::time::timeout(
                left.min(std::time::Duration::from_millis(2)),
                self.stage_notify.notified(),
            )
            .await;
        }
        if waited {
            Metrics::inc(&self.metrics.stage_race_waits);
        }
        waited
    }

    /// THE COLD GRACE (`anchor_grace_ms`). `await_stage_race` covers a
    /// stage that is RUNNING or has ARRIVED; this covers the fleet's actual
    /// case, where the stage has done NEITHER when the anchor lands — nothing
    /// staged, nothing claimed, `stage_pending` zero — because the CL spawned
    /// it fire-and-forget and it is still encoding 2.5 MB of base64 (see
    /// `anchor_grace_ms`).
    ///
    /// Returns whether evidence of the stage appeared inside the window: a
    /// claim on `c`, a staged entry for `c`, or simply a stage ARRIVING (then
    /// `await_stage_race` takes over and waits it out properly). The caller
    /// re-resolves locally on `true` and falls through to the peer pull on
    /// `false`.
    ///
    /// THE LAGGARD. A node that is genuinely behind gets anchors for
    /// commitments no stage of its own will ever produce, and it must not pay
    /// the grace on every one of them. Two cheap reads say so — a non-empty
    /// `sync_queue` (blocks waiting on a parent we lack) or a `sync_target`
    /// above our head (the backfill is armed) — and both skip the grace
    /// entirely, exactly as `STAGE_RACE_GRACE` is skipped when no stage is
    /// pending. A node that is behind WITHOUT either flag set (its CL parked,
    /// or the first anchor after falling behind) still answers SYNCING/peer
    /// as before, just up to `anchor_grace_ms` later — 60 ms on top of a
    /// path that already costs 200-250 ms, and it re-converges by the same
    /// route it always did.
    pub(super) async fn await_anchor_grace(&self, c: B256) -> bool {
        let grace = std::time::Duration::from_millis(self.cfg.anchor_grace_ms);
        if grace.is_zero() {
            return false;
        }
        // behind the network: no stage of ours is coming for this commitment
        let head_n = self.head.lock().await.1;
        if self.sync_target.load(Ordering::Relaxed) > head_n
            || !self.sync_queue.lock().await.is_empty()
        {
            return false;
        }
        let t_wait = Instant::now();
        let hit = loop {
            // A stage that has arrived at the RPC layer is enough: hand it to
            // `await_stage_race`, whose job is precisely that window.
            if self.stage_pending.load(Ordering::Relaxed) > 0 {
                break true;
            }
            if self.inflight.lock().await.contains_key(&c) {
                break true;
            }
            if self.staged.lock().await.iter().any(|(k, _)| *k == c) {
                break true;
            }
            let left = grace.checked_sub(t_wait.elapsed()).unwrap_or_default();
            if left.is_zero() {
                break false;
            }
            // Bounded poll rather than a bare `notified()`: `stage_notify` is
            // signalled on claim insert, on staged insert and when a pending
            // stage returns, but one that fires between the checks above and
            // this await would otherwise be lost — and missing one costs a
            // 200 ms pull, which is the whole thing we are here to avoid.
            let _ = tokio::time::timeout(
                left.min(std::time::Duration::from_millis(2)),
                self.stage_notify.notified(),
            )
            .await;
        };
        Metrics::inc(if hit {
            &self.metrics.anchor_grace_hits
        } else {
            &self.metrics.anchor_grace_misses
        });
        hit
    }
}
