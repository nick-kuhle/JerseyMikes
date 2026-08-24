//! Pool discovery.
//!
//! Grows the pool caches beyond the hardcoded `CORE_TOKENS` set by scanning
//! factory creation events: `PairCreated` on the V2 factories and (behind a
//! separate toggle) `PoolCreated` on the UniswapV3 factory. Runs once per block
//! on the block task, so it adds nothing to the pending-tx hot path. Discovery
//! only loads pools into the caches; it never emits an `Opportunity`.
//!
//! Three invariants hold everything together, and all three exist because
//! getting them wrong is silent:
//!
//! 1. **A pool is only remembered once it has been successfully read.** RPC
//!    failures are transient — during provider rate limiting or reorg recovery
//!    they are the common case — so a failed fetch must leave the pair
//!    eligible for a retry on the next block.
//! 2. **The scan cursor only advances over ranges that were actually
//!    scanned.** `eth_getLogs` returning an error is not the same as it
//!    returning zero logs, and advancing past a failed range loses those pools
//!    permanently.
//! 3. **V2 and V3 pools never share a cache.** See [`V3PoolCache`].

use std::collections::{HashMap, HashSet};

use alloy_primitives::{Address, U256};
use alloy_sol_types::{sol, SolCall};
use async_trait::async_trait;

sol! {
    interface IUniswapV3FactorySeed {
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address pool);
    }
}
use parking_lot::RwLock;

use crate::dex::{self, AeroPool, V2Pool, V3Pool, Venue};
use crate::rpc::RpcClient;
use crate::strategies::{
    try_scan_aero_pool_created, try_scan_pair_created, try_scan_pool_created, AeroPoolCache,
    AeroPoolSeed, PoolCache, StrategyCtx, V3PoolCache,
};
use crate::types::BlockHead;

/// Minimum WETH liquidity (wei) before a discovered pool enters the cache —
/// 0.5 WETH, mirroring the sniper's dust gate. Evaluated at runtime in
/// `discover` (U256 can't be built from a literal in a `const` on this alloy
/// version).
const MIN_WETH_RESERVE: u128 = 500_000_000_000_000_000;

/// How far back the very first scan of a run reaches.
const FIRST_SCAN_LOOKBACK: u64 = 50;

/// Blocks of overlap re-scanned on every pass. A reorg rewinds the head, and a
/// cursor that only moves forward would step over the re-org'd range and never
/// see the pools created in it. Duplicate logs are idempotent (the caches are
/// keyed by address), missed logs are not — so the overlap is the cheap side
/// of the trade.
const REORG_DEPTH: u64 = 12;

/// Hard cap on the span of a single `eth_getLogs`. A cold start with a stale
/// cursor, or a long provider outage, would otherwise request thousands of
/// blocks in one call and be rejected outright. Oversized backlogs are caught
/// up over successive blocks instead.
const MAX_LOG_SPAN: u64 = 500;

/// A pair that was read successfully but failed the liquidity gate is re-checked
/// this often. Liquidity grows; the token pair never changes, so non-WETH pairs
/// are dropped permanently instead.
const DUST_RECHECK_BLOCKS: u64 = 50;

/// The chain reads discovery needs, behind a trait so the logic can be tested
/// without a network. `None` from any method means *the call failed*, which is
/// treated very differently from an empty result.
#[async_trait]
pub trait DiscoverySource: Send + Sync {
    async fn scan_pairs(&self, from: u64, to: u64) -> Option<Vec<(Venue, Address)>>;
    async fn fetch_pool(&self, pair: Address, venue: Venue, block: u64) -> Option<V2Pool>;
    async fn scan_v3_pools(&self, from: u64, to: u64) -> Option<Vec<V3Pool>>;
    /// Aerodrome creation-log scan. Default is "no lane on this chain":
    /// an *empty success*, which is honest for a source that genuinely has
    /// nothing to scan — callers only run the lane when the registry has an
    /// Aerodrome factory, so this default never advances a real cursor.
    async fn scan_aero_pools(&self, _from: u64, _to: u64) -> Option<Vec<AeroPoolSeed>> {
        Some(Vec::new())
    }
    /// Full Aerodrome pool read (tokens, reserves, per-pool fee, on-chain
    /// `stable()`). `None` means the call failed — retryable.
    async fn fetch_aero(&self, _pool: Address, _block: u64) -> Option<AeroPool> {
        None
    }
}

