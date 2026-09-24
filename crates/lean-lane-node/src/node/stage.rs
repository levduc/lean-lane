//! Vote-gap staging (`arc_stageBlock`), the staged map, the in-progress set
//! and the one arrival wait that lets an anchor wait for a local stage
//! instead of pulling the block from a peer.

use super::*;

/// Upper bound on waiting for a stage or commit that is already RUNNING here.
/// A stage takes ~20 ms at 2.5 MB; the cap only matters if one wedges.
const IN_PROGRESS_CAP: std::time::Duration = std::time::Duration::from_secs(2);

/// One entry in `LaneNode::in_progress`, removed (and waiters woken) on drop,
/// so every exit of the work it covers, `?` and panics included, clears it.
pub(super) struct InProgress<'a> {
    node: &'a LaneNode,
    commitment: B256,
}

impl Drop for InProgress<'_> {
    fn drop(&mut self) {
        {
            let mut m = self.node.in_progress_map();
            if let Some(n) = m.get_mut(&self.commitment) {
                *n -= 1;
                if *n == 0 {
                    m.remove(&self.commitment);
                }
            }
        }
        self.node.stage_notify.notify_waiters();
    }
}

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
        // wake an anchor waiting in `await_arrival` for this commitment
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

    fn in_progress_map(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<B256, u32>> {
        self.in_progress.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mark `commitment` as being worked on until the guard drops.
    pub(super) fn begin_work(&self, commitment: B256) -> InProgress<'_> {
        *self.in_progress_map().entry(commitment).or_insert(0) += 1;
        self.stage_notify.notify_waiters();
        InProgress {
            node: self,
            commitment,
        }
    }

    /// Is a stage or by-bytes commit of `commitment` running right now?
    pub fn is_in_progress(&self, commitment: B256) -> bool {
        self.in_progress_map().contains_key(&commitment)
    }

    /// Shim: `arc_stageBlock` — fully validate + EXECUTE against the current
    /// head state into a memory-only staged entry (vote-gap deferred anchor).
    /// Never appends, never queues (speculative). Bounded to 32 entries.
    ///
    /// The CL fires `arc_stageBlock` and then `arc_newBlock{commitment}` for
    /// the same block ~15 ms later, often while the stage is still running.
    /// So the commitment goes into `in_progress` first thing, computed by
    /// `commitment_of_wire` (hashes the borrowed wire, no tx list), and an
    /// anchor that arrives meanwhile waits for the staged entry instead of
    /// pulling the block from a peer.
    pub async fn stage_block(&self, bytes: &[u8]) -> Result<StageOutcome, String> {
        let t0 = Instant::now();
        let hdr = LeanBlock::commitment_of_wire(bytes)?;
        let number = hdr.number;
        let (outcome, t) = {
            let _work = self.begin_work(hdr.commitment);
            self.stage_inner(bytes, hdr, t0).await?
        };
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

    /// The body of `stage_block`, run with the commitment marked in progress.
    async fn stage_inner(
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
        // produced over THESE bytes at the top of `stage_block`, the trust
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
        // visible BEFORE the in-progress mark drops: a waiter woken by that
        // must find the entry it was waiting for.
        self.insert_staged(commitment, entry).await;
        Ok((StageOutcome::Staged(commitment), t))
    }

    /// Is `c` resolvable here without a peer: canonical, or staged?
    async fn held_locally(&self, c: B256) -> bool {
        self.by_commitment.lock().await.contains_key(&c)
            || self.staged.lock().await.iter().any(|(k, _)| *k == c)
    }

    /// THE ARRIVAL WAIT. Called when `c` is neither canonical nor staged:
    /// wait on `stage_notify` until it is, or until the deadline. Returns
    /// `(found, waited)`.
    ///
    /// The deadline is `grace` from the call while nothing is working on `c`
    /// (the fleet's case: the anchor is a tiny request, the stage a 2.5 MB
    /// one the CL spawned first, and the small one lands first), and
    /// `IN_PROGRESS_CAP` from the call while a stage or commit of `c` runs
    /// here. Every change it can be waiting for (a mark entering or leaving
    /// `in_progress`, a staged insert) fires `stage_notify`; the `Notified`
    /// is enabled before the checks, so none is missed.
    pub(super) async fn await_arrival(&self, c: B256, grace: std::time::Duration) -> (bool, bool) {
        let start = Instant::now();
        let mut waited = false;
        loop {
            let notified = self.stage_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.held_locally(c).await {
                return (true, waited);
            }
            let window = if self.is_in_progress(c) {
                IN_PROGRESS_CAP
            } else {
                grace
            };
            let Some(left) = window.checked_sub(start.elapsed()).filter(|d| !d.is_zero()) else {
                return (false, waited);
            };
            waited = true;
            let _ = tokio::time::timeout(left, notified).await;
        }
    }

    /// The grace an anchor gives a stage that has not arrived:
    /// `anchor_grace_ms`, or zero on a node that is behind the network (a
    /// non-empty `sync_queue`, or a backfill target above the head). A
    /// laggard gets anchors for blocks no stage of its own will produce and
    /// must not pay the grace on each of them.
    pub(super) async fn anchor_grace(&self) -> std::time::Duration {
        let head_n = self.head.lock().await.1;
        if self.sync_target.load(Ordering::Relaxed) > head_n
            || !self.sync_queue.lock().await.is_empty()
        {
            return std::time::Duration::ZERO;
        }
        std::time::Duration::from_millis(self.cfg.anchor_grace_ms)
    }
}
