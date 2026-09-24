//! Node-to-node gossip: announce-on-append, the SYNCING queue and the
//! backfill that pulls missing heights from peers.

use super::*;

impl LaneNode {
    /// Queue a block that does not connect to our head, keyed by its PARENT
    /// (bounded at 512 — beyond that it is dropped and a later announce or
    /// backfill re-delivers it), and wake the backfill.
    pub(super) async fn queue_syncing(&self, block: LeanBlock) {
        {
            let mut q = self.sync_queue.lock().await;
            if q.len() < 512 {
                q.insert(block.parent, block);
            }
        }
        self.sync_notify.notify_one();
    }

    /// Fire-and-forget gossip: after appending a block, ANNOUNCE it
    /// ({commitment, number}, ~100 bytes) — never the block bytes. On a
    /// healthy chain every validator's CL anchors every block into its own
    /// node, so full-block pushes were 100% redundant link saturation
    /// (measured: wifi validator moved 11.2MB per 1.65MB block under v1.1
    /// push). Receivers pull only what they don't have. Sent to every peer,
    /// or to a random `--announce-fanout` subset per block (`announce_targets`).
    pub(super) fn announce_to_peers(&self, commitment: B256, number: u64) {
        for c in self.announce_targets() {
            tokio::spawn(async move {
                use jsonrpsee::core::client::ClientT;
                let mut params = jsonrpsee::core::params::ObjectParams::new();
                let _ = params.insert("commitment", format!("{commitment}"));
                let _ = params.insert("number", number);
                let _ = c
                    .request::<serde_json::Value, _>("arc_announceBlock", params)
                    .await;
            });
        }
    }

    /// Do we already hold the announced block? Three ways, all hash lookups:
    /// the exact commitment is STAGED (we validated and executed it for
    /// consensus in the vote gap), a stage/commit of it is IN PROGRESS, or —
    /// for the head+1 case — we hold *some* staged candidate at that height
    /// (the announcer may be naming a different candidate for the height our
    /// CL is about to anchor; either way the anchor, not a pull, settles it).
    ///
    /// No log read, no commitment resolve.
    pub(super) async fn holds_announced(&self, commitment: B256, number: u64, head_n: u64) -> bool {
        {
            let q = self.staged.lock().await;
            if q.iter().any(|(c, e)| {
                *c == commitment || (number == head_n + 1 && e.block.number == number)
            }) {
                return true;
            }
        }
        self.is_in_progress(commitment)
    }

    /// Handle a peer's announcement: known -> ignore (the universal in-sync
    /// case, zero further traffic); ALREADY HELD (staged/in progress) -> ignore,
    /// our own CL anchors it in milliseconds; head+1 and not held -> wait out
    /// `announce_grace_ms` for that anchor, then arm the backfill if it
    /// never came; further ahead -> arm the backfill at once (existing
    /// machinery — this is the genuine laggard).
    ///
    /// WHY the grace (2026-09-15). A validator stages every height for the
    /// vote gap, so when a peer's announcement for head+1 lands it already has
    /// the block, fully executed, in memory — and the CL's `arc_newBlock`
    /// anchor for it is milliseconds behind. Arming the backfill there pulled
    /// 2.5 MB per announcing peer per height and ran a SECOND commit of the
    /// same block (execution + provider upserts under the `state` lock)
    /// that the anchor then had to wait behind: measured on the 4-validator
    /// fleet as ~1.5 `lean_anchor` lines per height, duplicate on 617 of 781
    /// heights, `pre_ms` p90 226-443 ms and max 1.5 s. The self-healing
    /// property is untouched: a node whose CL is parked holds nothing staged,
    /// so it arms after the grace, and a node more than one height behind arms
    /// immediately.
    ///
    /// This path never touches the log: it decides on numbers plus two
    /// in-memory hash lookups — a block above our head cannot be in our log,
    /// and one at or below it is never pulled backwards.
    pub async fn on_announce(&self, commitment: B256, number: u64) {
        Metrics::inc(&self.metrics.announces_rx);
        let head_n = self.head.lock().await.1;
        if number <= head_n {
            // known height: our chain already has a block there (commitment
            // equality is irrelevant to traffic — never pull backwards)
            Metrics::inc(&self.metrics.announces_known);
            return;
        }
        if self.holds_announced(commitment, number, head_n).await {
            Metrics::inc(&self.metrics.announce_skipped_staged);
            return;
        }
        if number == head_n + 1 {
            // One height behind and nothing staged for it. Either our CL is
            // about to anchor it (the common case: the announcement simply
            // beat our own `arc_stageBlock`/`arc_newBlock`) or our CL is gone.
            // Give the anchor the grace window before spending a pull.
            let grace = self.cfg.announce_grace_ms;
            if grace > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(grace)).await;
                let head_n = self.head.lock().await.1;
                if number <= head_n {
                    Metrics::inc(&self.metrics.announces_known);
                    return;
                }
                if self.holds_announced(commitment, number, head_n).await {
                    Metrics::inc(&self.metrics.announce_skipped_staged);
                    return;
                }
            }
        }
        // genuinely behind: raise the target and let the backfill pull the gap
        // (for head+1 the backfill's first fetch IS the direct pull-from-peers;
        // the announcer is in the peer set and provably serves it)
        self.sync_target.fetch_max(number, Ordering::Relaxed);
        self.sync_notify.notify_one();
    }

    /// Background backfill: when a SYNCING entry exists, fetch gap blocks
    /// (head+1 upward) from peers — shuffled and raced, first valid answer
    /// wins (`pull_block`) — and apply in order; queued blocks drain via
    /// the shim_new apply path. Gives up after ~10s without progress; retried
    /// on the next trigger.
    pub fn spawn_sync(self: &std::sync::Arc<Self>) {
        let node = self.clone();
        tokio::spawn(async move {
            loop {
                node.sync_notify.notified().await;
                let mut last_progress = std::time::Instant::now();
                loop {
                    let behind_target =
                        node.head.lock().await.1 < node.sync_target.load(Ordering::Relaxed);
                    if node.sync_queue.lock().await.is_empty() && !behind_target {
                        break;
                    }
                    if last_progress.elapsed() > std::time::Duration::from_secs(10) {
                        break; // give up; next SYNCING trigger retries
                    }
                    let (head_c, head_n, _) = *node.head.lock().await;
                    let want = head_n + 1;
                    let fetched = node
                        .pull_block(peers::BlockQuery::Number(want), |h| {
                            h.number == want && h.parent == head_c
                        })
                        .await;
                    let mut got = false;
                    if let Some(bytes) = fetched {
                        Metrics::inc(&node.metrics.block_pulls);
                        Metrics::inc(&node.metrics.backfill_pulls);
                        let path = PathCell::default();
                        got = matches!(
                            node.shim_new_inner(&bytes, &path).await,
                            Ok(NewBlockOutcome::Valid(_))
                        );
                    }
                    if got {
                        last_progress = std::time::Instant::now();
                        continue; // progress: immediately try the next number
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        });
    }
}
