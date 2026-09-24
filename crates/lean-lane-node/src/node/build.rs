//! `arc_buildBlock`: pick a candidate from the pool and self-stage it.

use super::*;

impl LaneNode {
    /// Shim: `arc_buildBlock` — build a candidate on the CURRENT head against a
    /// STAGED state copy. Never appends, never evicts, never mutates committed
    /// state. Contract: parent must be the head commitment; number must be
    /// head+1 (a consistent CL can send nothing else — checked loudly).
    ///
    /// SELF-STAGES its result (2026-09-15). The proposer used to execute its
    /// own block on a throwaway copy and drop it: at decide its own
    /// `arc_newBlock{commitment}` found nothing staged and nothing queued, so
    /// it pulled ITS OWN block back over HTTP from a peer — on the consensus
    /// critical path. Measured on the 4-validator fleet: `path=peer` on
    /// 25-35% of heights (exactly the proposer's turn share), mean 150-300 ms
    /// against 8-11 ms for `path=staged`. Staging the build result costs one
    /// entry in a 32-slot map and turns that into the staged path.
    pub async fn shim_build(
        &self,
        parent: B256,
        number: u64,
        timestamp_ms: u64,
        budget_gas: u64,
    ) -> Result<(B256, Vec<u8>), String> {
        let t0 = Instant::now();
        let (head_c, head_n, _) = *self.head.lock().await;
        if parent != head_c {
            return Err(format!("stale parent: build on {parent}, head is {head_c}"));
        }
        if number != head_n + 1 {
            return Err(format!(
                "bad number {number}: head is {head_n} (want head+1)"
            ));
        }
        // pick under the CALLER's budget (may differ from cfg)
        let mut gas = 0u64;
        let mut items = Vec::new();
        let mut raw = Vec::new();
        for tx in self.pool.best_transactions() {
            let g = tx.transaction.gas_limit();
            if gas + g > budget_gas {
                break;
            }
            gas += g;
            let pooled: &ArcPooledTx = &tx.transaction;
            if let ArcTxEnvelope::Lean(lean) = pooled.0.transaction.inner() {
                items.push((
                    pooled.0.transaction.signer(),
                    lean.tx().clone(),
                    *lean.hash(),
                ));
                raw.push(lean.raw().to_vec());
            }
        }
        let block = LeanBlock::new(parent, number, timestamp_ms, raw);
        let commitment = block.commitment;
        let wire = block.to_wire_bytes();
        // Staged execution against the committed state, read in place
        // (`execute` is pure) — proves the txs apply; the commitment derives
        // from content alone and is returned either way.
        //
        // The head is re-read and the state lock taken in ONE critical section
        // (same lock order and for the same reason as `stage_inner`): `head`
        // was read at the top of this function, before the pool pick, so a
        // block landing in between would otherwise make the delta a post-state
        // of a DIFFERENT parent than the `staged_parent` recorded with it.
        let delta = {
            let head = self.head.lock().await;
            if head.0 == head_c {
                let state = self.state.lock().await;
                drop(head);
                Some(execute(&state, &items, self.cfg.beneficiary))
            } else {
                None
            }
        };
        if let Some(delta) = delta {
            if !self.already_staged(commitment, head_c).await {
                let entry = StagedBlock {
                    staged_parent: head_c,
                    block,
                    wire: wire.clone(),
                    delta,
                };
                self.insert_staged(commitment, entry).await;
            }
        }
        tracing::debug!(
            n = number,
            txs = items.len(),
            ms = t0.elapsed().as_secs_f64() * 1e3,
            "lean_build"
        );
        Ok((commitment, wire))
    }
}
