//! AMM math and on-chain pool discovery.
//!
//! Only the maths the strategies actually need:
//!   * constant-product (UniswapV2 / Sushi / forks) exact-in pricing,
//!   * optimal front-run size for a sandwich (ternary search on the exact
//!     integer pricing function — no floating point, no closed-form rounding
//!     surprises),
//!   * optimal input for a two-leg cyclic arbitrage,
//!   * UniswapV3 pricing via the on-chain QuoterV2 (`eth_call`), which is exact
//!     because it runs the real swap logic.

use alloy_primitives::{Address, U256};
use alloy_sol_types::{sol, SolCall};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::rpc::RpcClient;
use crate::types::RouteHop;

/// Router calldata decoders (V2, UniversalRouter).
pub mod calldata;
/// Directed venue edges (V2 adapter + V3 quote-book) for mixed-venue search.
pub mod edge;
/// Multi-leg cycle search over the pool graph.
pub mod graph;

sol! {
    interface IUniswapV2Factory {
        function getPair(address tokenA, address tokenB) external view returns (address pair);
    }

    interface IUniswapV2Pair {
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
        function token0() external view returns (address);
        function token1() external view returns (address);
        function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
    }

    interface IUniswapV2Router {
        function swapExactTokensForTokens(
            uint256 amountIn,
            uint256 amountOutMin,
            address[] calldata path,
            address to,
            uint256 deadline
        ) external returns (uint256[] memory amounts);
        function swapExactETHForTokens(
            uint256 amountOutMin,
            address[] calldata path,
            address to,
            uint256 deadline
        ) external payable returns (uint256[] memory amounts);
        function swapExactTokensForETH(
            uint256 amountIn,
            uint256 amountOutMin,
            address[] calldata path,
            address to,
            uint256 deadline
        ) external returns (uint256[] memory amounts);
        function swapTokensForExactTokens(
            uint256 amountOut,
            uint256 amountInMax,
            address[] calldata path,
            address to,
            uint256 deadline
        ) external returns (uint256[] memory amounts);
    }

    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
        function transfer(address to, uint256 amount) external returns (bool);
        function decimals() external view returns (uint8);
        function symbol() external view returns (string memory);
        function totalSupply() external view returns (uint256);
    }

    interface ISwapRouter02 {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }
        function exactInputSingle(ExactInputSingleParams calldata params) external payable returns (uint256 amountOut);
    }

    interface IQuoterV2 {
        struct QuoteExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            uint24 fee;
            uint160 sqrtPriceLimitX96;
        }
        function quoteExactInputSingle(QuoteExactInputSingleParams memory params)
            external
            returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed, uint256 gasEstimate);
    }

    interface IUniswapV3Pool {
        function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked);
        function liquidity() external view returns (uint128);
        function fee() external view returns (uint24);
        function token0() external view returns (address);
        function token1() external view returns (address);
        function tickSpacing() external view returns (int24);
    }
}

sol! {
    /// Aerodrome (Base, Solidly lineage). Volatile pools are constant-product
    /// WITH A DIFFERENT FEE LOCATION than UniV2 (see
    /// [`aero_volatile_amount_out`]); stable pools use an iterative invariant
    /// that is deliberately out of scope until `DEX_AERODROME_STABLE` work
    /// (work order P4) lands.
    interface IAerodromeFactory {
        function getPool(address tokenA, address tokenB, bool stable) external view returns (address pool);
        function getFee(address pool, bool stable) external view returns (uint256);
        function allPoolsLength() external view returns (uint256);
        function allPools(uint256 index) external view returns (address);
    }

    interface IAerodromePool {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function stable() external view returns (bool);
        function getReserves() external view returns (uint256 reserve0, uint256 reserve1, uint256 blockTimestampLast);
        function getAmountOut(uint256 amountIn, address tokenIn) external view returns (uint256);
    }

    interface IAerodromeRouter {
        struct Route {
            address from;
            address to;
            bool stable;
            address factory;
        }
        function swapExactTokensForTokens(
            uint256 amountIn,
            uint256 amountOutMin,
            Route[] calldata routes,
            address to,
            uint256 deadline
        ) external returns (uint256[] memory amounts);
        function getAmountsOut(uint256 amountIn, Route[] calldata routes) external view returns (uint256[] memory amounts);
        function defaultFactory() external view returns (address);
    }
}