/// The production source: plain JSON-RPC against the chain's registered
/// factories (from the address registry — a chain without a V3 or Aerodrome
/// factory simply scans nothing on that lane).
pub struct RpcSource<'a> {
    pub rpc: &'a RpcClient,
    pub pair_factories: &'a [(Venue, Address)],
    pub v3_factory: Option<Address>,
    pub aero_factory: Option<Address>,
}

#[async_trait]
impl DiscoverySource for RpcSource<'_> {
    async fn scan_pairs(&self, from: u64, to: u64) -> Option<Vec<(Venue, Address)>> {
        if self.pair_factories.is_empty() {
            return Some(Vec::new());
        }
        try_scan_pair_created(self.rpc, self.pair_factories, from, to).await
    }

    async fn fetch_pool(&self, pair: Address, venue: Venue, block: u64) -> Option<V2Pool> {
        dex::fetch_v2_pool(self.rpc, pair, venue, 30, block)
            .await
            .ok()
    }

    async fn scan_v3_pools(&self, from: u64, to: u64) -> Option<Vec<V3Pool>> {
        let factory = self.v3_factory?;
        try_scan_pool_created(self.rpc, factory, from, to).await
    }

    async fn scan_aero_pools(&self, from: u64, to: u64) -> Option<Vec<AeroPoolSeed>> {
        let factory = self.aero_factory?;
        try_scan_aero_pool_created(self.rpc, factory, from, to).await
    }

    async fn fetch_aero(&self, pool: Address, block: u64) -> Option<AeroPool> {
        let factory = self.aero_factory?;
        dex::fetch_aero_pool(self.rpc, factory, pool, block).await.ok()
    }
}

/// Outcome of one discovery pass, returned for logging and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiscoveryStats {
    /// Pools added to a cache this pass.
    pub loaded: usize,
    /// Pairs whose read failed and will be retried.
    pub retryable: usize,
    /// Blocks covered by this pass (0 when the scan itself failed).
    pub scanned_blocks: u64,
}

/// Tracks what has already been seen so pools are neither re-loaded nor
/// permanently lost.
pub struct PoolDiscovery {
    /// Pairs that made it into the cache. Never re-read.
    accepted: RwLock<HashSet<Address>>,
    /// Pairs that were read fine but hold neither side in WETH. The token pair
    /// of a V2 pair is immutable, so this is a permanent rejection.
    non_weth: RwLock<HashSet<Address>>,
    /// Pairs that were read fine but were below the dust gate, with the block
    /// they were last checked at. Liquidity can arrive later, so these are
    /// re-checked periodically rather than dropped or hammered every block.
    dust: RwLock<HashMap<Address, u64>>,
    /// V3 pools already in the V3 cache or permanently rejected.
    seen_v3: RwLock<HashSet<Address>>,
    /// Aerodrome pools in the Aero cache — same acceptance bookkeeping as
    /// the V2 lane, kept in separate sets because the lanes are separate
    /// caches (accepting a pool into the wrong cache prices it wrong).
    seen_aero: RwLock<HashSet<Address>>,
    non_weth_aero: RwLock<HashSet<Address>>,
    dust_aero: RwLock<HashMap<Address, u64>>,
    last_log_block: RwLock<u64>,
    last_log_block_v3: RwLock<u64>,
    last_log_block_aero: RwLock<u64>,
}

impl Default for PoolDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

impl PoolDiscovery {
    pub fn new() -> Self {
        Self {
            accepted: RwLock::new(HashSet::new()),
            non_weth: RwLock::new(HashSet::new()),
            dust: RwLock::new(HashMap::new()),
            seen_v3: RwLock::new(HashSet::new()),
            seen_aero: RwLock::new(HashSet::new()),
            non_weth_aero: RwLock::new(HashSet::new()),
            dust_aero: RwLock::new(HashMap::new()),
            last_log_block: RwLock::new(0),
            last_log_block_v3: RwLock::new(0),
            last_log_block_aero: RwLock::new(0),
        }
    }

    /// Pairs currently held in the V2 cache by discovery.
    pub fn seen_count(&self) -> usize {
        self.accepted.read().len()
    }

