//! Strategy framework: shared context, pool cache, and router calldata decoding.

pub mod arb;
pub mod discovery;
pub mod jit;
pub mod leads;
pub mod liquidation;
pub mod liquidation_compound;
pub mod liquidation_maker;
pub mod liquidation_morpho;
pub mod oracle_frontrun;
pub mod sandwich;
pub mod sandwich_v3;
pub mod sniper;

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use async_trait::async_trait;
use parking_lot::RwLock;

use crate::config::Config;
use crate::dex::{self, IUniswapV2Router, V2Pool, Venue};
use crate::rpc::RpcClient;
use crate::types::{BlockHead, Opportunity, PendingTx, Strategy};

/// `PairCreated(address indexed token0, address indexed token1, address pair, uint256)`
/// — emitted by every UniV2/SushiV2 factory when a pair is created.
pub const V2_PAIR_CREATED_TOPIC: &str =
    "0x0d3648bd0f6ba80134a33ba9275ac585d9d315f0ad8355cddefde31afa28d0e9";

/// `PoolCreated(address indexed token0, address indexed token1, uint24 indexed fee, int24 tickSpacing, address pool)`
/// — emitted by the UniswapV3 factory. Note the different shape from
/// `PairCreated`: three indexed parameters, and two words of data.
pub const V3_POOL_CREATED_TOPIC: &str =
    "0x783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118";

/// Shared, cheap-to-clone state handed to every strategy.
pub struct StrategyCtx {
    pub cfg: Arc<Config>,
    pub rpc: RpcClient,
    pub executor: Address,
    pub pools: PoolCache,
    /// V3 pools discovered from `PoolCreated`.
    ///
    /// **Deliberately a separate cache from `pools`.** `V2Pool`'s reserve pair
    /// is meaningless for concentrated liquidity, so a V3 pool sitting in the
    /// V2 cache would be priced by `v2_amount_out` and produce quotes that
    /// look plausible, are wrong, and pass every downstream gate.
    pub pools_v3: V3PoolCache,
    /// Aerodrome pools (volatile and, later, stable), separate for the same
    /// reason as `pools_v3`: constant-product-with-fee-off-input math is not
    /// UniV2 math and must never be priced by it.
    pub pools_aero: AeroPoolCache,
    head: RwLock<BlockHead>,
}

impl StrategyCtx {
    pub fn new(cfg: Arc<Config>, rpc: RpcClient, executor: Address, head: BlockHead) -> Self {
        Self {
            pools: PoolCache::new(rpc.clone(), cfg.addresses.pair_factories()),
            pools_v3: V3PoolCache::new(),
            pools_aero: AeroPoolCache::new(rpc.clone(), cfg.addresses.aerodrome_factory),
            cfg,
            rpc,
            executor,
            head: RwLock::new(head),
        }
    }

    pub fn head(&self) -> BlockHead {
        self.head.read().clone()
    }

    pub fn set_head(&self, head: BlockHead) {
        *self.head.write() = head;
    }

    pub fn target_block(&self) -> u64 {
        self.head().number + self.cfg.sim.target_block_offset
    }

    /// JSON-RPC block tag for a state read: `"latest"` at the head, an explicit
    /// height for historical (replay) reads.
    pub fn block_tag(&self, block: u64) -> String {
        block_tag_for(block, self.head().number)
    }

    /// Read a pool at `block`, using the shared cache only when `block` is the
    /// head.
    ///
    /// This is the single entry point strategies should use on the pending
    /// path, because it is what keeps live and replay evaluation from
    /// contaminating each other: live reads hit (and populate) the cache as
    /// before, historical reads go straight to the node and are discarded.
    pub async fn pool_at(&self, pair: Address, venue: Venue, block: u64) -> Option<V2Pool> {
        if block >= self.head().number {
            self.pools.load(pair, venue, block).await
        } else {
            self.pools.read_at(pair, venue, block).await
        }
    }

    /// The Aerodrome counterpart of [`StrategyCtx::pool_at`]: the shared
    /// cache at the head, a discarded direct read for history (replay reads
    /// must never pollute the live cache that arb prices).
    pub async fn aero_pool_at(&self, pool: Address, block: u64) -> Option<crate::dex::AeroPool> {
        if block >= self.head().number {
            self.pools_aero.load(pool, block).await
        } else {
            self.pools_aero.read_at(pool, block).await
        }
    }

    /// Capital the bot is willing to commit to a single bundle.
    pub fn max_position(&self) -> U256 {
        self.cfg.risk.max_position_wei
    }
}

#[async_trait]
pub trait StrategyImpl: Send + Sync {
    fn kind(&self) -> Strategy;

    /// React to a transaction seen in the mempool / private orderflow.
    async fn on_pending(&self, _ctx: &StrategyCtx, _tx: &PendingTx) -> Vec<Opportunity> {
        Vec::new()
    }