/// `PoolCreated(address indexed token0, address indexed token1, bool indexed stable, address pool, uint256)`
/// — emitted by the Aerodrome (Solidly) pool factory. Three indexed topics
/// (token0, token1, stable); the pool address is the first word of data.
/// keccak256 of the canonical signature: verified with `cast keccak` 2026-08-24.
pub const AERO_POOL_CREATED_TOPIC: &str =
    "0x2128d88d14c80cb081c1252a5acff7a264671bf199ce226b53788fb26065005e";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    UniV2,
    SushiV2,
    UniV3,
    /// Aerodrome volatile (constant-product) pool. Kept venue-distinct from
    /// UniV2 because the fee location differs (off the input, floored) and
    /// execution goes through the Aerodrome router's `Route[]` calldata, not
    /// the pair's `swap()`.
    AeroVolatile,
}

impl Venue {
    pub fn as_str(&self) -> &'static str {
        match self {
            Venue::UniV2 => "univ2",
            Venue::SushiV2 => "sushiv2",
            Venue::UniV3 => "univ3",
            Venue::AeroVolatile => "aerodrome",
        }
    }

    /// Inverse of [`Venue::as_str`] for labels persisted on rows (positions
    /// carry their venue as a string). Unknown labels return `None` — the
    /// caller must refuse to dispatch on a venue it cannot identify.
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "univ2" => Some(Venue::UniV2),
            "sushiv2" => Some(Venue::SushiV2),
            "univ3" => Some(Venue::UniV3),
            "aerodrome" => Some(Venue::AeroVolatile),
            _ => None,
        }
    }
}

/// Snapshot of an Aerodrome pool. Only `stable = false` pools are priced;
/// a stable pool returns `None` from [`AeroPool::amount_out`] — fail closed
/// until the iterative invariant work (work order P4) lands behind
/// `DEX_AERODROME_STABLE`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct AeroPool {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub reserve0: U256,
    pub reserve1: U256,
    /// Per-pool fee in basis points, read from `factory.getFee(pool, stable)`
    /// — Aerodrome fees are per-pool, not a venue constant.
    pub fee_bps: u32,
    /// `pool.stable()` as deployed. Volatile-only until work order P4.
    pub stable: bool,
    pub block: u64,
}

impl AeroPool {
    pub fn venue(&self) -> Venue {
        Venue::AeroVolatile
    }

    pub fn reserves_for(&self, token_in: Address) -> Option<(U256, U256)> {
        if token_in == self.token0 {
            Some((self.reserve0, self.reserve1))
        } else if token_in == self.token1 {
            Some((self.reserve1, self.reserve0))
        } else {
            None
        }
    }

    pub fn other_token(&self, token: Address) -> Option<Address> {
        if token == self.token0 {
            Some(self.token1)
        } else if token == self.token1 {
            Some(self.token0)
        } else {
            None
        }
    }

    /// Exact-in quote using the pool's own per-pool fee. Stable pools refuse
    /// to quote: the x³y+y³x invariant is not approximated by volatile math,
    /// and an approximate quote upstream is an invented arbitrage downstream.
    pub fn amount_out(&self, token_in: Address, amount_in: U256) -> Option<U256> {
        if self.stable {
            return None;
        }
        let (r_in, r_out) = self.reserves_for(token_in)?;
        let out = aero_volatile_amount_out(amount_in, r_in, r_out, self.fee_bps);
        if out.is_zero() {
            None
        } else {
            Some(out)
        }
    }

    /// The pool state after a swap of `amount_in` of `token_in` — the
    /// preconfirmation back-run prices against this, never against the
    /// pre-victim state. Stable pools refuse (same rule as
    /// [`AeroPool::amount_out`]).
    pub fn with_swap(&self, token_in: Address, amount_in: U256) -> Option<(AeroPool, U256)> {
        let out = self.amount_out(token_in, amount_in)?;
        let mut p = *self;
        if token_in == self.token0 {
            p.reserve0 += amount_in;
            p.reserve1 = p.reserve1.saturating_sub(out);
        } else {
            p.reserve1 += amount_in;
            p.reserve0 = p.reserve0.saturating_sub(out);
        }
        Some((p, out))
    }
}

/// Aerodrome volatile `getAmountOut`, exactly as the contract computes it.
///
/// **Not** the UniV2 formula: the fee is taken OFF THE INPUT with integer
/// truncation first (`xin = x - floor(x·fee/10_000)`), then the constant
/// product is applied to whole units (`xin·rOut / (rIn + xin)`). At small
/// sizes this differs from UniV2's in-numerator fee by wei — which is why
/// Aerodrome is its own venue, not a renamed V2 pool. Verified against live
/// Base `getAmountOut` 2026-08-24 (1 WETH → 2500574490 USDC on the WETH/USDC
/// volatile pool, matching the pool call and the router call exactly).
pub fn aero_volatile_amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: u32,
) -> U256 {
    if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
        return U256::ZERO;
    }
    let fee = amount_in * U256::from(fee_bps) / U256::from(10_000u32);
    let xin = amount_in.saturating_sub(fee);
    if xin.is_zero() {
        return U256::ZERO;
    }
    let denominator = reserve_in + xin;
    if denominator.is_zero() {
        return U256::ZERO;
    }
    xin * reserve_out / denominator
}

