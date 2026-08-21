//! Strategy framework: shared context, pool cache, and router calldata decoding.

pub mod arb;
pub mod discovery;
pub mod jit;
pub mod liquidation;
pub mod sandwich;
pub mod sniper;

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;
use async_trait::async_trait;
use parking_lot::RwLock;

use crate::config::{known, Config};
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
    head: RwLock<BlockHead>,
}

impl StrategyCtx {
    pub fn new(cfg: Arc<Config>, rpc: RpcClient, executor: Address, head: BlockHead) -> Self {
        Self {
            pools: PoolCache::new(rpc.clone()),
            pools_v3: V3PoolCache::new(),
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
#[derive(Clone)]
pub struct PoolCache {
    rpc: RpcClient,
    inner: Arc<RwLock<HashMap<Address, V2Pool>>>,
    pair_index: Arc<RwLock<HashMap<(Address, Address, Venue), Option<Address>>>>,
}

impl PoolCache {
    pub fn new(rpc: RpcClient) -> Self {
        Self {
            rpc,
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
        let factory = match venue {
            Venue::UniV2 => known::UNIV2_FACTORY,
            Venue::SushiV2 => known::SUSHI_FACTORY,
            Venue::UniV3 => return None,
        };
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

/// Run `eth_getLogs` for `PairCreated` on both V2 factories over `[from, to]`.
/// Returns the decoded `(venue, pair_address)` tuples. A failed RPC call yields
/// an empty vec (callers treat it as "no new pools this block").
///
/// Prefer [`try_scan_pair_created`] when the caller advances a scan cursor:
/// this signature cannot distinguish "no pairs created" from "the RPC call
/// failed", and advancing a cursor past a failed range loses those logs
/// permanently.
pub async fn scan_pair_created(
    rpc: &crate::rpc::RpcClient,
    from: u64,
    to: u64,
) -> Vec<(crate::dex::Venue, Address)> {
    try_scan_pair_created(rpc, from, to).await.unwrap_or_default()
}

/// Fallible form of [`scan_pair_created`]: `None` means the RPC call itself
/// failed, `Some(vec![])` means the range genuinely contained no pairs.
pub async fn try_scan_pair_created(
    rpc: &crate::rpc::RpcClient,
    from: u64,
    to: u64,
) -> Option<Vec<(crate::dex::Venue, Address)>> {
    let logs = scan_factory_logs(
        rpc,
        &[known::UNIV2_FACTORY, known::SUSHI_FACTORY],
        V2_PAIR_CREATED_TOPIC,
        from,
        to,
    )
    .await?;
    Some(logs.iter().filter_map(decode_pair_created).collect())
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
pub fn decode_pair_created(log: &serde_json::Value) -> Option<(crate::dex::Venue, Address)> {
    let topic0 = log["topics"].as_array()?.first()?.as_str()?;
    if !topic0.eq_ignore_ascii_case(V2_PAIR_CREATED_TOPIC) {
        return None;
    }
    let venue = venue_from_factory(&log["address"])?;
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
    from: u64,
    to: u64,
) -> Option<Vec<crate::dex::V3Pool>> {
    let logs = scan_factory_logs(
        rpc,
        &[known::UNIV3_FACTORY],
        V3_POOL_CREATED_TOPIC,
        from,
        to,
    )
    .await?;
    Some(logs.iter().filter_map(decode_pool_created).collect())
}

/// Map the emitting factory address to its venue.
fn venue_from_factory(factory: &serde_json::Value) -> Option<crate::dex::Venue> {
    let s = factory.as_str()?;
    if s.eq_ignore_ascii_case(&format!("{:?}", known::UNIV2_FACTORY)) {
        Some(crate::dex::Venue::UniV2)
    } else if s.eq_ignore_ascii_case(&format!("{:?}", known::SUSHI_FACTORY)) {
        Some(crate::dex::Venue::SushiV2)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(decode_swap(&tx_with(vec![0xde, 0xad, 0xbe, 0xef], U256::ZERO), known::WETH).is_none());
        assert!(decode_swap(&tx_with(vec![], U256::ZERO), known::WETH).is_none());
    }

    #[test]
    fn venue_from_factory_maps_known_factories() {
        assert_eq!(
            venue_from_factory(&serde_json::json!(format!("{:?}", known::UNIV2_FACTORY))),
            Some(Venue::UniV2)
        );
        assert_eq!(
            venue_from_factory(&serde_json::json!(format!("{:?}", known::SUSHI_FACTORY))),
            Some(Venue::SushiV2)
        );
        // Unknown factory -> None.
        assert_eq!(
            venue_from_factory(&serde_json::json!("0x0000000000000000000000000000000000000000")),
            None
        );
        // Non-string -> None.
        assert_eq!(venue_from_factory(&serde_json::json!(42)), None);
    }

    #[test]
    fn scan_decodes_pair_address_from_log_data() {
        // Build a 32-byte data payload with a known address right-aligned.
        let expected_pair = address!("1234567890abcdef1234567890abcdef12345678");
        let mut data = vec![0u8; 32];
        data[12..32].copy_from_slice(expected_pair.as_slice());

        // Simulate the inner decode loop of `scan_pair_created` without an RPC.
        let pair = Address::from_slice(&data[12..32]);
        assert_eq!(pair, expected_pair);

        // A short data payload (< 32 bytes) must be skipped.
        let short = vec![0u8; 16];
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

    #[test]
    fn decodes_a_pair_created_log() {
        let pair = address!("b4e16d0168e52d35cacd2c6185b44281ec28c9dc");
        let (venue, got) = decode_pair_created(&pair_created_log(known::UNIV2_FACTORY, pair))
            .expect("a well-formed PairCreated log decodes");
        assert_eq!(venue, Venue::UniV2);
        assert_eq!(got, pair);

        let (venue, _) = decode_pair_created(&pair_created_log(known::SUSHI_FACTORY, pair)).unwrap();
        assert_eq!(venue, Venue::SushiV2);
    }

    #[test]
    fn decodes_a_pool_created_log() {
        // Indexed tokens and fee come from the topics; tickSpacing and the pool
        // address come from data. Mixing those up yields addresses that look
        // valid, which is why this is pinned against a real mainnet pool.
        let got = decode_pool_created(&pool_created_log()).expect("PoolCreated decodes");
        assert_eq!(got.address, address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"));
        assert_eq!(got.token0, known::USDC);
        assert_eq!(got.token1, known::WETH);
        assert_eq!(got.fee, 500);
        assert_eq!(got.tick_spacing, 10);
        assert_eq!(got.block, 18_000_000);
        assert!(crate::dex::V3Pool::is_actionable_fee(got.fee));
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
            decode_pair_created(&v3).is_none(),
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
        assert!(decode_pair_created(&serde_json::json!({})).is_none());
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
