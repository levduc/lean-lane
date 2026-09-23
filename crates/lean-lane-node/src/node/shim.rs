//! The anchor verbs (`arc_newBlock` by bytes and by commitment) and the
//! serving verb (`arc_getBlockBytes`).

use super::*;

impl LaneNode {
    /// Shim: `arc_newBlock` — decode strictly, recompute the commitment,
    /// verify the parent link, total-STF execute, append+fsync, evict.
    /// IDEMPOTENT by commitment: re-feeding the head or any ancestor returns
    /// its commitment as a no-op; a DIFFERENT block at a known height is a
    /// loud error (fork attempt); head+2 gaps are errors (feed in order).
    pub async fn shim_new(&self, bytes: &[u8]) -> Result<NewBlockOutcome, String> {
        let t0 = Instant::now();
        let path = PathCell::new(AnchorPath::Bytes);
        let r = self.shim_new_inner(bytes, &path).await;
        self.finish_anchor(&r, &path, t0);
        r
    }

    /// Count the route one `arc_newBlock` took and emit its debug event.
    pub(super) fn finish_anchor(
        &self,
        r: &Result<NewBlockOutcome, String>,
        path: &PathCell,
        t0: Instant,
    ) {
        if matches!(r, Ok(NewBlockOutcome::Syncing { .. })) {
            path.set(AnchorPath::Syncing);
        }
        if r.is_ok() {
            self.metrics.record_anchor(path.get());
        }
        tracing::debug!(
            path = path.get().name(),
            ok = r.is_ok(),
            total_ms = t0.elapsed().as_secs_f64() * 1e3,
            "lean_anchor"
        );
    }