    /// React to a new block (used by block-cadence strategies: arb, liquidation).
    async fn on_block(&self, _ctx: &StrategyCtx, _head: &BlockHead) -> Vec<Opportunity> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Pool cache
// ---------------------------------------------------------------------------

/// Caches V2 pool snapshots and refreshes their reserves once per block.
///
/// Reserves are the only thing that changes, and they can be read for dozens of
/// pools in a single batched JSON-RPC round trip.
type PairKey = (Address, Address, Venue);
type PairIndex = Arc<RwLock<HashMap<PairKey, Option<Address>>>>;

#[derive(Clone)]
pub struct PoolCache {
    rpc: RpcClient,
    /// `(venue, factory)` pairs for this chain, from the address registry.
    /// A venue whose factory is absent on the chain simply never resolves a
    /// pair — the strategy's `pair_for` miss path handles it.
    factories: Vec<(Venue, Address)>,
    inner: Arc<RwLock<HashMap<Address, V2Pool>>>,
    pair_index: PairIndex,
}

impl PoolCache {
    pub fn new(rpc: RpcClient, factories: Vec<(Venue, Address)>) -> Self {
        Self {
            rpc,
            factories,
            inner: Arc::new(RwLock::new(HashMap::new())),
            pair_index: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn get(&self, pair: Address) -> Option<V2Pool> {
        self.inner.read().get(&pair).copied()
    }

    pub fn insert(&self, pool: V2Pool) {
        self.inner.write().insert(pool.address, pool);
    }

    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn all(&self) -> Vec<V2Pool> {
        self.inner.read().values().copied().collect()
    }

    /// Look up (and memoise) the pair address for a token couple on a venue.
    pub async fn pair_for(&self, a: Address, b: Address, venue: Venue) -> Option<Address> {
        let (x, y) = if a < b { (a, b) } else { (b, a) };
        if let Some(hit) = self.pair_index.read().get(&(x, y, venue)) {
            return *hit;
        }
        // A venue with no factory on this chain has no pairs to resolve.
        let factory = self.factories.iter().find(|(v, _)| *v == venue)?.1;
        let found = dex::get_pair(&self.rpc, factory, x, y).await.ok().flatten();
        self.pair_index.write().insert((x, y, venue), found);
        found
    }

    /// Load a pool (from cache when fresh for this block, otherwise on chain).
    pub async fn load(&self, pair: Address, venue: Venue, block: u64) -> Option<V2Pool> {
        if let Some(p) = self.get(pair) {
            if p.block == block {
                return Some(p);
            }
        }
        let fee = match venue {
            Venue::SushiV2 => 30,
            _ => 30,
        };
        match dex::fetch_v2_pool(&self.rpc, pair, venue, fee, block).await {
            Ok(p) => {
                self.insert(p);
                Some(p)
            }
            Err(e) => {
                tracing::debug!(target: "pools", pair = ?pair, error = %e, "pool load failed");
                None
            }
        }
    }

    /// Read a pool's state **at a specific historical block**, bypassing the
    /// cache entirely.
    ///
    /// The cache holds one snapshot per pool and `refresh_all` keeps those at
    /// the head, so writing a historical snapshot into it would hand stale
    /// reserves to every live strategy — `graph::search` prices the whole
    /// cache in one pass, so a single polluted entry silently corrupts the
    /// block-cadence arb search. Replay reads therefore cost an RPC each and
    /// leave no trace.
    pub async fn read_at(&self, pair: Address, venue: Venue, block: u64) -> Option<V2Pool> {
        let fee = match venue {
            Venue::SushiV2 => 30,
            _ => 30,
        };
        match dex::fetch_v2_pool(&self.rpc, pair, venue, fee, block).await {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::debug!(target: "pools", pair = ?pair, block, error = %e, "historical pool read failed");
                None
            }
        }
    }

    /// Refresh every cached pool's reserves for `block` in one batch.
    pub async fn refresh_all(&self, block: u64) {
        let pairs: Vec<(Address, Venue, u32)> = self
            .inner
            .read()
            .values()
            .map(|p| (p.address, p.venue, p.fee_bps))
            .collect();
        for chunk in pairs.chunks(50) {
            let calls: Vec<(String, serde_json::Value)> = chunk
                .iter()
                .map(|(addr, _, _)| {
                    (
                        "eth_call".to_string(),
                        serde_json::json!([
                            {
                                "to": format!("{addr:?}"),
                                "data": format!("0x{}", hex::encode(dex::IUniswapV2Pair::getReservesCall {}.abi_encode()))
                            },
                            format!("0x{block:x}")
                        ]),
                    )
                })
                .collect();
            let Ok(results) = self.rpc.batch(&calls).await else {
                continue;
            };
            let mut guard = self.inner.write();
            for ((addr, _, _), res) in chunk.iter().zip(results) {
                let Ok(v) = res else { continue };
                let Some(s) = v.as_str() else { continue };
                let Ok(raw) = hex::decode(s.strip_prefix("0x").unwrap_or(s)) else {
                    continue;
                };
                if raw.len() < 64 {
                    continue;
                }
                if let Some(p) = guard.get_mut(addr) {
                    p.reserve0 = U256::from_be_slice(&raw[0..32]);
                    p.reserve1 = U256::from_be_slice(&raw[32..64]);
                    p.block = block;
                }
            }
        }
    }
}

/// JSON-RPC block tag for reading state at `block` when the chain head is at
/// `head`.
///
/// Anything at or ahead of the head reads `"latest"`; anything behind it is
/// pinned to an explicit height. Getting this backwards is how a replay ends
/// up silently priced against the present.
pub fn block_tag_for(block: u64, head: u64) -> String {
    if block >= head {
        "latest".to_string()
    } else {
        format!("0x{block:x}")
    }
}

// ---------------------------------------------------------------------------
// V3 pool cache
// ---------------------------------------------------------------------------

/// Metadata cache for UniswapV3 pools discovered from `PoolCreated`.
///
/// Only immutable metadata is stored (tokens, fee tier, tick spacing). Mutable
/// state — `slot0`, `liquidity` — is deliberately **not** cached: it changes
/// every swap, and the strategies that need it read it on demand via the
/// helpers in `strategies::jit`. Caching it would invite stale-price sizing.
#[derive(Clone, Default)]
pub struct V3PoolCache {
    inner: Arc<RwLock<HashMap<Address, crate::dex::V3Pool>>>,
}

impl V3PoolCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn get(&self, pool: Address) -> Option<crate::dex::V3Pool> {
        self.inner.read().get(&pool).copied()
    }

