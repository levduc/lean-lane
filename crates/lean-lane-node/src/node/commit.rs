//! The append path: the last-moment link check, execution into a `Delta`,
//! and the ONE `append` both the direct commit and the staged promote use.

use super::*;

/// What `link_to_head` decided about a block that is about to be appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    /// `number == head + 1` and `parent == head commitment`: append it.
    Connects,
    /// Already ours at its own number, with this commitment: append nothing.
    AlreadyCanonical,
}

impl LaneNode {
    /// The last-moment linkage decision, taken under `commit_gate` and
    /// therefore in the same critical section as the append and the head write
    /// that follow it. Every caller of `commit_block`/`promote_staged` has
    /// already checked the link — that check is what lets them execute at all
    /// — but between that check and the append the head can move, and until
    /// this existed nothing looked again.
    ///
    /// Three answers, and the middle one is the point: a block that is already
    /// ours at its own number is a NO-OP, not an append. That is what makes a
    /// second committer of the same bytes (a stolen claim, a drain racing an
    /// anchor) harmless instead of a second record in `blocks.log` — the log
    /// diverges byte-for-byte from every other validator's while the state
    /// still converges, so nothing else in the node would ever notice.
    async fn link_to_head(&self, block: &LeanBlock) -> eyre::Result<Link> {
        let (head_c, head_n, _) = *self.head.lock().await;
        if block.number == head_n + 1 && block.parent == head_c {
            return Ok(Link::Connects);
        }
        if block.number > head_n {
            eyre::bail!(
                "block {} does not connect: parent {}, head {head_c} at {head_n}",
                block.number,
                block.parent,
            );
        }
        // At or below the head. The only benign case is that this exact block
        // is already canonical — someone else committed it while we executed.
        let known = if block.number == head_n {
            Some(head_c)
        } else {
            let mut log = self.log.lock().await;
            log.read_block(block.number)?.map(|b| b.commitment)
        };
        match known {
            Some(c) if c == block.commitment => Ok(Link::AlreadyCanonical),
            Some(c) => eyre::bail!(
                "conflicting block at known height {}: have {c}, got {}",
                block.number,
                block.commitment,
            ),
            None => eyre::bail!("height {} predates the log", block.number),
        }
    }

    /// Execute + append. Re-checks the parent link under `commit_gate`
    /// immediately before touching state, so it is the ONLY way the chain
    /// advances by execution *and* the place the "does this connect?"
    /// question is finally answered — no caller's earlier check is trusted.
    /// Idempotent: re-committing a block that is already canonical appends
    /// nothing and is still `Ok`, which is what the callers report as VALID.
    pub(super) async fn commit_block(
        &self,
        block: LeanBlock,
        items: Vec<(Address, LeanTx, B256)>,
    ) -> eyre::Result<()> {
        let gate = self.commit_gate.lock().await;
        if self.link_to_head(&block).await? == Link::AlreadyCanonical {
            return Ok(());
        }
        let delta = execute(&*self.state.lock().await, &items, self.cfg.beneficiary);
        self.append(&gate, &block, delta).await
    }

    /// Promote a staged block: the same gate, the same last-moment link check
    /// and the same `append` as `commit_block` — only the execution is skipped,
    /// because the entry already carries the `Delta` its staging computed.
    ///
    /// Taking the staged entry out of `staged` makes this promote unique among
    /// promotes, but not among APPENDS — a by-bytes feed that arrives after the
    /// entry is taken finds nothing staged, takes the free claim, and commits
    /// the same block. The link check under the gate answers that race.
    pub(super) async fn promote_staged(&self, entry: StagedBlock) -> eyre::Result<()> {
        let gate = self.commit_gate.lock().await;
        if self.link_to_head(&entry.block).await? == Link::AlreadyCanonical {
            return Ok(());
        }
        self.append(&gate, &entry.block, entry.delta).await?;
        Metrics::inc(&self.metrics.promoted);
        Ok(())
    }

    /// THE append: write `delta` into state and the pool's view, append the
    /// block to the log (fsync — the durability point), index it, snapshot if
    /// due, move the head, re-base the pool and sweep the staging maps. Both
    /// `commit_block` and `promote_staged` end here, so a staged block and a
    /// directly fed block are written by the same code. The `_gate` argument is
    /// proof the caller holds `commit_gate` and has passed `link_to_head`.
    async fn append(
        &self,
        _gate: &tokio::sync::MutexGuard<'_, ()>,
        block: &LeanBlock,
        mut delta: Delta,
    ) -> eyre::Result<()> {
        let mut state = self.state.lock().await;
        delta.apply_to(&mut state);
        for (a, acct) in &delta.writes {
            self.provider.upsert(*a, *acct);
        }
        self.provider.set_head(block.number);
        let snapshot = self.cfg.snapshot_due(block.number).then(|| state.clone());
        drop(state);

        self.log.lock().await.append(block)?;
        self.by_commitment
            .lock()
            .await
            .insert(block.commitment, block.number);
        if let Some(s) = snapshot {
            s.write_snapshot(
                &self.cfg.datadir,
                block.commitment,
                block.number,
                block.timestamp_ms,
            )?;
        }

        *self.head.lock().await = (block.commitment, block.number, block.timestamp_ms);
        self.pool
            .remove_transactions(std::mem::take(&mut delta.evict));
        // re-base the post-block (nonce, balance) of every written account —
        // without this the pool never learns mined nonces are consumed and a
        // sender's remaining txs demote to QUEUED forever (the fleet
        // regression: pending 0 / queued 25-31k / empty blocks). It promotes
        // now-connectable queued txs and discards stale ones (reth's
        // canonical-update analog).
        self.pool.update_accounts(
            delta
                .writes
                .iter()
                .map(|(a, acct)| reth_execution_types::ChangedAccount {
                    address: *a,
                    nonce: acct.nonce,
                    balance: U256::from(acct.balance),
                })
                .collect(),
        );
        // head advanced: keep entries that are still AHEAD of the new head
        // (losing candidates for this height are dropped when the height is
        // passed; bytes for the current height stay servable by commitment)
        let (new_head_c, new_head_n, _) = *self.head.lock().await;
        self.staged
            .lock()
            .await
            .retain(|(_, e)| e.block.number >= new_head_n);
        self.inflight
            .lock()
            .await
            .retain(|_, (p, _, _)| *p == new_head_c);
        let m = &self.metrics;
        Metrics::inc(&m.blocks);
        Metrics::add(&m.txs, delta.txs() as u64);
        Metrics::add(&m.noops, delta.outcome.noops as u64);
        Metrics::add(&m.outputs, delta.outcome.outputs as u64);
        Ok(())
    }
}
