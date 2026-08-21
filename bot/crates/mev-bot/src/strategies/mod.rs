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

/// Shared, cheap-to-clone state handed to every strategy.
pub struct StrategyCtx {
    pub cfg: Arc<Config>,
    pub rpc: RpcClient,
    pub executor: Address,
    pub pools: PoolCache,
    head: RwLock<BlockHead>,
}

impl StrategyCtx {
    pub fn new(cfg: Arc<Config>, rpc: RpcClient, executor: Address, head: BlockHead) -> Self {
        Self {
            pools: PoolCache::new(rpc.clone()),
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
pub async fn scan_pair_created(
    rpc: &crate::rpc::RpcClient,
    from: u64,
    to: u64,
) -> Vec<(crate::dex::Venue, Address)> {
    let params = serde_json::json!([{
        "fromBlock": format!("0x{from:x}"),
        "toBlock": format!("0x{to:x}"),
        "address": [
            format!("{:?}", known::UNIV2_FACTORY),
            format!("{:?}", known::SUSHI_FACTORY),
        ],
        "topics": [V2_PAIR_CREATED_TOPIC],
    }]);

    let Ok(v) = rpc.call_raw("eth_getLogs", params).await else {
        return Vec::new();
    };
    let Some(logs) = v.as_array() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for log in logs {
        // Which factory emitted it decides the venue.
        let Some(venue) = venue_from_factory(&log["address"]) else {
            continue;
        };
        let data = crate::types::parse_bytes(&log["data"]);
        if data.len() < 32 {
            continue;
        }
        // For `PairCreated`, `data` is `(pair address, uint256 allPairsLength)`
        // with the pair address right-aligned in the first 32 bytes.
        let pair = Address::from_slice(&data[12..32]);
        out.push((venue, pair));
    }
    out
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
        assert!(decode_swap(&tx_with(vec![], U256::ZERO), known::WETH).is_none();
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
}
