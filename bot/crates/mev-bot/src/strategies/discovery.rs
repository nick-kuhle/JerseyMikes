//! V2 pool discovery.
//!
//! Grows the pool cache beyond the hardcoded `CORE_TOKENS` set by scanning
//! `PairCreated` on the V2 factories. Runs once per block (block cadence, not
//! per-pending-tx), so it adds nothing to the hot path. Discovery only loads
//! pools into `PoolCache`; it never emits an `Opportunity`.

use std::collections::HashSet;

use alloy_primitives::{Address, U256};
use parking_lot::RwLock;

use crate::dex;
use crate::strategies::{scan_pair_created, StrategyCtx};
use crate::types::BlockHead;

/// Minimum WETH liquidity (wei) before a discovered pool enters the cache —
/// 0.5 WETH, mirroring the sniper's dust gate. Evaluated at runtime in
/// `discover` (U256 can't be built from a literal in a `const` on this alloy
/// version).
const MIN_WETH_RESERVE: u128 = 500_000_000_000_000_000;

/// Tracks seen pairs so we never re-load or re-scan the same pool.
pub struct PoolDiscovery {
    seen: RwLock<HashSet<Address>>,
    last_log_block: RwLock<u64>,
}

impl Default for PoolDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

impl PoolDiscovery {
    pub fn new() -> Self {
        Self {
            seen: RwLock::new(HashSet::new()),
            last_log_block: RwLock::new(0),
        }
    }

    pub fn seen_count(&self) -> usize {
        self.seen.read().len()
    }

    /// Scan both V2 factories for `PairCreated` since the last scan and load
    /// qualifying pools into the shared cache. Returns the number of new pools
    /// loaded.
    pub async fn discover(&self, ctx: &StrategyCtx, head: &BlockHead) -> usize {
        let from = {
            let last = *self.last_log_block.read();
            if last == 0 {
                head.number.saturating_sub(50)
            } else if head.number <= last {
                return 0;
            } else {
                last + 1
            }
        };
        *self.last_log_block.write() = head.number;

        let weth = ctx.cfg.chain.weth;
        let min_reserve = U256::from(MIN_WETH_RESERVE); // 0.5 WETH

        let mut loaded = 0usize;
        for (venue, pair) in scan_pair_created(&ctx.rpc, from, head.number).await {
            if !self.seen.write().insert(pair) {
                continue;
            }
            // Fetch first, then insert only qualifying pools. `PoolCache::load`
            // inserts immediately, which would retain non-WETH and dust pools.
            let Ok(pool) = dex::fetch_v2_pool(&ctx.rpc, pair, venue, 30, head.number).await else {
                continue;
            };
            let Some((weth_reserve, _)) = pool.reserves_for(weth) else {
                continue;
            };
            if weth_reserve < min_reserve {
                continue;
            }
            ctx.pools.insert(pool);
            tracing::info!(
                target: "pools",
                pair = ?pool.address,
                venue = pool.venue.as_str(),
                token = ?pool.other_token(weth),
                weth = ?weth_reserve,
                "discovered new WETH-quoted pool"
            );
            loaded += 1;
        }
        loaded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seen_set_is_initially_empty() {
        let d = PoolDiscovery::new();
        assert_eq!(d.seen_count(), 0);
    }

    #[test]
    fn window_uses_fallback_on_first_scan() {
        // last_log_block starts at 0 -> discover() would use head.number-50.
        let d = PoolDiscovery::new();
        assert_eq!(*d.last_log_block.read(), 0);
        // (The RPC path itself needs a live RPC and is not unit-testable here;
        // cover the pure invariants above and in strategies/mod.rs.)
    }
}