    pub fn seen_v3_count(&self) -> usize {
        self.seen_v3.read().len()
    }

    /// Seed established core V3 pools directly from the factory. Creation-log
    /// scanning intentionally starts near the head, so without this pass the
    /// years-old WETH/USDC pool never enters a fresh process's V3 cache.
    pub async fn seed_core_v3(&self, ctx: &StrategyCtx) -> usize {
        let mut loaded = 0usize;
        let Some(v3_factory) = ctx.cfg.addresses.univ3_factory else {
            // No V3 factory on this chain: nothing to seed.
            return 0;
        };
        for token in ctx.cfg.addresses.core_tokens() {
            for (fee, tick_spacing) in [(500u32, 10i32), (3_000, 60), (10_000, 200)] {
                let call = IUniswapV3FactorySeed::getPoolCall {
                    tokenA: ctx.cfg.chain.weth,
                    tokenB: token,
                    fee: alloy_primitives::aliases::U24::from(fee),
                }
                .abi_encode();
                let Ok(value) = ctx
                    .rpc
                    .call_raw(
                        "eth_call",
                        serde_json::json!([{
                            "to": format!("{v3_factory:?}"),
                            "data": format!("0x{}", hex::encode(call))
                        }, "latest"]),
                    )
                    .await
                else {
                    continue;
                };
                let raw = crate::types::parse_bytes(&value);
                if raw.len() < 32 {
                    continue;
                }
                let pool = Address::from_slice(&raw[12..32]);
                if pool == Address::ZERO || ctx.pools_v3.contains(pool) {
                    continue;
                }
                let (token0, token1) = if ctx.cfg.chain.weth < token {
                    (ctx.cfg.chain.weth, token)
                } else {
                    (token, ctx.cfg.chain.weth)
                };
                ctx.pools_v3.insert(V3Pool {
                    address: pool,
                    token0,
                    token1,
                    fee,
                    tick_spacing,
                    // Factory seeding proves the pool exists but does not
                    // retrieve its historical PoolCreated height.
                    block: 0,
                });
                self.seen_v3.write().insert(pool);
                loaded += 1;
            }
        }
        loaded
    }

    /// The `[from, to]` range to scan for a head of `head`, given `cursor`
    /// (0 == never scanned). Pure, so the window rules are directly testable.
    pub(crate) fn window(cursor: u64, head: u64) -> (u64, u64) {
        let from = if cursor == 0 {
            head.saturating_sub(FIRST_SCAN_LOOKBACK)
        } else if head <= cursor {
            // Head did not advance, or a reorg rewound it. Re-scan the overlap
            // rather than assuming the earlier pass is still valid.
            head.saturating_sub(REORG_DEPTH)
        } else {
            (cursor + 1).saturating_sub(REORG_DEPTH)
        };
        let to = head.min(from.saturating_add(MAX_LOG_SPAN - 1));
        (from, to.max(from))
    }

    /// Scan the V2 factories and load qualifying pools into `pools`.
    pub async fn discover_v2_with<S: DiscoverySource + ?Sized>(
        &self,
        src: &S,
        pools: &PoolCache,
        weth: Address,
        head: u64,
    ) -> DiscoveryStats {
        let (from, to) = Self::window(*self.last_log_block.read(), head);

        let Some(pairs) = src.scan_pairs(from, to).await else {
            // The scan failed: leave the cursor where it is so this range is
            // retried on the next block.
            return DiscoveryStats::default();
        };

        let min_reserve = U256::from(MIN_WETH_RESERVE);
        let mut stats = DiscoveryStats {
            scanned_blocks: to.saturating_sub(from) + 1,
            ..Default::default()
        };

        for (venue, pair) in pairs {
            if !self.should_examine(pair, head) {
                continue;
            }

            // Fetch first, insert second. `PoolCache::load` inserts
            // unconditionally, which would retain non-WETH and dust pools.
            let Some(pool) = src.fetch_pool(pair, venue, head).await else {
                // Transient: record nothing, so the next pass retries.
                stats.retryable += 1;
                continue;
            };
            let Some((weth_reserve, _)) = pool.reserves_for(weth) else {
                // The token pair is immutable — this can never qualify.
                self.non_weth.write().insert(pair);
                continue;
            };
            if weth_reserve < min_reserve {
                // Might qualify later; re-check on a slow cadence.
                self.dust.write().insert(pair, head);
                continue;
            }

            self.accepted.write().insert(pair);
            self.dust.write().remove(&pair);
            pools.insert(pool);
            stats.loaded += 1;
            tracing::info!(
                target: "pools",
                pair = ?pool.address,
                venue = pool.venue.as_str(),
                token = ?pool.other_token(weth),
                weth = ?weth_reserve,
                "discovered new WETH-quoted pool"
            );
        }

        // Only now, after the range was genuinely scanned, advance the cursor.
        *self.last_log_block.write() = to;
        stats
    }