    /// The body of `arc_newBlock{blockBytes}`, labelling its route in `path`.
    pub(super) async fn shim_new_inner(
        &self,
        bytes: &[u8],
        path: &PathCell,
    ) -> Result<NewBlockOutcome, String> {
        let block = LeanBlock::from_wire_bytes(bytes)?;
        let (head_c, head_n, _) = *self.head.lock().await;
        if block.number <= head_n {
            // known-height path: compare against our chain
            if block.number == 0 {
                return Err("block number 0 is the genesis sentinel".into());
            }
            let known = self
                .log
                .lock()
                .await
                .read_block(block.number)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("height {} predates the log", block.number))?;
            if known.commitment == block.commitment {
                path.set(AnchorPath::Index);
                return Ok(NewBlockOutcome::Valid(block.commitment));
            }
            return Err(format!(
                "conflicting block at known height {}: have {}, got {}",
                block.number, known.commitment, block.commitment
            ));
        }
        if block.number != head_n + 1 || block.parent != head_c {
            // Behind-the-tip machinery (Engine-API "SYNCING" analog): a block
            // that does not connect is QUEUED, never an error — the node heals
            // itself, consensus never orchestrates recovery. Commitments are
            // recomputed on ingest, so a forged queue entry can never apply.
            self.queue_syncing(block).await;
            return Ok(NewBlockOutcome::Syncing { head: head_n });
        }
        let commitment = block.commitment;
        let number = block.number;
        // deferred-anchor fast path: staged in the vote gap for THIS head?
        let take_staged = || async {
            let mut q = self.staged.lock().await;
            let pos = q
                .iter()
                .position(|(c, e)| *c == commitment && e.staged_parent == head_c);
            pos.map(|i| q.remove(i).unwrap().1)
        };
        let mut staged = take_staged().await;
        // v1.3.1 + 2026-09-15: the COMMIT of a block is now an exclusive claim
        // in `inflight`, taken get-or-insert under ONE guard, so two feeds of
        // the same bytes (the backfill's pull and the CL's anchor racing for
        // the same height) cannot both pass the parent-link check and append
        // it twice. Whoever loses the claim waits for the winner's result —
        // a bounded <=35 ms wait beats a ~230 ms redo — and then answers from
        // whatever the winner produced (a staged entry to promote, or a
        // canonical block to acknowledge).
        let mut owner: Option<(u64, tokio::sync::watch::Sender<bool>)> = None;
        if staged.is_none() {
            // Two rounds: wait for whoever holds the claim, then TAKE IT OVER
            // if it is still held. A holder that dies without signalling (a
            // panicked stager — `dead_inflight_staging_never_wedges_newblock`)
            // or one wedged past the guard must never cost us a block we can
            // commit: the second round reproduces exactly the pre-claim
            // behaviour of falling through to the full path.
            //
            // The takeover is ID-CHECKED: we only steal the claim we have
            // already waited out. A claim that appeared WHILE we waited belongs
            // to a holder that is live and may be committing right now, and
            // stealing that one puts two committers on the same block —
            // two identical records in `blocks.log`. In that case we fall out
            // of the loop with no claim and answer SYNCING, which is safe: the
            // CL retries and the backfill re-delivers.
            const CLAIM_ROUNDS: usize = 2;
            let mut waited_on: Option<u64> = None;
            for round in 0..CLAIM_ROUNDS {
                let (done_tx, done_rx) = tokio::sync::watch::channel(false);
                let my_id = next_claim_id();
                let waiter = {
                    let mut fl = self.inflight.lock().await;
                    match fl.get(&commitment) {
                        Some((_, id, rx)) if round + 1 < CLAIM_ROUNDS || waited_on != Some(*id) => {
                            waited_on = Some(*id);
                            Some(rx.clone())
                        }
                        _ => {
                            fl.insert(commitment, (head_c, my_id, done_rx));
                            None
                        }
                    }
                };
                let Some(mut rx) = waiter else {
                    owner = Some((my_id, done_tx));
                    break;
                };
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while !*rx.borrow() {
                        if rx.changed().await.is_err() {
                            break; // owner dropped (failed/panicked): fall through
                        }
                    }
                })
                .await;
                // a STAGING finished: promote its entry (the v1.3.1 case)
                staged = take_staged().await;
                if staged.is_some() {
                    break;
                }
                // a COMMIT finished: the block is ours already, anchoring it is
                // a no-op. Answer VALID instead of appending it a second time.
                let (head_c2, head_n2, _) = *self.head.lock().await;
                if head_c2 == commitment
                    || self.by_commitment.lock().await.get(&commitment) == Some(&number)
                {
                    path.refine(AnchorPath::Index);
                    return Ok(NewBlockOutcome::Valid(commitment));
                }
                if number != head_n2 + 1 || block.parent != head_c2 {
                    // head moved elsewhere while we waited: this no longer
                    // connects, so queue it rather than commit off-chain
                    self.queue_syncing(block).await;
                    return Ok(NewBlockOutcome::Syncing { head: head_n2 });
                }
                // still committable and the claim is free again: take it
            }
        }
        match staged {
            Some(entry) => {
                debug_assert_eq!(entry.wire, bytes);
                path.refine(AnchorPath::Staged);
                if let Some((id, done_tx)) = owner.take() {
                    // never happens (a claim is only taken when nothing is
                    // staged), but leaving a claim behind would park the next
                    // feed of these bytes for the full 2 s guard
                    let _ = done_tx.send(true);
                    self.release_claim(commitment, id).await;
                }
                self.promote_staged(entry)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            None => {
                let Some((my_id, done_tx)) = owner else {
                    // Reachable BY DESIGN since the takeover became id-checked
                    // (see the claim rounds above): a claim that appeared while
                    // we waited belongs to a live holder, so we leave with none
                    // and refuse. Safe — the CL retries and the backfill
                    // re-delivers.
                    let head_n = self.head.lock().await.1;
                    return Ok(NewBlockOutcome::Syncing { head: head_n });
                };
                let items = decode_block_txs(&block.txs);
                let r = self.commit_block(block, items).await;
                // release the claim in BOTH outcomes: `commit_block`'s own
                // retention sweep already drops the entry (its parent is the
                // old head), but a failed commit never reaches that sweep.
                let _ = done_tx.send(true);
                self.release_claim(commitment, my_id).await;
                r.map_err(|e| e.to_string())?;
            }
        }
        self.announce_to_peers(commitment, number);
        // Drain any queued children that now connect (loop: each apply may
        // unlock the next).
        let mut parent = commitment;
        loop {
            let next = self.sync_queue.lock().await.remove(&parent);
            let Some(nb) = next else { break };
            let c = nb.commitment;
            let num = nb.number;
            // Cheap early-out: the head may have moved past this child via
            // backfill while we committed its parent. This check is advisory
            // only — the drain holds NO claim, so an anchor or a backfill for
            // the same block can take the (free) claim in the window between
            // here and the append. `commit_block` re-takes the check under
            // `commit_gate` and answers that race with a no-op.
            let (hc, hn, _) = *self.head.lock().await;
            if nb.number != hn + 1 || nb.parent != hc {
                break;
            }
            let items = decode_block_txs(&nb.txs);
            self.commit_block(nb, items)
                .await
                .map_err(|e| e.to_string())?;
            self.announce_to_peers(c, num);
            parent = c;
        }
        Ok(NewBlockOutcome::Valid(commitment))
    }

    /// Shim: `arc_newBlock{commitment}` — the CL only knows the commitment
    /// (it is in the EVM header). Idempotent for canonical blocks; otherwise
    /// apply from our staged/queued copy, else fetch the bytes from a peer by
    /// commitment, else SYNCING.
    pub async fn shim_new_by_commitment(&self, c: B256) -> Result<NewBlockOutcome, String> {
        let t0 = Instant::now();
        let path = PathCell::default();
        let r = self.new_by_commitment_inner(c, &path).await;
        self.finish_anchor(&r, &path, t0);
        r
    }

    /// Re-resolve `c` from LOCAL sources only: wait out any in-flight stage of
    /// it, re-read the commitment index, then `locate_wire` (staged -> sync
    /// queue -> index). `Ok(None)` means the block is still
    /// nowhere in this process and the caller may spend a peer round trip.
    ///
    /// `always_locate` forces the `locate_wire` even when `await_stage_race`
    /// waited on nothing. The hot path leaves it off on its second attempt —
    /// no wait means nothing can have changed since the first locate.
    /// The cold-grace caller turns it on, because its evidence may be a STAGED
    /// ENTRY, which carries no claim for `await_stage_race` to see.
    pub(super) async fn resolve_local(
        &self,
        c: B256,
        path: &PathCell,
        always_locate: bool,
    ) -> Result<Option<NewBlockOutcome>, String> {
        let waited = self.await_stage_race(c).await;
        if waited && self.by_commitment.lock().await.contains_key(&c) {
            path.set(AnchorPath::Index);
            return Ok(Some(NewBlockOutcome::Valid(c)));
        }
        if !waited && !always_locate {
            return Ok(None);
        }
        if let Some((bytes, src)) = self.locate(c).await? {
            path.set(src);
            return self.shim_new_inner(&bytes, path).await.map(Some);
        }
        Ok(None)
    }

    pub(super) async fn new_by_commitment_inner(
        &self,
        c: B256,
        path: &PathCell,
    ) -> Result<NewBlockOutcome, String> {
        // Idempotency check against the commitment index, which is complete
        // for every canonical block. The block being decided is by definition
        // not canonical yet, so a miss here is the normal case.
        if self.by_commitment.lock().await.contains_key(&c) {
            path.set(AnchorPath::Index);
            return Ok(NewBlockOutcome::Valid(c));
        }
        // A staging or a by-bytes COMMIT of exactly this block may be in
        // flight (our CL's own `arc_stageBlock`, fired ~15 ms ago and still
        // running; or the gossip backfill racing this anchor for the height).
        // Wait for that result instead of queueing our own `state` and `log`
        // locks behind it: if it was a commit, the block is canonical and this
        // anchor is an idempotent VALID; if it was a staging, `locate_wire`
        // finds the staged entry instead of falling through to a peer fetch.
        if let Some(o) = self.resolve_local(c, path, true).await? {
            return Ok(o);
        }
        // LAST CHANCE before spending 200-250 ms on a peer: a stage may have
        // registered while we ran `locate_wire`. Same bounded wait, then one
        // more locate — still cheaper than the round trip it replaces.
        if let Some(o) = self.resolve_local(c, path, false).await? {
            return Ok(o);
        }
        // ...and the fleet's real case: the stage has not ARRIVED yet, so
        // neither wait above had anything to wait on. Sit out the cold grace
        // and re-resolve once if it pays off (see `await_anchor_grace`).
        if self.await_anchor_grace(c).await {
            if let Some(o) = self.resolve_local(c, path, true).await? {
                return Ok(o);
            }
        }
        for cl in &self.peer_clients {
            use jsonrpsee::core::client::ClientT;
            let mut params = jsonrpsee::core::params::ObjectParams::new();
            let _ = params.insert("commitment", format!("{c}"));
            let r: Result<serde_json::Value, _> = cl.request("arc_getBlockBytes", params).await;
            let Ok(v) = r else { continue };
            let Some(b64) = v.get("blockBytes").and_then(|x| x.as_str()) else {
                continue;
            };
            let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
            else {
                continue;
            };
            match LeanBlock::from_wire_bytes(&bytes) {
                Ok(b) if b.commitment == c => {
                    Metrics::inc(&self.metrics.block_pulls);
                    path.set(AnchorPath::Peer);
                    return self.shim_new_inner(&bytes, path).await;
                }
                _ => continue, // peer served the wrong block: ignore it
            }
        }
        // every peer missed or failed
        let head_n = self.head.lock().await.1;
        Ok(NewBlockOutcome::Syncing { head: head_n })
    }

    /// Shim: `arc_getBlockBytes` — serve a block's consensus wire bytes from
    /// the log (sync path).
    pub async fn block_wire_bytes(&self, number: u64) -> Result<Option<Vec<u8>>, String> {
        // Read under the log mutex, SERIALISE outside it. `to_wire_bytes`
        // copies the whole block (2.5 MB on the fleet) and the log mutex is
        // exactly what the decide anchor's append needs — as one temporary in
        // the return expression the guard lived across the copy, so every
        // peer/CL `arc_getBlockBytes` stalled the next height's append by a
        // memcpy it had no reason to wait for.
        let block = {
            self.log
                .lock()
                .await
                .read_block(number)
                .map_err(|e| e.to_string())?
        };
        Ok(block.map(|b| b.to_wire_bytes()))
    }

    /// Resolve a commitment to block bytes: staged, then the sync queue, then
    /// the commitment index (complete from height 1, so a miss there is final).
    /// Returns the bytes and WHERE they came from.
    pub(super) async fn locate(&self, c: B256) -> Result<Option<(Vec<u8>, AnchorPath)>, String> {
        let t0 = Instant::now();
        let r = self.locate_inner(c).await;
        tracing::debug!(
            hit = match &r {
                Ok(Some((_, p))) => p.name(),
                Ok(None) => "miss",
                Err(_) => "error",
            },
            ms = t0.elapsed().as_secs_f64() * 1e3,
            "lean_locate"
        );
        r
    }

    pub(super) async fn locate_inner(
        &self,
        c: B256,
    ) -> Result<Option<(Vec<u8>, AnchorPath)>, String> {
        // Staged and queued copies are exactly what the decide anchor asks for;
        // check them before anything that can touch the log.
        if let Some((_, e)) = self.staged.lock().await.iter().find(|(k, _)| *k == c) {
            return Ok(Some((e.wire.clone(), AnchorPath::Staged)));
        }
        if let Some(b) = self
            .sync_queue
            .lock()
            .await
            .values()
            .find(|b| b.commitment == c)
        {
            return Ok(Some((b.to_wire_bytes(), AnchorPath::Queued)));
        }
        // Drop the index guard BEFORE reading the log: `block_wire_bytes`
        // awaits the log lock, and holding the commitment index across that
        // await serialises every other lookup behind one disk read.
        let hit = { self.by_commitment.lock().await.get(&c).copied() };
        match hit {
            Some(n) => Ok(self
                .block_wire_bytes(n)
                .await?
                .map(|w| (w, AnchorPath::Index))),
            None => Ok(None),
        }
    }

    /// Shim: `arc_getBlockBytes{commitment}` — canonical, staged or queued
    /// block with that commitment.
    pub async fn block_wire_bytes_by_commitment(&self, c: B256) -> Result<Option<Vec<u8>>, String> {
        Ok(self.locate(c).await?.map(|(w, _)| w))
    }
}