/// Immutable metadata for a UniswapV3 pool.
///
/// There is no reserve pair here on purpose: concentrated liquidity has no
/// single "reserve" and any attempt to price this with `v2_amount_out` is
/// wrong. Exact-in pricing goes through [`quote_v3`] (the on-chain QuoterV2),
/// and live state (`slot0`, `liquidity`) is read on demand.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct V3Pool {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    /// Fee in hundredths of a bip (500 == 0.05%, 3000 == 0.30%).
    pub fee: u32,
    pub tick_spacing: i32,
    /// Block in which the factory created the pool.
    pub block: u64,
}

impl V3Pool {
    pub fn other_token(&self, token: Address) -> Option<Address> {
        if token == self.token0 {
            Some(self.token1)
        } else if token == self.token1 {
            Some(self.token0)
        } else {
            None
        }
    }

    /// Fee tiers the bot is willing to act on. The 1 bp tier is skipped: it is
    /// almost entirely stable-stable pairs where our strategies have no edge.
    pub fn is_actionable_fee(fee: u32) -> bool {
        matches!(fee, 500 | 3_000 | 10_000)
    }
}

/// Snapshot of a constant-product pool.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct V2Pool {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub reserve0: U256,
    pub reserve1: U256,
    /// Fee in basis points (30 == 0.30%).
    pub fee_bps: u32,
    pub venue: Venue,
    pub block: u64,
}

impl V2Pool {
    pub fn reserves_for(&self, token_in: Address) -> Option<(U256, U256)> {
        if token_in == self.token0 {
            Some((self.reserve0, self.reserve1))
        } else if token_in == self.token1 {
            Some((self.reserve1, self.reserve0))
        } else {
            None
        }
    }

    pub fn other_token(&self, token: Address) -> Option<Address> {
        if token == self.token0 {
            Some(self.token1)
        } else if token == self.token1 {
            Some(self.token0)
        } else {
            None
        }
    }

    /// Exact-in quote using the pool's own fee.
    pub fn amount_out(&self, token_in: Address, amount_in: U256) -> Option<U256> {
        let (r_in, r_out) = self.reserves_for(token_in)?;
        Some(v2_amount_out(amount_in, r_in, r_out, self.fee_bps))
    }

    /// Apply a swap to a *copy* of the pool, returning the post-trade state.
    /// Used to model the victim transaction's price impact.
    pub fn with_swap(&self, token_in: Address, amount_in: U256) -> Option<(V2Pool, U256)> {
        let out = self.amount_out(token_in, amount_in)?;
        let mut p = *self;
        if token_in == self.token0 {
            p.reserve0 += amount_in;
            p.reserve1 = p.reserve1.saturating_sub(out);
        } else {
            p.reserve1 += amount_in;
            p.reserve0 = p.reserve0.saturating_sub(out);
        }
        Some((p, out))
    }

    /// Mid price of `token` denominated in the other token, scaled by 1e18.
    pub fn price_1e18(&self, token: Address) -> Option<U256> {
        let (r_in, r_out) = self.reserves_for(token)?;
        if r_in.is_zero() {
            return None;
        }
        Some(r_out * U256::from(1_000_000_000_000_000_000u128) / r_in)
    }
}

/// UniswapV2 `getAmountOut`, exactly as implemented on chain.
pub fn v2_amount_out(amount_in: U256, reserve_in: U256, reserve_out: U256, fee_bps: u32) -> U256 {
    if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
        return U256::ZERO;
    }
    let fee_num = U256::from(10_000u32 - fee_bps);
    let amount_in_with_fee = amount_in * fee_num;
    let numerator = amount_in_with_fee * reserve_out;
    let denominator = reserve_in * U256::from(10_000u32) + amount_in_with_fee;
    if denominator.is_zero() {
        return U256::ZERO;
    }
    numerator / denominator
}