    /// Should this pair be read from chain on this pass?
    fn should_examine(&self, pair: Address, head: u64) -> bool {
        if self.accepted.read().contains(&pair) || self.non_weth.read().contains(&pair) {
            return false;
        }
        match self.dust.read().get(&pair) {
            Some(&checked_at) => head.saturating_sub(checked_at) >= DUST_RECHECK_BLOCKS,
            None => true,
        }
    }

    /// Scan the V3 factory and load qualifying pools into `pools_v3`.
    pub async fn discover_v3_with<S: DiscoverySource + ?Sized>(
        &self,
        src: &S,
        pools_v3: &V3PoolCache,
        weth: Address,
        head: u64,
    ) -> DiscoveryStats {
        let (from, to) = Self::window(*self.last_log_block_v3.read(), head);

        let Some(found) = src.scan_v3_pools(from, to).await else {
            return DiscoveryStats::default();
        };

        let mut stats = DiscoveryStats {
            scanned_blocks: to.saturating_sub(from) + 1,
            ..Default::default()
        };

        for pool in found {
            if self.seen_v3.read().contains(&pool.address) {
                continue;
            }
            // Both filters are on immutable metadata, so a rejection here is
            // permanent and safe to remember.
            self.seen_v3.write().insert(pool.address);
            if pool.other_token(weth).is_none() || !V3Pool::is_actionable_fee(pool.fee) {
                continue;
            }
            pools_v3.insert(pool);
            stats.loaded += 1;
            tracing::info!(
                target: "pools",
                pool = ?pool.address,
                fee = pool.fee,
                token = ?pool.other_token(weth),
                "discovered new WETH-quoted V3 pool"
            );
        }

        *self.last_log_block_v3.write() = to;
        stats
    }

    /// Scan the Aerodrome factory and load qualifying **volatile** pools into
    /// `pools_aero`. Stable pools are recorded-and-skipped permanently: the
    /// flag is immutable and nothing can price them until the P4 invariant
    /// work lands. Same three invariants as the V2 lane — fetch before
    /// insert, cursor advances only over genuinely scanned ranges, separate
    /// cache.
    pub async fn discover_aero_with<S: DiscoverySource + ?Sized>(
        &self,
        src: &S,
        pools_aero: &AeroPoolCache,
        weth: Address,
        head: u64,
    ) -> DiscoveryStats {
        let (from, to) = Self::window(*self.last_log_block_aero.read(), head);

        let Some(seeds) = src.scan_aero_pools(from, to).await else {
            return DiscoveryStats::default();
        };

        let min_reserve = U256::from(MIN_WETH_RESERVE);
        let mut stats = DiscoveryStats {
            scanned_blocks: to.saturating_sub(from) + 1,
            ..Default::default()
        };

        for seed in seeds {
            if seed.stable {
                // Unpriceable by construction until work order P4; immutable,
                // so this rejection is permanent.
                self.seen_aero.write().insert(seed.address);
                continue;
            }
            if self.seen_aero.read().contains(&seed.address)
                || self.non_weth_aero.read().contains(&seed.address)
            {
                continue;
            }
            match self.dust_aero.read().get(&seed.address) {
                Some(&checked_at) if head.saturating_sub(checked_at) < DUST_RECHECK_BLOCKS => {
                    continue
                }
                _ => {}
            }

            // Fetch first, insert second — a failed read leaves the pool
            // eligible for retry next pass (invariant 1).
            let Some(pool) = src.fetch_aero(seed.address, head).await else {
                stats.retryable += 1;
                continue;
            };
            // Belt and braces: the creation log's indexed `stable` flag said
            // volatile, but pricing keys off the on-chain getter.
            if pool.stable {
                self.seen_aero.write().insert(seed.address);
                continue;
            }
            let Some((weth_reserve, _)) = pool.reserves_for(weth) else {
                self.non_weth_aero.write().insert(seed.address);
                continue;
            };
            if weth_reserve < min_reserve {
                self.dust_aero.write().insert(seed.address, head);
                continue;
            }

            self.seen_aero.write().insert(seed.address);
            self.dust_aero.write().remove(&seed.address);
            pools_aero.insert(pool);
            stats.loaded += 1;
            tracing::info!(
                target: "pools",
                pool = ?pool.address,
                token = ?pool.other_token(weth),
                fee_bps = pool.fee_bps,
                "discovered new WETH-quoted Aerodrome volatile pool"
            );
        }

        *self.last_log_block_aero.write() = to;
        stats
    }