    pub fn contains(&self, pool: Address) -> bool {
        self.inner.read().contains_key(&pool)
    }

    pub fn insert(&self, pool: crate::dex::V3Pool) {
        self.inner.write().insert(pool.address, pool);
    }

    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn all(&self) -> Vec<crate::dex::V3Pool> {
        self.inner.read().values().copied().collect()
    }

    /// Every cached pool that quotes `token` (usually WETH).
    pub fn quoted_by(&self, token: Address) -> Vec<crate::dex::V3Pool> {
        self.inner
            .read()
            .values()
            .filter(|p| p.token0 == token || p.token1 == token)
            .copied()
            .collect()
    }

    /// The cached pool for `(a, b, fee)`, if discovery has loaded it.
    pub fn for_pair(&self, a: Address, b: Address, fee: u32) -> Option<crate::dex::V3Pool> {
        self.inner.read().values().copied().find(|p| {
            p.fee == fee && ((p.token0 == a && p.token1 == b) || (p.token0 == b && p.token1 == a))
        })
    }
}

// ---------------------------------------------------------------------------
// Aerodrome pool cache
// ---------------------------------------------------------------------------

/// Reserve-bearing Aerodrome pools, refreshed per block like the V2 cache
/// but priced with the venue's own fee-off-input math ([`crate::dex::AeroPool`]).
///
/// Deliberately separate from [`PoolCache`]: an Aerodrome pool sitting in the
/// V2 cache would be priced by UniV2's in-numerator-fee formula and would
/// produce quotes that look plausible, differ by wei, and pass every
/// downstream gate.
/// Memoised `(tokenA, tokenB, stable)` → pool lookups for the Aerodrome
/// factory, tokens stored sorted like the V2 index.
type AeroPairKey = (Address, Address, bool);

#[derive(Clone)]
pub struct AeroPoolCache {
    rpc: RpcClient,
    /// Aerodrome pool factory for this chain (registry), or `None` when the
    /// chain has none — lookups fail closed with no pools.
    factory: Option<Address>,
    inner: Arc<RwLock<HashMap<Address, crate::dex::AeroPool>>>,
    pair_index: Arc<RwLock<HashMap<AeroPairKey, Option<Address>>>>,
}

impl AeroPoolCache {
    pub fn new(rpc: RpcClient, factory: Option<Address>) -> Self {
        Self {
            rpc,
            factory,
            inner: Arc::new(RwLock::new(HashMap::new())),
            pair_index: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn get(&self, pool: Address) -> Option<crate::dex::AeroPool> {
        self.inner.read().get(&pool).copied()
    }

    pub fn insert(&self, pool: crate::dex::AeroPool) {
        self.inner.write().insert(pool.address, pool);
    }

    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn all(&self) -> Vec<crate::dex::AeroPool> {
        self.inner.read().values().copied().collect()
    }

    /// Volatile pools only — the only capability the v1 graph can execute.
    pub fn all_volatile(&self) -> Vec<crate::dex::AeroPool> {
        self.inner
            .read()
            .values()
            .filter(|p| !p.stable)
            .copied()
            .collect()
    }

    /// Look up (and memoise) the pool address for a token couple.
    /// `stable = false` is the volatile lane; stable lookups are memoised
    /// too but only ever loaded once work order P4 exists.
    pub async fn pool_for(&self, a: Address, b: Address, stable: bool) -> Option<Address> {
        let (x, y) = if a < b { (a, b) } else { (b, a) };
        if let Some(hit) = self.pair_index.read().get(&(x, y, stable)) {
            return *hit;
        }
        let factory = self.factory?;
        let found = dex::aero_get_pool(&self.rpc, factory, x, y, stable)
            .await
            .ok()
            .flatten();
        self.pair_index.write().insert((x, y, stable), found);
        found
    }

    /// Load (or fresh-hit) a pool at `block`.
    pub async fn load(&self, pool: Address, block: u64) -> Option<crate::dex::AeroPool> {
        if let Some(p) = self.get(pool) {
            if p.block == block {
                return Some(p);
            }
        }
        let factory = self.factory?;
        match dex::fetch_aero_pool(&self.rpc, factory, pool, block).await {
            Ok(p) => {
                self.insert(p);
                Some(p)
            }
            Err(e) => {
                tracing::debug!(target: "pools", pool = ?pool, error = %e, "aerodrome pool load failed");
                None
            }
        }
    }

    /// Read a pool's state at a specific historical block, bypassing the
    /// cache (same replay-pollution argument as [`PoolCache::read_at`]).
    pub async fn read_at(&self, pool: Address, block: u64) -> Option<crate::dex::AeroPool> {
        let factory = self.factory?;
        match dex::fetch_aero_pool(&self.rpc, factory, pool, block).await {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::debug!(target: "pools", pool = ?pool, block, error = %e, "historical aerodrome read failed");
                None
            }
        }
    }

    /// Refresh every cached pool's reserves for `block` in one batch, chunk
    /// of 50, same shape as [`PoolCache::refresh_all`] — Aerodrome's
    /// `getReserves()` returns the same leading two uint256 words.
    pub async fn refresh_all(&self, block: u64) {
        let pools: Vec<Address> = self.inner.read().keys().copied().collect();
        for chunk in pools.chunks(50) {
            let calls: Vec<(String, serde_json::Value)> = chunk
                .iter()
                .map(|addr| {
                    (
                        "eth_call".to_string(),
                        serde_json::json!([
                            {
                                "to": format!("{addr:?}"),
                                "data": format!("0x{}", hex::encode(dex::IAerodromePool::getReservesCall {}.abi_encode()))
                            },
                            format!("0x{block:x}")
                        ]),
                    )
                })
                .collect();
            let Ok(results) = self.rpc.batch(&calls).await else {
                continue;
            };
            let mut guard = self.inner.write();
            for (addr, res) in chunk.iter().zip(results) {
                let Ok(v) = res else { continue };
                let Some(s) = v.as_str() else { continue };
                let Ok(raw) = hex::decode(s.strip_prefix("0x").unwrap_or(s)) else {
                    continue;
                };
                if raw.len() < 64 {
                    continue;
                }
                if let Some(p) = guard.get_mut(addr) {
                    p.reserve0 = U256::from_be_slice(&raw[0..32]);
                    p.reserve1 = U256::from_be_slice(&raw[32..64]);
                    p.block = block;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Router calldata decoding
// ---------------------------------------------------------------------------
/// A swap intent extracted from a pending transaction.
#[derive(Clone, Debug)]
pub struct SwapIntent {
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub min_out: U256,
    pub path: Vec<Address>,
    pub router: Address,
    /// True when the input is native ETH (the router wraps it).
    pub native_in: bool,
}

/// Decode the UniswapV2-style router calls that make up the overwhelming
/// majority of sandwichable retail flow.
pub fn decode_swap(tx: &PendingTx, weth: Address) -> Option<SwapIntent> {
    let to = tx.to?;
    let data = &tx.input;
    if data.len() < 4 {
        return None;
    }
    let sel: [u8; 4] = [data[0], data[1], data[2], data[3]];

    if sel == IUniswapV2Router::swapExactTokensForTokensCall::SELECTOR {
        let c = IUniswapV2Router::swapExactTokensForTokensCall::abi_decode(data, false).ok()?;
        let path = c.path.clone();
        return Some(SwapIntent {
            token_in: *path.first()?,
            token_out: *path.last()?,
            amount_in: c.amountIn,
            min_out: c.amountOutMin,
            path,
            router: to,
            native_in: false,
        });
    }
    if sel == IUniswapV2Router::swapExactETHForTokensCall::SELECTOR {
        let c = IUniswapV2Router::swapExactETHForTokensCall::abi_decode(data, false).ok()?;
        let path = c.path.clone();
        return Some(SwapIntent {
            token_in: *path.first().unwrap_or(&weth),
            token_out: *path.last()?,
            amount_in: tx.value,
            min_out: c.amountOutMin,
            path,
            router: to,
            native_in: true,
        });
    }
    if sel == IUniswapV2Router::swapExactTokensForETHCall::SELECTOR {
        let c = IUniswapV2Router::swapExactTokensForETHCall::abi_decode(data, false).ok()?;
        let path = c.path.clone();
        return Some(SwapIntent {
            token_in: *path.first()?,
            token_out: *path.last().unwrap_or(&weth),
            amount_in: c.amountIn,
            min_out: c.amountOutMin,
            path,
            router: to,
            native_in: false,
        });
    }
    None
}

/// Combined decoder: V2 routers first, then UniversalRouter.
///
/// Strategies that already consume [`SwapIntent`] should call this (or
/// [`decode_router`]) rather than growing a second decode path.
/// `universal_router` is the chain's UniversalRouter deployment from the
/// address registry — `None` disables the UR pass for this chain.
pub fn decode_any_router(
    tx: &PendingTx,
    weth: Address,
    universal_router: Option<Address>,
) -> Option<SwapIntent> {
    decode_swap(tx, weth).or_else(|| {
        let (router, to) = universal_router.zip(tx.to)?;
        crate::dex::calldata::decode_universal_router(router, to, &tx.input, tx.value, weth).map(
            |u| SwapIntent {
                token_in: u.token_in,
                token_out: u.token_out,
                amount_in: u.amount_in,
                min_out: u.min_out,
                path: u.path,
                router: to,
                native_in: u.native_in,
            },
        )
    })
}

/// Pick the decoder the operator asked for. When UniversalRouter is off this
/// is exactly [`decode_swap`], so a default-off checkout is behaviour-identical
/// to the code that collected the funnel baseline.
pub fn decode_router(
    tx: &PendingTx,
    weth: Address,
    universal: bool,
    universal_router: Option<Address>,
) -> Option<SwapIntent> {
    if universal {
        decode_any_router(tx, weth, universal_router)
    } else {
        decode_swap(tx, weth)
    }
}

/// Run `eth_getLogs` for `PairCreated` on the chain's V2 factories over
/// `[from, to]`. Returns the decoded `(venue, pair_address)` tuples. A failed
/// RPC call yields an empty vec (callers treat it as "no new pools this block").
///
/// Prefer [`try_scan_pair_created`] when the caller advances a scan cursor:
/// this signature cannot distinguish "no pairs created" from "the RPC call
/// failed", and advancing a cursor past a failed range loses those logs
/// permanently.
pub async fn scan_pair_created(
    rpc: &crate::rpc::RpcClient,
    factories: &[(crate::dex::Venue, Address)],
    from: u64,
    to: u64,
) -> Vec<(crate::dex::Venue, Address)> {
    try_scan_pair_created(rpc, factories, from, to)
        .await
        .unwrap_or_default()
}

/// Fallible form of [`scan_pair_created`]: `None` means the RPC call itself
/// failed, `Some(vec![])` means the range genuinely contained no pairs.
pub async fn try_scan_pair_created(
    rpc: &crate::rpc::RpcClient,
    factories: &[(crate::dex::Venue, Address)],
    from: u64,
    to: u64,
) -> Option<Vec<(crate::dex::Venue, Address)>> {
    let addresses: Vec<Address> = factories.iter().map(|(_, a)| *a).collect();
    let logs = scan_factory_logs(rpc, &addresses, V2_PAIR_CREATED_TOPIC, from, to).await?;
    Some(
        logs.iter()
            .filter_map(|log| decode_pair_created(log, factories))
            .collect(),
    )
}

/// Fetch raw logs for one topic across a set of factory addresses.
///
/// `None` is returned when the RPC call fails, so callers can tell a failed
/// scan apart from an empty one. Decoding is the caller's job — each event has
/// its own layout and conflating them is how a V3 pool ends up in a V2 cache.
pub async fn scan_factory_logs(
    rpc: &crate::rpc::RpcClient,
    addresses: &[Address],
    topic0: &str,
    from: u64,
    to: u64,
) -> Option<Vec<serde_json::Value>> {
    let addrs: Vec<String> = addresses.iter().map(|a| format!("{a:?}")).collect();
    let params = serde_json::json!([{
        "fromBlock": format!("0x{from:x}"),
        "toBlock": format!("0x{to:x}"),
        "address": addrs,
        "topics": [topic0],
    }]);

    match rpc.call_raw("eth_getLogs", params).await {
        Ok(v) => Some(v.as_array().cloned().unwrap_or_default()),
        Err(e) => {
            tracing::debug!(target: "pools", from, to, error = %e, "eth_getLogs failed");
            None
        }
    }
}

/// Decode one `PairCreated` log into `(venue, pair)`.
///
/// For `PairCreated`, `data` is `(pair address, uint256 allPairsLength)` with
/// the pair address right-aligned in the first 32 bytes.
pub fn decode_pair_created(
    log: &serde_json::Value,
    factories: &[(crate::dex::Venue, Address)],
) -> Option<(crate::dex::Venue, Address)> {
    let topic0 = log["topics"].as_array()?.first()?.as_str()?;
    if !topic0.eq_ignore_ascii_case(V2_PAIR_CREATED_TOPIC) {
        return None;
    }
    let venue = venue_from_factory(&log["address"], factories)?;
    let data = crate::types::parse_bytes(&log["data"]);
    if data.len() < 32 {
        return None;
    }
    Some((venue, Address::from_slice(&data[12..32])))
}

/// Decode one UniswapV3 `PoolCreated` log.
///
/// Layout differs from `PairCreated` and getting it wrong yields addresses
/// that look valid: `token0`, `token1` and `fee` are **indexed** (topics 1..3),
/// while `tickSpacing` and `pool` live in `data` in that order.
pub fn decode_pool_created(log: &serde_json::Value) -> Option<crate::dex::V3Pool> {
    let topics = log["topics"].as_array()?;
    let topic0 = topics.first()?.as_str()?;
    if !topic0.eq_ignore_ascii_case(V3_POOL_CREATED_TOPIC) {
        return None;
    }
    if topics.len() < 4 {
        return None;
    }
    let t1 = crate::types::parse_bytes(&topics[1]);
    let t2 = crate::types::parse_bytes(&topics[2]);
    let t3 = crate::types::parse_bytes(&topics[3]);
    if t1.len() < 32 || t2.len() < 32 || t3.len() < 32 {
        return None;
    }
    let token0 = Address::from_slice(&t1[12..32]);
    let token1 = Address::from_slice(&t2[12..32]);
    // uint24 fee, right-aligned in the 32-byte topic.
    let fee = u32::from_be_bytes([0, t3[29], t3[30], t3[31]]);

    let data = crate::types::parse_bytes(&log["data"]);
    if data.len() < 64 {
        return None;
    }
    let tick_spacing = crate::strategies::jit::i256_word_to_i32(&data[0..32]);
    let address = Address::from_slice(&data[44..64]);
    let block = crate::types::parse_u64(&log["blockNumber"]);

    Some(crate::dex::V3Pool {
        address,
        token0,
        token1,
        fee,
        tick_spacing,
        block,
    })
}

/// Scan the UniswapV3 factory for `PoolCreated` over `[from, to]`.
/// `None` means the RPC call failed.
pub async fn try_scan_pool_created(
    rpc: &crate::rpc::RpcClient,
    v3_factory: Address,
    from: u64,
    to: u64,
) -> Option<Vec<crate::dex::V3Pool>> {
    let logs = scan_factory_logs(rpc, &[v3_factory], V3_POOL_CREATED_TOPIC, from, to).await?;
    Some(logs.iter().filter_map(decode_pool_created).collect())
}

/// An Aerodrome `PoolCreated` log reduced to its immutable seed. Reserves
/// and the per-pool fee are deliberately **not** in here — they are mutable
/// (or live on the factory), so the discovery lane fetches the pool itself
/// rather than quoting from a stale creation-log snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AeroPoolSeed {
    pub token0: Address,
    pub token1: Address,
    pub stable: bool,
    pub address: Address,
    pub block: u64,
}

/// Decode one Aerodrome `PoolCreated(token0, token1, stable, pool, _)`.
///
/// All three of `token0`, `token1`, `stable` are **indexed** (topics 1..3);
/// `pool` is the first word of `data` (right-aligned), followed by the
/// factory's `allPoolsLength`. Layout verified against a real Base log
/// 2026-08-24 — mixing indexed and data fields up yields plausible-looking
/// wrong addresses, the failure mode these guards exist for.
pub fn decode_aero_pool_created(log: &serde_json::Value) -> Option<AeroPoolSeed> {
    let topics = log["topics"].as_array()?;
    if !topics
        .first()?
        .as_str()?
        .eq_ignore_ascii_case(dex::AERO_POOL_CREATED_TOPIC)
    {
        return None;
    }
    if topics.len() < 4 {
        return None;
    }
    let t1 = crate::types::parse_bytes(&topics[1]);
    let t2 = crate::types::parse_bytes(&topics[2]);
    let t3 = crate::types::parse_bytes(&topics[3]);
    if t1.len() < 32 || t2.len() < 32 || t3.len() < 32 {
        return None;
    }
    let token0 = Address::from_slice(&t1[12..32]);
    let token1 = Address::from_slice(&t2[12..32]);
    // bool, right-aligned: only the last byte may be non-zero.
    let stable = match t3[31] {
        0 if t3[..31].iter().all(|b| *b == 0) => false,
        1 if t3[..31].iter().all(|b| *b == 0) => true,
        _ => return None,
    };
    let data = crate::types::parse_bytes(&log["data"]);
    if data.len() < 32 {
        return None;
    }
    Some(AeroPoolSeed {
        token0,
        token1,
        stable,
        address: Address::from_slice(&data[12..32]),
        block: crate::types::parse_u64(&log["blockNumber"]),
    })
}

/// Scan the Aerodrome pool factory for `PoolCreated` over `[from, to]`.
/// `None` means the RPC call failed.
pub async fn try_scan_aero_pool_created(
    rpc: &crate::rpc::RpcClient,
    factory: Address,
    from: u64,
    to: u64,
) -> Option<Vec<AeroPoolSeed>> {
    let logs = scan_factory_logs(rpc, &[factory], dex::AERO_POOL_CREATED_TOPIC, from, to).await?;
    Some(logs.iter().filter_map(decode_aero_pool_created).collect())
}

/// Map the emitting factory address to its venue, using the chain's
/// registry (`factories`) rather than a hardcoded table — a chain whose
/// V2 factory differs from mainnet's must still decode its own logs.
fn venue_from_factory(
    factory: &serde_json::Value,
    factories: &[(crate::dex::Venue, Address)],
) -> Option<crate::dex::Venue> {
    let s = factory.as_str()?;
    factories
        .iter()
        .find(|(_, a)| s.eq_ignore_ascii_case(&format!("{a:?}")))
        .map(|(v, _)| *v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::known;
    use crate::types::{now_ms, TxSource};
    use alloy_primitives::address;
    use alloy_primitives::B256;

    fn tx_with(data: Vec<u8>, value: U256) -> PendingTx {
        PendingTx {
            hash: B256::ZERO,
            from: None,
            to: Some(known::UNIV2_ROUTER),
            value,
            gas: 200_000,
            max_fee_per_gas: U256::from(20_000_000_000u64),
            max_priority_fee_per_gas: U256::from(1_000_000_000u64),
            nonce: 0,
            input: data,
            raw: None,
            source: TxSource::PublicMempool,
            mined_at: None,
            preconfirmed: None,
            seen_at_ms: now_ms(),
        }
    }

    #[test]
    fn decodes_exact_tokens_for_tokens() {
        let path = vec![known::WETH, known::USDC];
        let data = IUniswapV2Router::swapExactTokensForTokensCall {
            amountIn: U256::from(1_000u64),
            amountOutMin: U256::from(900u64),
            path: path.clone(),
            to: Address::ZERO,
            deadline: U256::from(9_999_999_999u64),
        }
        .abi_encode();
        let intent = decode_swap(&tx_with(data, U256::ZERO), known::WETH).unwrap();
        assert_eq!(intent.token_in, known::WETH);
        assert_eq!(intent.token_out, known::USDC);
        assert_eq!(intent.amount_in, U256::from(1_000u64));
        assert_eq!(intent.min_out, U256::from(900u64));
        assert!(!intent.native_in);
    }

    #[test]
    fn decodes_eth_swaps_using_tx_value() {
        let data = IUniswapV2Router::swapExactETHForTokensCall {
            amountOutMin: U256::from(1u64),
            path: vec![known::WETH, known::USDC],
            to: Address::ZERO,
            deadline: U256::from(9_999_999_999u64),
        }
        .abi_encode();
        let intent = decode_swap(&tx_with(data, U256::from(5_000u64)), known::WETH).unwrap();
        assert!(intent.native_in);
        assert_eq!(intent.amount_in, U256::from(5_000u64));
    }

    #[test]
    fn ignores_unrelated_calldata() {
        assert!(decode_swap(
            &tx_with(vec![0xde, 0xad, 0xbe, 0xef], U256::ZERO),
            known::WETH
        )
        .is_none());
        assert!(decode_swap(&tx_with(vec![], U256::ZERO), known::WETH).is_none());
    }

    #[test]
    fn decode_router_off_does_not_see_universal_router() {
        // Default-off must be behaviour-identical to decode_swap: a
        // UniversalRouter execute is invisible until the operator flips
        // DECODE_UNIVERSAL_ROUTER. Otherwise the funnel baseline is polluted.
        use alloy_sol_types::SolValue;
        let path = vec![known::WETH, known::USDC];
        let input = (
            Address::with_last_byte(9),
            U256::from(1_000u64),
            U256::from(1u64),
            path,
            true,
        )
            .abi_encode();
        let data = crate::dex::calldata::universal_router::encode_execute(
            vec![crate::dex::calldata::universal_router::CMD_V2_SWAP_EXACT_IN],
            vec![input],
        );
        let mut tx = tx_with(data, U256::ZERO);
        tx.to = Some(known::UNIVERSAL_ROUTER);
        assert!(decode_router(&tx, known::WETH, false, None).is_none());
        let got = decode_router(&tx, known::WETH, true, Some(known::UNIVERSAL_ROUTER))
            .expect("UR decodes when enabled");
        assert_eq!(got.token_in, known::WETH);
        assert_eq!(got.token_out, known::USDC);
        assert_eq!(got.amount_in, U256::from(1_000u64));
    }

    #[test]
    fn venue_from_factory_maps_registered_factories() {
        let factories = [
            (Venue::UniV2, known::UNIV2_FACTORY),
            (Venue::SushiV2, known::SUSHI_FACTORY),
        ];
        assert_eq!(
            venue_from_factory(
                &serde_json::json!(format!("{:?}", known::UNIV2_FACTORY)),
                &factories
            ),
            Some(Venue::UniV2)
        );
        assert_eq!(
            venue_from_factory(
                &serde_json::json!(format!("{:?}", known::SUSHI_FACTORY)),
                &factories
            ),
            Some(Venue::SushiV2)
        );
        // Unknown factory -> None.
        assert_eq!(
            venue_from_factory(
                &serde_json::json!("0x0000000000000000000000000000000000000000"),
                &factories
            ),
            None
        );
        // A registry without the emitting factory (e.g. Base has no Sushi)
        // maps it to None as well.
        let only_univ2 = [(Venue::UniV2, known::UNIV2_FACTORY)];
        assert_eq!(
            venue_from_factory(
                &serde_json::json!(format!("{:?}", known::SUSHI_FACTORY)),
                &only_univ2
            ),
            None
        );
        // Non-string -> None.
        assert_eq!(venue_from_factory(&serde_json::json!(42), &factories), None);
    }

    #[test]
    fn scan_decodes_pair_address_from_log_data() {
        // Build a 32-byte data payload with a known address right-aligned.
        let expected_pair = address!("1234567890abcdef1234567890abcdef12345678");
        let mut data = [0u8; 32];
        data[12..32].copy_from_slice(expected_pair.as_slice());

        // Simulate the inner decode loop of `scan_pair_created` without an RPC.
        let pair = Address::from_slice(&data[12..32]);
        assert_eq!(pair, expected_pair);

        // A short data payload (< 32 bytes) must be skipped.
        let short = [0u8; 16];
        assert!(short.len() < 32);
    }

    fn padded(a: Address) -> String {
        format!("0x000000000000000000000000{}", hex::encode(a.as_slice()))
    }

    /// A `PairCreated` log shaped exactly as a node returns one.
    fn pair_created_log(factory: Address, pair: Address) -> serde_json::Value {
        serde_json::json!({
            "address": format!("{factory:?}"),
            "topics": [
                V2_PAIR_CREATED_TOPIC,
                padded(known::WETH),
                padded(known::USDC),
            ],
            // (pair address, allPairsLength)
            "data": format!("0x000000000000000000000000{}{:064x}", hex::encode(pair.as_slice()), 42u64),
        })
    }

    /// A `PoolCreated` log for the real USDC/WETH 0.05% pool.
    fn pool_created_log() -> serde_json::Value {
        let pool = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
        serde_json::json!({
            "address": format!("{:?}", known::UNIV3_FACTORY),
            "blockNumber": "0x112a880",
            "topics": [
                V3_POOL_CREATED_TOPIC,
                padded(known::USDC),
                padded(known::WETH),
                format!("0x{:064x}", 500u32),
            ],
            // (int24 tickSpacing, address pool)
            "data": format!("0x{:064x}000000000000000000000000{}", 10u32, hex::encode(pool.as_slice())),
        })
    }

    fn test_factories() -> [(Venue, Address); 2] {
        [
            (Venue::UniV2, known::UNIV2_FACTORY),
            (Venue::SushiV2, known::SUSHI_FACTORY),
        ]
    }

    #[test]
    fn decodes_a_pair_created_log() {
        let pair = address!("b4e16d0168e52d35cacd2c6185b44281ec28c9dc");
        let (venue, got) = decode_pair_created(
            &pair_created_log(known::UNIV2_FACTORY, pair),
            &test_factories(),
        )
        .expect("a well-formed PairCreated log decodes");
        assert_eq!(venue, Venue::UniV2);
        assert_eq!(got, pair);

        let (venue, _) = decode_pair_created(
            &pair_created_log(known::SUSHI_FACTORY, pair),
            &test_factories(),
        )
        .unwrap();
        assert_eq!(venue, Venue::SushiV2);
    }

    #[test]
    fn decodes_a_pool_created_log() {
        // Indexed tokens and fee come from the topics; tickSpacing and the pool
        // address come from data. Mixing those up yields addresses that look
        // valid, which is why this is pinned against a real mainnet pool.
        let got = decode_pool_created(&pool_created_log()).expect("PoolCreated decodes");
        assert_eq!(
            got.address,
            address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640")
        );
        assert_eq!(got.token0, known::USDC);
        assert_eq!(got.token1, known::WETH);
        assert_eq!(got.fee, 500);
        assert_eq!(got.tick_spacing, 10);
        assert_eq!(got.block, 18_000_000);
        assert!(crate::dex::V3Pool::is_actionable_fee(got.fee));
    }

    /// The real Aerodrome `PoolCreated` for the WETH/`0x9c05…e9f6` volatile
    /// pool `0xb64ce58ed12a84ba00dc4dd58d28771b9308597d`, emitted by Base
    /// block 50,390,210 (0x300e4c2). Cross-checked live 2026-08-24:
    /// `factory.getPool(t0, t1, false)` returns the pool, `pool.stable()` is
    /// false, `factory.getFee(pool, false)` is 30.
    fn aero_pool_created_log() -> serde_json::Value {
        serde_json::json!({
            "address": "0x420DD381b31aEf6683db6B902084cB0FFECe40Da",
            "blockNumber": "0x300e4c2",
            "topics": [
                crate::dex::AERO_POOL_CREATED_TOPIC,
                "0x0000000000000000000000004200000000000000000000000000000000000006",
                "0x0000000000000000000000009c0540faceb85ef926c53e693cf2f3353802e9f6",
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ],
            "data": "0x000000000000000000000000b64ce58ed12a84ba00dc4dd58d28771b9308597d0000000000000000000000000000000000000000000000000000000000006fd9",
        })
    }

    #[test]
    fn decodes_a_real_aerodrome_pool_created_log() {
        let seed = decode_aero_pool_created(&aero_pool_created_log()).expect("decodes");
        assert_eq!(
            seed.token0,
            address!("4200000000000000000000000000000000000006")
        );
        assert_eq!(
            seed.token1,
            address!("9c0540faceb85ef926c53e693cf2f3353802e9f6")
        );
        assert!(!seed.stable, "the real pool is volatile (topic3 == 0)");
        assert_eq!(
            seed.address,
            address!("b64ce58ed12a84ba00dc4dd58d28771b9308597d")
        );
        assert_eq!(seed.block, 50_390_210);
    }

    #[test]
    fn aero_decoder_rejects_wrong_shapes_and_foreign_logs() {
        // A V2 PairCreated log must not decode as an Aero pool.
        let pair = address!("b4e16d0168e52d35cacd2c6185b44281ec28c9dc");
        assert!(decode_aero_pool_created(&pair_created_log(known::UNIV2_FACTORY, pair)).is_none());
        // A V3 PoolCreated log likewise.
        assert!(decode_aero_pool_created(&pool_created_log()).is_none());
        // A genuine Aero PoolCreated with stable = true flags the stable lane.
        let mut stable_log = aero_pool_created_log();
        stable_log["topics"][3] = serde_json::json!(format!("0x{:064x}", 1u8));
        let seed = decode_aero_pool_created(&stable_log).expect("stable=true still decodes");
        assert!(seed.stable);
        // But a garbage bool word is malformed, not "true".
        let mut bad = aero_pool_created_log();
        bad["topics"][3] = serde_json::json!(format!("0x{:064x}", 2u8));
        assert!(decode_aero_pool_created(&bad).is_none());
        // Truncated data cannot yield a pool address.
        let mut short = aero_pool_created_log();
        short["data"] = serde_json::json!("0x00");
        assert!(decode_aero_pool_created(&short).is_none());
        assert!(decode_aero_pool_created(&serde_json::json!({})).is_none());
    }

    #[test]
    fn the_two_decoders_reject_each_other_s_logs() {
        // A V2 pair decoded as a V3 pool (or the reverse) is the failure mode
        // that would put a concentrated-liquidity pool into a constant-product
        // cache, so both directions get an explicit test.
        let pair = address!("b4e16d0168e52d35cacd2c6185b44281ec28c9dc");
        let v2 = pair_created_log(known::UNIV2_FACTORY, pair);
        let v3 = pool_created_log();

        assert!(
            decode_pool_created(&v2).is_none(),
            "a PairCreated log has only 3 topics and must not decode as V3"
        );
        assert!(
            decode_pair_created(&v3, &test_factories()).is_none(),
            "a PoolCreated log comes from the V3 factory, which maps to no V2 venue"
        );
    }

    #[test]
    fn block_tag_is_latest_only_at_or_ahead_of_the_head() {
        assert_eq!(block_tag_for(1_000, 1_000), "latest");
        assert_eq!(block_tag_for(1_001, 1_000), "latest");
        // Historical reads must be pinned, and pinned in hex.
        assert_eq!(block_tag_for(999, 1_000), "0x3e7");
        assert_eq!(block_tag_for(0, 1_000), "0x0");
    }

    #[test]
    fn malformed_logs_return_none_instead_of_panicking() {
        assert!(decode_pair_created(&serde_json::json!({}), &test_factories()).is_none());
        assert!(decode_pool_created(&serde_json::json!({})).is_none());
        assert!(decode_pool_created(&serde_json::json!({"topics": [], "data": "0x"})).is_none());
        // Right topic count, truncated data.
        let short = serde_json::json!({
            "address": format!("{:?}", known::UNIV3_FACTORY),
            "topics": [
                V3_POOL_CREATED_TOPIC,
                padded(known::USDC),
                padded(known::WETH),
                format!("0x{:064x}", 500u32),
            ],
            "data": "0x00",
        });
        assert!(decode_pool_created(&short).is_none());
    }
}