/// UniswapV2 `getAmountIn`.
pub fn v2_amount_in(amount_out: U256, reserve_in: U256, reserve_out: U256, fee_bps: u32) -> U256 {
    if amount_out.is_zero() || amount_out >= reserve_out {
        return U256::MAX;
    }
    let numerator = reserve_in * amount_out * U256::from(10_000u32);
    let denominator = (reserve_out - amount_out) * U256::from(10_000u32 - fee_bps);
    if denominator.is_zero() {
        return U256::MAX;
    }
    numerator / denominator + U256::from(1u8)
}

/// Maximise `f` over `[lo, hi]` assuming it is unimodal (true for all the AMM
/// profit curves we deal with). Integer ternary search: ~120 iterations worst
/// case, each one a couple of 256-bit multiplications.
pub fn ternary_search_max<F>(lo: U256, hi: U256, f: F) -> (U256, U256)
where
    F: Fn(U256) -> U256,
{
    let mut lo = lo;
    let mut hi = hi;
    let three = U256::from(3u8);
    let mut guard = 0u32;
    while hi > lo + U256::from(2u8) && guard < 256 {
        guard += 1;
        let span = hi - lo;
        let m1 = lo + span / three;
        let m2 = hi - span / three;
        if f(m1) < f(m2) {
            lo = m1 + U256::from(1u8);
        } else {
            hi = m2;
        }
    }
    let mut best = lo;
    let mut best_v = f(lo);
    let mut x = lo;
    while x <= hi {
        let v = f(x);
        if v > best_v {
            best_v = v;
            best = x;
        }
        x += U256::from(1u8);
    }
    (best, best_v)
}

