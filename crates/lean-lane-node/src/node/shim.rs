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
        let commitment = block.commitment;
        let number = block.number;
        let mut waited = false;
        loop {
            let (head_c, head_n, _) = *self.head.lock().await;
            if number <= head_n {
                // known-height path: compare against our chain
                if number == 0 {
                    return Err("block number 0 is the genesis sentinel".into());
                }
                let known = self
                    .log
                    .lock()
                    .await
                    .read_block(number)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("height {number} predates the log"))?;
                if known.commitment == commitment {
                    path.set(AnchorPath::Index);
                    return Ok(NewBlockOutcome::Valid(commitment));
                }
                return Err(format!(
                    "conflicting block at known height {number}: have {}, got {commitment}",
                    known.commitment
                ));
            }
            if number != head_n + 1 || block.parent != head_c {
                // Behind-the-tip machinery (Engine-API "SYNCING" analog): a
                // block that does not connect is QUEUED, never an error — the
                // node heals itself, consensus never orchestrates recovery.
                // Commitments are recomputed on ingest, so a forged queue
                // entry can never apply.
                self.queue_syncing(block).await;
                return Ok(NewBlockOutcome::Syncing { head: head_n });
            }
            // deferred-anchor fast path: staged in the vote gap for THIS head?
            if let Some(entry) = self.take_staged(commitment, head_c).await {
                debug_assert_eq!(entry.wire, bytes);
                path.refine(AnchorPath::Staged);
                self.promote_staged(entry)
                    .await
                    .map_err(|e| e.to_string())?;
                break;
            }
            // A stage (or another feed) of these bytes is executing right now:
            // wait for its result once, then look again, rather than executing
            // the same block a second time.
            if !waited && self.is_in_progress(commitment) {
                waited = true;
                self.await_arrival(commitment, std::time::Duration::ZERO)
                    .await;
                continue;
            }
            let _work = self.begin_work(commitment);
            let items = decode_block_txs(&block.txs);
            self.commit_block(block, items)
                .await
                .map_err(|e| e.to_string())?;
            break;
        }
        self.announce_to_peers(commitment, number);
        // Drain any queued children that now connect (loop: each apply may
        // unlock the next). The head check is only an early-out; the commit
        // gate decides.
        let mut parent = commitment;
        loop {
            let next = self.sync_queue.lock().await.remove(&parent);
            let Some(nb) = next else { break };
            let c = nb.commitment;
            let num = nb.number;
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

    /// Take the staged entry for `commitment` if it was staged on `head_c`.
    async fn take_staged(&self, commitment: B256, head_c: B256) -> Option<StagedBlock> {
        let mut q = self.staged.lock().await;
        let pos = q
            .iter()
            .position(|(c, e)| *c == commitment && e.staged_parent == head_c)?;
        q.remove(pos).map(|(_, e)| e)
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
        if let Some((bytes, src)) = self.locate(c).await? {
            path.set(src);
            return self.shim_new_inner(&bytes, path).await;
        }
        // Nowhere here yet. Our own stage of it is usually running or about
        // to arrive: wait for it (see `await_arrival`) before spending a
        // 200-250 ms peer round trip on the consensus critical path.
        let (found, waited) = self.await_arrival(c, self.anchor_grace().await).await;
        if waited {
            Metrics::inc(if found {
                &self.metrics.anchor_grace_hits
            } else {
                &self.metrics.anchor_grace_misses
            });
        }
        if found {
            if self.by_commitment.lock().await.contains_key(&c) {
                path.set(AnchorPath::Index);
                return Ok(NewBlockOutcome::Valid(c));
            }
            if let Some((bytes, src)) = self.locate(c).await? {
                path.set(src);
                return self.shim_new_inner(&bytes, path).await;
            }
        }
        let fetched = self
            .pull_block(peers::BlockQuery::Commitment(c), |h| h.commitment == c)
            .await;
        if let Some(bytes) = fetched {
            Metrics::inc(&self.metrics.block_pulls);
            path.set(AnchorPath::Peer);
            return self.shim_new_inner(&bytes, path).await;
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