    /// Count of Aerodrome pools discovery has resolved (accepted or
    /// permanently classified).
    pub fn seen_aero_count(&self) -> usize {
        self.seen_aero.read().len()
    }

    /// Production entry point: run whichever scans are enabled for this block.
    pub async fn discover(&self, ctx: &StrategyCtx, head: &BlockHead) -> usize {
        let pair_factories = ctx.cfg.addresses.pair_factories();
        let src = RpcSource {
            rpc: &ctx.rpc,
            pair_factories: &pair_factories,
            v3_factory: ctx.cfg.addresses.univ3_factory,
            aero_factory: ctx.cfg.addresses.aerodrome_factory,
        };
        let weth = ctx.cfg.chain.weth;

        let v2 = if ctx.cfg.pool_discovery {
            self.discover_v2_with(&src, &ctx.pools, weth, head.number)
                .await
        } else {
            DiscoveryStats::default()
        };

        let v3 = if ctx.cfg.pool_discovery_v3 {
            self.discover_v3_with(&src, &ctx.pools_v3, weth, head.number)
                .await
        } else {
            DiscoveryStats::default()
        };

        // The Aero lane only runs when the arb graph can price Aerodrome
        // pools — scanning a lane nothing consumes would waste RPC budget.
        let aero = if ctx.cfg.dex_aerodrome_arb && ctx.cfg.addresses.aerodrome_factory.is_some() {
            self.discover_aero_with(&src, &ctx.pools_aero, weth, head.number)
                .await
        } else {
            DiscoveryStats::default()
        };

        if v2.retryable > 0 || aero.retryable > 0 {
            tracing::debug!(
                target: "pools",
                retryable = v2.retryable + aero.retryable,
                "pool reads failed this pass; they stay eligible for retry"
            );
        }
        v2.loaded + v3.loaded + aero.loaded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn addr(n: u8) -> Address {
        Address::with_last_byte(n)
    }

    // A function rather than a const: `with_last_byte` is the constructor the
    // rest of the crate uses and carries no const-fn assumption.
    #[allow(non_snake_case)]
    fn WETH() -> Address {
        Address::with_last_byte(0xee)
    }

    fn v2_pool(pair: Address, weth_reserve: u128) -> V2Pool {
        V2Pool {
            address: pair,
            token0: WETH(),
            token1: addr(9),
            reserve0: U256::from(weth_reserve),
            reserve1: U256::from(1_000_000u64),
            fee_bps: 30,
            venue: Venue::UniV2,
            block: 1,
        }
    }

    /// Scriptable source. `fail_fetch_until` makes `fetch_pool` fail for the
    /// first N calls, which is how the retry invariant is exercised.
    #[derive(Default)]
    struct FakeSource {
        pairs: Vec<(Venue, Address)>,
        v3: Vec<V3Pool>,
        weth_reserve: u128,
        scan_fails: bool,
        fail_fetch_until: usize,
        fetch_calls: AtomicUsize,
        scan_calls: AtomicUsize,
        last_range: RwLock<(u64, u64)>,
    }

    #[async_trait]
    impl DiscoverySource for FakeSource {
        async fn scan_pairs(&self, from: u64, to: u64) -> Option<Vec<(Venue, Address)>> {
            self.scan_calls.fetch_add(1, Ordering::Relaxed);
            *self.last_range.write() = (from, to);
            if self.scan_fails {
                return None;
            }
            Some(self.pairs.clone())
        }

        async fn fetch_pool(&self, pair: Address, _v: Venue, _b: u64) -> Option<V2Pool> {
            let n = self.fetch_calls.fetch_add(1, Ordering::Relaxed);
            if n < self.fail_fetch_until {
                return None;
            }
            Some(v2_pool(pair, self.weth_reserve))
        }

        async fn scan_v3_pools(&self, from: u64, to: u64) -> Option<Vec<V3Pool>> {
            *self.last_range.write() = (from, to);
            if self.scan_fails {
                return None;
            }
            Some(self.v3.clone())
        }
    }

    fn cache() -> PoolCache {
        // PoolCache needs an RpcClient handle, but none of these tests reach
        // the network: every read goes through the FakeSource.
        PoolCache::new(
            RpcClient::new("http://127.0.0.1:1").unwrap(),
            vec![(Venue::UniV2, Address::with_last_byte(1))],
        )
    }

    #[test]
    fn seen_set_is_initially_empty() {
        let d = PoolDiscovery::new();
        assert_eq!(d.seen_count(), 0);
        assert_eq!(d.seen_v3_count(), 0);
    }

    #[test]
    fn first_window_looks_back_a_fixed_distance() {
        assert_eq!(PoolDiscovery::window(0, 1_000), (950, 1_000));
    }

    #[test]
    fn window_advances_with_an_overlap() {
        // Cursor at 1000, head 1010 -> start 12 blocks behind 1001, not at 1001,
        // so a reorg inside the overlap is re-scanned.
        assert_eq!(PoolDiscovery::window(1_000, 1_010), (989, 1_010));
    }

    #[test]
    fn window_handles_a_rewound_head() {
        // Reorg: head went backwards. Re-scan the overlap ending at the new head.
        assert_eq!(PoolDiscovery::window(1_000, 995), (983, 995));
    }

    #[test]
    fn window_is_capped_for_a_stale_cursor() {
        // A 10k-block backlog must not become one enormous eth_getLogs.
        let (from, to) = PoolDiscovery::window(1_000, 11_000);
        assert_eq!(from, 989);
        assert_eq!(to - from + 1, MAX_LOG_SPAN);
    }

    #[tokio::test]
    async fn transient_fetch_failure_is_retried_on_the_next_pass() {
        // The invariant the review called out: a pair whose read failed must
        // not be remembered, or a rate-limited provider permanently blacklists
        // pools that are perfectly good.
        let src = FakeSource {
            pairs: vec![(Venue::UniV2, addr(1))],
            weth_reserve: MIN_WETH_RESERVE,
            fail_fetch_until: 1,
            ..Default::default()
        };
        let d = PoolDiscovery::new();
        let pools = cache();

        let first = d.discover_v2_with(&src, &pools, WETH(), 100).await;
        assert_eq!(first.loaded, 0);
        assert_eq!(first.retryable, 1);
        assert_eq!(d.seen_count(), 0, "a failed read must not be remembered");

        let second = d.discover_v2_with(&src, &pools, WETH(), 101).await;
        assert_eq!(second.loaded, 1, "the pair must be retried and succeed");
        assert_eq!(d.seen_count(), 1);
        assert_eq!(pools.len(), 1);
    }

    #[tokio::test]
    async fn failed_scan_does_not_advance_the_cursor() {
        // Same invariant one level up: if eth_getLogs itself fails, the range
        // must be scanned again rather than skipped.
        let src = FakeSource {
            scan_fails: true,
            ..Default::default()
        };
        let d = PoolDiscovery::new();
        let pools = cache();

        let stats = d.discover_v2_with(&src, &pools, WETH(), 100).await;
        assert_eq!(stats.scanned_blocks, 0);
        assert_eq!(*d.last_log_block.read(), 0, "cursor must not move");

        // Next pass still starts from the first-scan lookback.
        let _ = d.discover_v2_with(&src, &pools, WETH(), 101).await;
        assert_eq!(*src.last_range.read(), (51, 101));
    }

    #[tokio::test]
    async fn duplicate_logs_load_a_pool_once() {
        let src = FakeSource {
            pairs: vec![(Venue::UniV2, addr(1)), (Venue::UniV2, addr(1))],
            weth_reserve: MIN_WETH_RESERVE,
            ..Default::default()
        };
        let d = PoolDiscovery::new();
        let pools = cache();

        let stats = d.discover_v2_with(&src, &pools, WETH(), 100).await;
        assert_eq!(stats.loaded, 1);
        assert_eq!(pools.len(), 1);
        assert_eq!(src.fetch_calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn dust_pools_are_not_cached_but_are_rechecked_later() {
        let src = FakeSource {
            pairs: vec![(Venue::UniV2, addr(1))],
            weth_reserve: MIN_WETH_RESERVE - 1,
            ..Default::default()
        };
        let d = PoolDiscovery::new();
        let pools = cache();

        assert_eq!(
            d.discover_v2_with(&src, &pools, WETH(), 100).await.loaded,
            0
        );
        assert_eq!(pools.len(), 0);
        assert_eq!(d.seen_count(), 0);

        // Immediately after, the pair is not re-read: no per-block RPC storm
        // over the long tail of dust pairs.
        let _ = d.discover_v2_with(&src, &pools, WETH(), 101).await;
        assert_eq!(src.fetch_calls.load(Ordering::Relaxed), 1);

        // Once the recheck interval passes it is read again.
        let _ = d
            .discover_v2_with(&src, &pools, WETH(), 100 + DUST_RECHECK_BLOCKS)
            .await;
        assert_eq!(src.fetch_calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn non_weth_pairs_are_rejected_permanently() {
        struct NonWeth;
        #[async_trait]
        impl DiscoverySource for NonWeth {
            async fn scan_pairs(&self, _f: u64, _t: u64) -> Option<Vec<(Venue, Address)>> {
                Some(vec![(Venue::UniV2, addr(1))])
            }
            async fn fetch_pool(&self, pair: Address, _v: Venue, _b: u64) -> Option<V2Pool> {
                let mut p = v2_pool(pair, MIN_WETH_RESERVE);
                p.token0 = addr(7); // neither side is WETH()
                Some(p)
            }
            async fn scan_v3_pools(&self, _f: u64, _t: u64) -> Option<Vec<V3Pool>> {
                Some(Vec::new())
            }
        }
        let d = PoolDiscovery::new();
        let pools = cache();
        assert_eq!(
            d.discover_v2_with(&NonWeth, &pools, WETH(), 100)
                .await
                .loaded,
            0
        );
        assert!(d.non_weth.read().contains(&addr(1)));
        assert_eq!(pools.len(), 0);
    }

    #[tokio::test]
    async fn v3_pools_never_enter_the_v2_cache() {
        let src = FakeSource {
            v3: vec![V3Pool {
                address: addr(3),
                token0: WETH(),
                token1: addr(9),
                fee: 3_000,
                tick_spacing: 60,
                block: 1,
            }],
            ..Default::default()
        };
        let d = PoolDiscovery::new();
        let pools = cache();
        let pools_v3 = V3PoolCache::new();

        let stats = d.discover_v3_with(&src, &pools_v3, WETH(), 100).await;
        assert_eq!(stats.loaded, 1);
        assert_eq!(pools_v3.len(), 1);
        assert_eq!(pools.len(), 0, "the V2 cache must stay disjoint");
        assert!(pools.get(addr(3)).is_none());
    }

    #[tokio::test]
    async fn v3_filters_unactionable_fee_tiers_and_non_weth_pools() {
        let src = FakeSource {
            v3: vec![
                V3Pool {
                    address: addr(3),
                    token0: WETH(),
                    token1: addr(9),
                    fee: 100, // 1bp tier: skipped
                    tick_spacing: 1,
                    block: 1,
                },
                V3Pool {
                    address: addr(4),
                    token0: addr(8),
                    token1: addr(9), // no WETH() side
                    fee: 3_000,
                    tick_spacing: 60,
                    block: 1,
                },
            ],
            ..Default::default()
        };
        let d = PoolDiscovery::new();
        let pools_v3 = V3PoolCache::new();
        assert_eq!(
            d.discover_v3_with(&src, &pools_v3, WETH(), 100)
                .await
                .loaded,
            0
        );
        assert_eq!(pools_v3.len(), 0);
    }
}