/// Optimal front-run size for a sandwich on a constant-product pool.
///
/// The searcher buys `x` of `token_out` before the victim, the victim buys with
/// `victim_amount_in`, then the searcher sells everything back. Profit is
/// measured in `token_in` (usually WETH), and is unimodal in `x`.
///
/// `max_in` caps the search at the capital the bot is willing to risk.
pub fn optimal_sandwich_in(
    pool: &V2Pool,
    token_in: Address,
    victim_amount_in: U256,
    victim_min_out: U256,
    max_in: U256,
) -> Option<SandwichSizing> {
    let (r_in, r_out) = pool.reserves_for(token_in)?;
    if r_in.is_zero() || r_out.is_zero() || victim_amount_in.is_zero() {
        return None;
    }
    let fee = pool.fee_bps;
    // Cap at the pool's own depth; beyond that the curve is meaningless.
    let hi = max_in.min(r_in);

    let profit_of = |x: U256| -> U256 {
        if x.is_zero() {
            return U256::ZERO;
        }
        // Leg 1: we buy token_out with x.
        let out1 = v2_amount_out(x, r_in, r_out, fee);
        if out1.is_zero() || out1 >= r_out {
            return U256::ZERO;
        }
        let (r_in2, r_out2) = (r_in + x, r_out - out1);

        // Leg 2: victim buys at the worsened price. If the victim's slippage
        // limit would be violated their transaction reverts and we are left
        // holding the bag, so that `x` is worthless to us.
        let victim_out = v2_amount_out(victim_amount_in, r_in2, r_out2, fee);
        if victim_out.is_zero() || victim_out >= r_out2 {
            return U256::ZERO;
        }
        if !victim_min_out.is_zero() && victim_out < victim_min_out {
            return U256::ZERO;
        }
        let (r_in3, r_out3) = (r_in2 + victim_amount_in, r_out2 - victim_out);

        // Leg 3: we sell out1 back.
        let back = v2_amount_out(out1, r_out3, r_in3, fee);
        back.saturating_sub(x)
    };

    let (best_x, best_profit) = ternary_search_max(U256::ZERO, hi, profit_of);
    if best_profit.is_zero() || best_x.is_zero() {
        return None;
    }

    let front_out = v2_amount_out(best_x, r_in, r_out, fee);
    let (r_in2, r_out2) = (r_in + best_x, r_out - front_out);
    let victim_out = v2_amount_out(victim_amount_in, r_in2, r_out2, fee);
    let (r_in3, r_out3) = (r_in2 + victim_amount_in, r_out2 - victim_out);
    let back_out = v2_amount_out(front_out, r_out3, r_in3, fee);

    Some(SandwichSizing {
        amount_in: best_x,
        front_out,
        back_out,
        gross_profit: best_profit,
        victim_out,
    })
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct SandwichSizing {
    /// How much `token_in` the front-run spends.
    pub amount_in: U256,
    /// How much `token_out` the front-run receives.
    pub front_out: U256,
    /// How much `token_in` the back-run returns.
    pub back_out: U256,
    /// `back_out - amount_in`, before gas.
    pub gross_profit: U256,
    /// What the victim ends up receiving (used to check their slippage bound).
    pub victim_out: U256,
}

/// Optimal input for a two-pool cyclic arbitrage: buy `token_mid` on `a`, sell
/// it on `b`, ending back in `token_in`. Returns `(amount_in, profit)`.
pub fn optimal_two_leg_arb(
    a: &V2Pool,
    b: &V2Pool,
    token_in: Address,
    max_in: U256,
) -> Option<(U256, U256)> {
    let token_mid = a.other_token(token_in)?;
    if b.other_token(token_mid)? != token_in {
        return None;
    }
    let (ar_in, ar_out) = a.reserves_for(token_in)?;
    let (br_in, br_out) = b.reserves_for(token_mid)?;
    if ar_in.is_zero() || ar_out.is_zero() || br_in.is_zero() || br_out.is_zero() {
        return None;
    }

    let profit_of = |x: U256| -> U256 {
        let mid = v2_amount_out(x, ar_in, ar_out, a.fee_bps);
        if mid.is_zero() {
            return U256::ZERO;
        }
        let back = v2_amount_out(mid, br_in, br_out, b.fee_bps);
        back.saturating_sub(x)
    };

    let hi = max_in.min(ar_in);
    let (x, p) = ternary_search_max(U256::ZERO, hi, profit_of);
    if p.is_zero() || x.is_zero() {
        None
    } else {
        Some((x, p))
    }
}

// ---------------------------------------------------------------------------
// On-chain reads
// ---------------------------------------------------------------------------

fn eth_call_params(to: Address, data: Vec<u8>, block: &str) -> serde_json::Value {
    json!([
        {"to": format!("{to:?}"), "data": format!("0x{}", hex::encode(data))},
        block
    ])
}

/// Fetch a V2 pool snapshot (token0/token1/reserves) in a single batch request.
pub async fn fetch_v2_pool(
    rpc: &RpcClient,
    pair: Address,
    venue: Venue,
    fee_bps: u32,
    block: u64,
) -> Result<V2Pool> {
    let tag = format!("0x{block:x}");
    let calls = vec![
        (
            "eth_call".to_string(),
            eth_call_params(pair, IUniswapV2Pair::token0Call {}.abi_encode(), &tag),
        ),
        (
            "eth_call".to_string(),
            eth_call_params(pair, IUniswapV2Pair::token1Call {}.abi_encode(), &tag),
        ),
        (
            "eth_call".to_string(),
            eth_call_params(pair, IUniswapV2Pair::getReservesCall {}.abi_encode(), &tag),
        ),
    ];
    let res = rpc.batch(&calls).await?;
    let token0 = decode_address(res.first())?;
    let token1 = decode_address(res.get(1))?;
    let reserves = res
        .get(2)
        .and_then(|r| r.as_ref().ok())
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("getReserves failed for {pair:?}"))?;
    let raw = hex::decode(reserves.strip_prefix("0x").unwrap_or(reserves))?;
    if raw.len() < 64 {
        return Err(anyhow!("short getReserves response for {pair:?}"));
    }
    let reserve0 = U256::from_be_slice(&raw[0..32]);
    let reserve1 = U256::from_be_slice(&raw[32..64]);

    Ok(V2Pool {
        address: pair,
        token0,
        token1,
        reserve0,
        reserve1,
        fee_bps,
        venue,
        block,
    })
}

fn decode_address(v: Option<&Result<serde_json::Value>>) -> Result<Address> {
    let s = v
        .and_then(|r| r.as_ref().ok())
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("expected address response"))?;
    let raw = hex::decode(s.strip_prefix("0x").unwrap_or(s))?;
    if raw.len() < 32 {
        return Err(anyhow!("short address response"));
    }
    Ok(Address::from_slice(&raw[12..32]))
}

/// Resolve the canonical V2 pair for a token couple.
pub async fn get_pair(
    rpc: &RpcClient,
    factory: Address,
    a: Address,
    b: Address,
) -> Result<Option<Address>> {
    let data = IUniswapV2Factory::getPairCall {
        tokenA: a,
        tokenB: b,
    }
    .abi_encode();
    let out: String = rpc
        .call("eth_call", eth_call_params(factory, data, "latest"))
        .await?;
    let raw = hex::decode(out.strip_prefix("0x").unwrap_or(&out))?;
    if raw.len() < 32 {
        return Ok(None);
    }
    let addr = Address::from_slice(&raw[12..32]);
    Ok(if addr == Address::ZERO {
        None
    } else {
        Some(addr)
    })
}

