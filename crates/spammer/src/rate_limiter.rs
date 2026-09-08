// Copyright 2025 Circle Internet Group, Inc. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use governor::{Jitter, Quota};

/// Token-bucket rate limiter for transaction sending.
///
/// Spaces sends evenly across each second using `governor` instead of
/// resetting a counter once per second. At 1000 TPS each call to `wait()`
/// sleeps ~1ms, eliminating the burst-then-idle pattern of the old approach.
///
/// Optionally closed-loop: with a pool governor installed, `wait()` also
/// pauses while the target node's txpool depth (pending + queued, fed by a
/// background poller) exceeds `pool_target`. Open-loop over-offering past
/// what the chain consumes drives the pool to its cap, where eviction
/// nonce-gaps accounts and collapses throughput (measured: 8.1k tx/s at a
/// balanced offer vs 3.2k at 2.5x over-offer) — the governor keeps the
/// backlog bounded instead.
pub(crate) struct RateLimiter {
    limiter: governor::DefaultDirectRateLimiter,
    jitter: Jitter,
    max_num_txs: u64,
    total_counter: AtomicU64,
    /// (live pool depth gauge, pause threshold); None = open loop.
    pool_governor: Option<(Arc<AtomicU64>, u64)>,
}

impl RateLimiter {
    pub fn new(tps: u64, max_num_txs: u64, num_senders: usize) -> Self {
        let tps_u32 = u32::try_from(tps).expect("TPS must fit in u32");
        let tps_nz = NonZeroU32::new(tps_u32).expect("TPS must be > 0");
        let burst = (tps / num_senders.max(1) as u64).max(1);
        let burst_nz = NonZeroU32::new(u32::try_from(burst).expect("burst must fit in u32"))
            .expect("burst must be > 0");
        let quota = Quota::per_second(tps_nz).allow_burst(burst_nz);
        let limiter = governor::RateLimiter::direct(quota);
        // Uniformly random jitter up to half the interval
        let jitter = Jitter::up_to(quota.replenish_interval() / 2);
        Self {
            limiter,
            jitter,
            max_num_txs,
            total_counter: AtomicU64::new(0),
            pool_governor: None,
        }
    }

    /// Install the closed-loop pool governor: `wait()` pauses while
    /// `gauge` (pending + queued, maintained by a background poller)
    /// exceeds `target`.
    pub fn with_pool_governor(mut self, gauge: Arc<AtomicU64>, target: u64) -> Self {
        self.pool_governor = Some((gauge, target));
        self
    }

    /// Wait until the rate limiter permits the next send.
    /// Also checks the total transaction limit, if it is set.
    /// Returns `true` to send or `false` to stop, when the transaction limit is reached.
    pub async fn wait(&self) -> bool {
        if let Some((gauge, target)) = &self.pool_governor {
            while gauge.load(Ordering::Relaxed) > *target {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        self.limiter.until_ready_with_jitter(self.jitter).await;
        if self.max_num_txs == 0 {
            return true;
        }
        let prev = self.total_counter.fetch_add(1, Ordering::Relaxed);
        prev < self.max_num_txs
    }
}