/// Resolve an Aerodrome pool address: `factory.getPool(a, b, stable)`.
pub async fn aero_get_pool(
    rpc: &RpcClient,
    factory: Address,
    a: Address,
    b: Address,
    stable: bool,
) -> Result<Option<Address>> {
    let data = IAerodromeFactory::getPoolCall {
        tokenA: a,
        tokenB: b,
        stable,
    }
    .abi_encode();
    let out: String = rpc
        .call("eth_call", eth_call_params(factory, data, "latest"))
        .await?;
    let raw = hex::decode(out.strip_prefix("0x").unwrap_or(&out))?;
    if raw.len() < 32 {
        return Ok(None);
    }
    let addr = Address::from_slice(&raw[12..32]);
    Ok(if addr == Address::ZERO {
        None
    } else {
        Some(addr)
    })
}

/// Per-pool fee in basis points: `factory.getFee(pool, stable)`.
pub async fn aero_fee_bps(
    rpc: &RpcClient,
    factory: Address,
    pool: Address,
    stable: bool,
    block: u64,
) -> Result<u32> {
    let data = IAerodromeFactory::getFeeCall { pool, stable }.abi_encode();
    let out: String = rpc
        .call(
            "eth_call",
            eth_call_params(factory, data, &format!("0x{block:x}")),
        )
        .await?;
    let raw = hex::decode(out.strip_prefix("0x").unwrap_or(&out))?;
    if raw.len() < 32 {
        return Err(anyhow!("getFee failed for pool {pool:?}"));
    }
    let fee = U256::from_be_slice(&raw[0..32]).to::<u64>();
    Ok(u32::try_from(fee).unwrap_or(u32::MAX))
}

/// Fetch an Aerodrome pool snapshot: tokens, reserves, stability flag and
/// the per-pool fee, in one batch plus one call. Stable pools are returned
/// with `stable = true` — pricing refuses them until work order P4.
pub async fn fetch_aero_pool(
    rpc: &RpcClient,
    factory: Address,
    pool: Address,
    block: u64,
) -> Result<AeroPool> {
    let tag = format!("0x{block:x}");
    let calls = vec![
        (
            "eth_call".to_string(),
            eth_call_params(pool, IAerodromePool::token0Call {}.abi_encode(), &tag),
        ),
        (
            "eth_call".to_string(),
            eth_call_params(pool, IAerodromePool::token1Call {}.abi_encode(), &tag),
        ),
        (
            "eth_call".to_string(),
            eth_call_params(pool, IAerodromePool::getReservesCall {}.abi_encode(), &tag),
        ),
        (
            "eth_call".to_string(),
            eth_call_params(pool, IAerodromePool::stableCall {}.abi_encode(), &tag),
        ),
    ];
    let res = rpc.batch(&calls).await?;
    let token0 = decode_address(res.first())?;
    let token1 = decode_address(res.get(1))?;
    let reserves = res
        .get(2)
        .and_then(|r| r.as_ref().ok())
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("getReserves failed for {pool:?}"))?;
    let raw = hex::decode(reserves.strip_prefix("0x").unwrap_or(reserves))?;
    if raw.len() < 64 {
        return Err(anyhow!("short getReserves response for {pool:?}"));
    }
    let reserve0 = U256::from_be_slice(&raw[0..32]);
    let reserve1 = U256::from_be_slice(&raw[32..64]);
    let stable_raw = res
        .get(3)
        .and_then(|r| r.as_ref().ok())
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("stable() failed for {pool:?}"))?;
    let stable = stable_raw.ends_with('1');
    let fee_bps = aero_fee_bps(rpc, factory, pool, stable, block).await?;

    Ok(AeroPool {
        address: pool,
        token0,
        token1,
        reserve0,
        reserve1,
        fee_bps,
        stable,
        block,
    })
}

/// Price an exact-in UniswapV3 swap using the on-chain QuoterV2.
///
/// `block` is a JSON-RPC block tag (`"latest"` or `"0x…"`). Replay must pin
/// this to the victim's parent; quoting `"latest"` for a mined transaction
/// is the same state-divergence bug the rest of the pending path already
/// avoids.
pub async fn quote_v3(
    rpc: &RpcClient,
    quoter: Address,
    token_in: Address,
    token_out: Address,
    fee: u32,
    amount_in: U256,
    block: &str,
) -> Result<U256> {
    let data = IQuoterV2::quoteExactInputSingleCall {
        params: IQuoterV2::QuoteExactInputSingleParams {
            tokenIn: token_in,
            tokenOut: token_out,
            amountIn: amount_in,
            fee: alloy_primitives::aliases::U24::from(fee),
            sqrtPriceLimitX96: alloy_primitives::aliases::U160::ZERO,
        },
    }
    .abi_encode();
    let out: String = rpc
        .call("eth_call", eth_call_params(quoter, data, block))
        .await?;
    let raw = hex::decode(out.strip_prefix("0x").unwrap_or(&out))?;
    if raw.len() < 32 {
        return Ok(U256::ZERO);
    }
    Ok(U256::from_be_slice(&raw[0..32]))
}

/// UniswapV3 fee (hundredths of a bip) → V2-style basis points.
/// 500 → 5, 3000 → 30, 10000 → 100.
pub fn v3_fee_to_bps(fee: u32) -> u32 {
    fee / 100
}

/// Build the [`RouteHop`] identity for one hop through a V2 pool.
pub fn hop_for_v2(pool: &V2Pool, token_in: Address) -> RouteHop {
    RouteHop {
        venue: pool.venue,
        pool: pool.address,
        token_in,
        fee_bps: pool.fee_bps,
    }
}

/// Build the [`RouteHop`] identity for one hop through an Aerodrome volatile
/// pool.
pub fn hop_for_aero(pool: &AeroPool, token_in: Address) -> RouteHop {
    RouteHop {
        venue: Venue::AeroVolatile,
        pool: pool.address,
        token_in,
        fee_bps: pool.fee_bps,
    }
}

/// The pool fetch + re-quote the WS-R state-comparison producer runs at the
/// canonical sealed block for one hop. V3's QuoterV2 path is deliberately
/// not re-quoted here: its quotes were captured off-chain at sizing time and
/// cannot be re-derived from 3 static calls.
pub async fn requote_hop(
    rpc: &RpcClient,
    hop: &RouteHop,
    anchor_factory: Option<Address>,
    amount_in: U256,
    target_block: u64,
) -> Result<Option<U256>> {
    match hop.venue {
        Venue::AeroVolatile => {
            let pool = fetch_aero_pool(
                rpc,
                anchor_factory.context("aerodrome hop without factory")?,
                hop.pool,
                target_block,
            )
            .await?;
            Ok(pool.amount_out(hop.token_in, amount_in))
        }
        Venue::UniV3 => Ok(None),
        _ => {
            let pool = fetch_v2_pool(rpc, hop.pool, hop.venue, hop.fee_bps, target_block).await?;
            Ok(pool.amount_out(hop.token_in, amount_in))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(r0: u128, r1: u128) -> V2Pool {
        V2Pool {
            address: Address::ZERO,
            token0: Address::with_last_byte(1),
            token1: Address::with_last_byte(2),
            reserve0: U256::from(r0),
            reserve1: U256::from(r1),
            fee_bps: 30,
            venue: Venue::UniV2,
            block: 0,
        }
    }

    #[test]
    fn aero_volatile_math_matches_live_chain_values() {
        // Pinned from live Base 2026-08-24: WETH/USDC volatile pool
        // 0xcDAC0d6c6C59727a65F871236188350531885C43 — reserves
        // 1718659049920153369322 WETH / 4313067253337 USDC, fee 30 bps;
        // pool.getAmountOut(1e18, WETH) == router.getAmountsOut == 2500574490.
        let x = U256::from(10u128.pow(18));
        let r0 = U256::from(1718659049920153369322u128);
        let r1 = U256::from(4313067253337u128);
        assert_eq!(
            aero_volatile_amount_out(x, r0, r1, 30),
            U256::from(2500574490u64)
        );
    }

    #[test]
    fn aero_volatile_fee_location_differs_from_univ2_at_small_sizes() {
        // Under ~333 wei of input at 30 bps, floor(x·fee/10_000) is zero, so
        // Aerodrome charges NOTHING where UniV2's in-numerator fee always
        // charges. The venues are deliberately not interchangeable.
        let x = U256::from(100u64);
        let r_in = U256::from(1_000_000u64);
        let r_out = U256::from(1_000_000u64);
        let aero = aero_volatile_amount_out(x, r_in, r_out, 30);
        let uni = v2_amount_out(x, r_in, r_out, 30);
        assert_eq!(aero, U256::from(99u64)); // (100·1e6)/(1e6+100)
        assert_eq!(uni, U256::from(99u64).saturating_sub(U256::from(0u64))); // same rounding here…
                                                                             // …but at a size where the floor bites, they diverge:
        let x = U256::from(333u64);
        let aero = aero_volatile_amount_out(x, r_in, r_out, 30);
        let uni = v2_amount_out(x, r_in, r_out, 30);
        assert_ne!(aero, uni, "fee location changes integer results");
    }

    #[test]
    fn aero_pool_refuses_to_price_stable_pools() {
        let p = AeroPool {
            address: Address::with_last_byte(7),
            token0: Address::with_last_byte(1),
            token1: Address::with_last_byte(2),
            reserve0: U256::from(1_000_000u64),
            reserve1: U256::from(1_000_000u64),
            fee_bps: 5,
            stable: true,
            block: 0,
        };
        assert_eq!(p.amount_out(p.token0, U256::from(1_000u64)), None);
    }

    #[test]
    fn amount_out_matches_solidity() {
        // 1000 in, reserves 1e6/1e6, 30bps -> floor(997000*1e6 / (1e6*1000 + 997000))
        let got = v2_amount_out(
            U256::from(1000u64),
            U256::from(1_000_000u64),
            U256::from(1_000_000u64),
            30,
        );
        assert_eq!(got, U256::from(996u64));
    }

    #[test]
    fn round_trip_loses_fees() {
        let p = pool(1_000_000e18 as u128, 1_000_000e18 as u128);
        let a = U256::from(10_000u128) * U256::from(10u128.pow(18));
        let mid = p.amount_out(p.token0, a).unwrap();
        let back = v2_amount_out(mid, p.reserve1 - mid, p.reserve0 + a, 30);
        assert!(back < a, "round trip must lose the fee");
    }

    #[test]
    fn sandwich_is_profitable_on_big_victim_trades() {
        let p = pool(1_000_000e18 as u128, 1_000_000e18 as u128);
        let victim = U256::from(50_000u128) * U256::from(10u128.pow(18));
        let max_in = U256::from(500_000u128) * U256::from(10u128.pow(18));
        let s = optimal_sandwich_in(&p, p.token0, victim, U256::ZERO, max_in)
            .expect("should find a sandwich");
        assert!(s.gross_profit > U256::ZERO);
        assert!(s.amount_in > U256::ZERO);
        // Profit must beat any naive fixed size.
        let naive = optimal_sandwich_in(
            &p,
            p.token0,
            victim,
            U256::ZERO,
            s.amount_in / U256::from(4u8),
        )
        .map(|x| x.gross_profit)
        .unwrap_or(U256::ZERO);
        assert!(s.gross_profit >= naive);
    }

    #[test]
    fn sandwich_respects_victim_slippage_bound() {
        let p = pool(1_000_000e18 as u128, 1_000_000e18 as u128);
        let victim = U256::from(50_000u128) * U256::from(10u128.pow(18));
        // Victim demands essentially the unsandwiched output: no room for us.
        let strict_min = p.amount_out(p.token0, victim).unwrap();
        let res = optimal_sandwich_in(
            &p,
            p.token0,
            victim,
            strict_min,
            U256::from(500_000u128) * U256::from(10u128.pow(18)),
        );
        assert!(res.is_none(), "must not sandwich a zero-slippage victim");
    }

    #[test]
    fn two_leg_arb_found_only_when_pools_disagree() {
        let mut a = pool(1_000_000e18 as u128, 2_000_000e18 as u128);
        let mut b = pool(1_000_000e18 as u128, 1_000_000e18 as u128);
        a.address = Address::with_last_byte(9);
        b.address = Address::with_last_byte(10);
        let max = U256::from(100_000u128) * U256::from(10u128.pow(18));
        let found = optimal_two_leg_arb(&a, &b, a.token0, max);
        assert!(found.is_some());

        let flat_a = pool(1_000_000e18 as u128, 1_000_000e18 as u128);
        let flat_b = pool(1_000_000e18 as u128, 1_000_000e18 as u128);
        assert!(optimal_two_leg_arb(&flat_a, &flat_b, flat_a.token0, max).is_none());
    }

    #[test]
    fn v3_fee_converts_to_v2_bps() {
        assert_eq!(v3_fee_to_bps(500), 5);
        assert_eq!(v3_fee_to_bps(3_000), 30);
        assert_eq!(v3_fee_to_bps(10_000), 100);
    }

    #[test]
    fn ternary_search_finds_the_peak() {
        // -(x-1000)^2 shifted positive
        let f = |x: U256| -> U256 {
            let x = x.to::<u128>() as i128;
            let v = 10_000_000i128 - (x - 1000).pow(2);
            U256::from(v.max(0) as u128)
        };
        let (x, _) = ternary_search_max(U256::ZERO, U256::from(5000u64), f);
        assert!(x.to::<u128>().abs_diff(1000) <= 2);
    }
}
