//! Just-in-time (JIT) liquidity on UniswapV3.
//!
//! When a large swap is spotted in the mempool we mint a concentrated position
//! straight in front of it, collect the swap's fee, and burn the position
//! immediately after — all inside one bundle, so the position is exposed for
//! zero blocks.
//!
//! Implementation notes
//! --------------------
//! * We talk to the **pool** directly (`mint`/`burn`/`collect`) rather than the
//!   position manager: pool positions are keyed by `(owner, tickLower,
//!   tickUpper)`, so there is no NFT id to thread from one call to the next.
//!   `MevExecutor.uniswapV3MintCallback` pays what the pool asks for, armed for
//!   exactly one pool by the preceding `armV3Callback` call.
//! * Liquidity sizing uses f64 sqrt-price math. It only needs to be
//!   approximately right — the fork simulation is the arbiter, and the
//!   on-chain profit guard makes a mis-sized position a no-op rather than a
//!   loss.

use alloy_primitives::{Address, U256};
use alloy_sol_types::{sol, SolCall};
use async_trait::async_trait;
use serde_json::json;

use crate::strategies::{StrategyCtx, StrategyImpl};
use crate::types::{now_ms, parse_u256, Call, Opportunity, PendingTx, Strategy};

sol! {
    interface ISwapRouter {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 deadline;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }
        function exactInputSingle(ExactInputSingleParams calldata params) external payable returns (uint256 amountOut);
    }

    interface IUniswapV3Factory {
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address pool);
    }

    interface IUniswapV3PoolActions {
        function mint(address recipient, int24 tickLower, int24 tickUpper, uint128 amount, bytes calldata data)
            external
            returns (uint256 amount0, uint256 amount1);
        function burn(int24 tickLower, int24 tickUpper, uint128 amount)
            external
            returns (uint256 amount0, uint256 amount1);
        function collect(address recipient, int24 tickLower, int24 tickUpper, uint128 amount0Requested, uint128 amount1Requested)
            external
            returns (uint128 amount0, uint128 amount1);
    }

    interface IMevExecutorJit {
        function armV3Callback(address pool) external;
    }
}

/// Only bother with swaps above this notional (in the profit token's wei).
const MIN_VICTIM_NOTIONAL: u128 = 20_000_000_000_000_000_000; // 20 WETH

pub struct JitStrategy;

#[async_trait]
impl StrategyImpl for JitStrategy {
    fn kind(&self) -> Strategy {
        Strategy::Jit
    }

    async fn on_pending(&self, ctx: &StrategyCtx, tx: &PendingTx) -> Vec<Opportunity> {
        if tx.source.backrun_only() {
            return Vec::new();
        }
        let Some(p) = decode_v3_swap(tx) else {
            return Vec::new();
        };
        let weth = ctx.cfg.chain.weth;
        if p.token_in != weth && p.token_out != weth {
            return Vec::new();
        }
        if p.amount_in < U256::from(MIN_VICTIM_NOTIONAL) {
            return Vec::new();
        }

        let head = ctx.head();
        // Read V3 state at the block the victim executed against, not at the
        // head: for a replayed transaction those are different worlds.
        let state_tag = ctx.block_tag(tx.state_block(&head));
        let Ok(Some(state)) = pool_state(ctx, p.token_in, p.token_out, p.fee, &state_tag).await
        else {
            return Vec::new();
        };

        let base_fee = tx.base_fee(&head);
        let target_block = tx.target_block(&head, ctx.cfg.sim.target_block_offset);
        let capital = ctx
            .max_position()
            .min(U256::from(MIN_VICTIM_NOTIONAL) * U256::from(10u8));
        let Some(plan) = size_position(&state, capital, p.zero_for_one) else {
            return Vec::new();
        };

        // Fee captured ≈ victim volume × pool fee × our share of in-range liquidity.
        let fee_num = U256::from(p.fee);
        let volume_fee = p.amount_in * fee_num / U256::from(1_000_000u32);
        let total_liq = state.liquidity + plan.liquidity;
        if total_liq == 0 {
            return Vec::new();
        }
        let share = U256::from(plan.liquidity) * U256::from(1_000_000u32) / U256::from(total_liq);
        let expected_fee = volume_fee * share / U256::from(1_000_000u32);

        // mint + burn + collect + arming ≈ 500k gas.
        let gas_cost = U256::from(500_000u64) * (base_fee + U256::from(1_000_000_000u64));
        if expected_fee <= gas_cost {
            return Vec::new();
        }

        let data = alloy_primitives::Bytes::from(alloy_sol_types::SolValue::abi_encode(&(
            state.token0,
            state.token1,
        )));

        let front = vec![
            Call::new(
                ctx.executor,
                IMevExecutorJit::armV3CallbackCall { pool: state.pool }.abi_encode(),
            ),
            Call::new(
                state.pool,
                IUniswapV3PoolActions::mintCall {
                    recipient: ctx.executor,
                    tickLower: to_i24(plan.tick_lower),
                    tickUpper: to_i24(plan.tick_upper),
                    amount: plan.liquidity,
                    data,
                }
                .abi_encode(),
            ),
        ];

        let back = vec![
            Call::new(
                state.pool,
                IUniswapV3PoolActions::burnCall {
                    tickLower: to_i24(plan.tick_lower),
                    tickUpper: to_i24(plan.tick_upper),
                    amount: plan.liquidity,
                }
                .abi_encode(),
            ),
            Call::new(
                state.pool,
                IUniswapV3PoolActions::collectCall {
                    recipient: ctx.executor,
                    tickLower: to_i24(plan.tick_lower),
                    tickUpper: to_i24(plan.tick_upper),
                    amount0Requested: u128::MAX,
                    amount1Requested: u128::MAX,
                }
                .abi_encode(),
            ),
        ];

        vec![Opportunity {
            id: uuid::Uuid::new_v4().to_string(),
            strategy: Strategy::Jit,
            victim_hashes: vec![tx.hash],
            front_calls: front,
            back_calls: back,
            flash_tokens: Vec::new(),
            flash_amounts: Vec::new(),
            profit_token: weth,
            expected_profit_wei: expected_fee.saturating_sub(gas_cost),
            notional_wei: capital,
            target_block,
            created_at_ms: now_ms(),
            notes: format!(
                "jit {:?} fee {} ticks [{}, {}] L {} victim_in {} expected_fee {}",
                state.pool,
                p.fee,
                plan.tick_lower,
                plan.tick_upper,
                plan.liquidity,
                p.amount_in,
                expected_fee
            ),
        }]
    }
}

#[derive(Debug, Clone, Copy)]
pub struct V3SwapIntent {
    pub token_in: Address,
    pub token_out: Address,
    pub fee: u32,
    pub amount_in: U256,
    /// Victim's `amountOutMinimum`. The V3 sandwich trap scores any
    /// front-run that would push the victim below this as zero profit.
    pub amount_out_min: U256,
    pub zero_for_one: bool,
}

pub fn decode_v3_swap(tx: &PendingTx) -> Option<V3SwapIntent> {
    let data = &tx.input;
    if data.len() < 4 {
        return None;
    }
    let sel: [u8; 4] = [data[0], data[1], data[2], data[3]];

    // SwapRouter02 — no deadline. Selector 0x04e45aaf. This is the
    // majority of current ISwapRouter02 / UniversalRouter-adjacent flow.
    if sel == crate::dex::ISwapRouter02::exactInputSingleCall::SELECTOR {
        let c = crate::dex::ISwapRouter02::exactInputSingleCall::abi_decode(data, false).ok()?;
        let p = c.params;
        return Some(V3SwapIntent {
            token_in: p.tokenIn,
            token_out: p.tokenOut,
            fee: p.fee.to::<u32>(),
            amount_in: if p.amountIn.is_zero() {
                tx.value
            } else {
                p.amountIn
            },
            amount_out_min: p.amountOutMinimum,
            zero_for_one: p.tokenIn < p.tokenOut,
        });
    }

    // Original SwapRouter — includes a deadline. Selector 0x414bf389.
    if sel == ISwapRouter::exactInputSingleCall::SELECTOR {
        let c = ISwapRouter::exactInputSingleCall::abi_decode(data, false).ok()?;
        let p = c.params;
        return Some(V3SwapIntent {
            token_in: p.tokenIn,
            token_out: p.tokenOut,
            fee: p.fee.to::<u32>(),
            amount_in: if p.amountIn.is_zero() {
                tx.value
            } else {
                p.amountIn
            },
            amount_out_min: p.amountOutMinimum,
            zero_for_one: p.tokenIn < p.tokenOut,
        });
    }
    None
}

#[derive(Debug, Clone, Copy)]
pub struct V3State {
    pub pool: Address,
    pub token0: Address,
    pub token1: Address,
    pub sqrt_price_x96: U256,
    pub tick: i32,
    pub tick_spacing: i32,
    pub liquidity: u128,
}

async fn pool_state(
    ctx: &StrategyCtx,
    a: Address,
    b: Address,
    fee: u32,
    block_tag: &str,
) -> anyhow::Result<Option<V3State>> {
    let call = |to: Address, data: Vec<u8>| {
        json!([
            {"to": format!("{to:?}"), "data": format!("0x{}", hex::encode(data))},
            block_tag
        ])
    };

    let Some(v3_factory) = ctx.cfg.addresses.univ3_factory else {
        // No V3 factory on this chain: nothing to look up.
        return Ok(None);
    };
    let pool_raw = ctx
        .rpc
        .call_raw(
            "eth_call",
            call(
                v3_factory,
                IUniswapV3Factory::getPoolCall {
                    tokenA: a,
                    tokenB: b,
                    fee: alloy_primitives::aliases::U24::from(fee),
                }
                .abi_encode(),
            ),
        )
        .await?;
    let bytes = crate::types::parse_bytes(&pool_raw);
    if bytes.len() < 32 {
        return Ok(None);
    }
    let pool = Address::from_slice(&bytes[12..32]);
    if pool == Address::ZERO {
        return Ok(None);
    }

    let results = ctx
        .rpc
        .batch(&[
            (
                "eth_call".into(),
                call(pool, crate::dex::IUniswapV3Pool::slot0Call {}.abi_encode()),
            ),
            (
                "eth_call".into(),
                call(
                    pool,
                    crate::dex::IUniswapV3Pool::liquidityCall {}.abi_encode(),
                ),
            ),
            (
                "eth_call".into(),
                call(
                    pool,
                    crate::dex::IUniswapV3Pool::tickSpacingCall {}.abi_encode(),
                ),
            ),
            (
                "eth_call".into(),
                call(pool, crate::dex::IUniswapV3Pool::token0Call {}.abi_encode()),
            ),
            (
                "eth_call".into(),
                call(pool, crate::dex::IUniswapV3Pool::token1Call {}.abi_encode()),
            ),
        ])
        .await?;

    let slot0 = results
        .first()
        .and_then(|r| r.as_ref().ok())
        .map(crate::types::parse_bytes);
    let Some(slot0) = slot0 else { return Ok(None) };
    if slot0.len() < 64 {
        return Ok(None);
    }
    let sqrt_price_x96 = U256::from_be_slice(&slot0[0..32]);
    let tick = i256_word_to_i32(&slot0[32..64]);

    let liquidity = results
        .get(1)
        .and_then(|r| r.as_ref().ok())
        .map(parse_u256)
        .unwrap_or(U256::ZERO)
        .to::<u128>();
    let tick_spacing = results
        .get(2)
        .and_then(|r| r.as_ref().ok())
        .map(crate::types::parse_bytes)
        .filter(|b| b.len() >= 32)
        .map(|b| i256_word_to_i32(&b[0..32]))
        .unwrap_or(60);
    let token0 = results
        .get(3)
        .and_then(|r| r.as_ref().ok())
        .map(crate::types::parse_bytes)
        .filter(|b| b.len() >= 32)
        .map(|b| Address::from_slice(&b[12..32]))
        .unwrap_or(a);
    let token1 = results
        .get(4)
        .and_then(|r| r.as_ref().ok())
        .map(crate::types::parse_bytes)
        .filter(|b| b.len() >= 32)
        .map(|b| Address::from_slice(&b[12..32]))
        .unwrap_or(b);

    Ok(Some(V3State {
        pool,
        token0,
        token1,
        sqrt_price_x96,
        tick,
        tick_spacing,
        liquidity,
    }))
}

/// Decode a 32-byte two's-complement word into `i32`.
///
/// Ticks are `int24`, so the low four bytes of the sign-extended word already
/// carry the correct `i32` value.
pub fn i256_word_to_i32(word: &[u8]) -> i32 {
    if word.len() < 4 {
        return 0;
    }
    let mut b = [0u8; 4];
    b.copy_from_slice(&word[word.len() - 4..]);
    i32::from_be_bytes(b)
}

/// Pack an `i32` tick into solidity's `int24`.
pub fn to_i24(v: i32) -> alloy_primitives::aliases::I24 {
    let raw = ((v as i64) & 0xff_ffff) as u32;
    alloy_primitives::aliases::I24::from_raw(alloy_primitives::aliases::U24::from(raw))
}

#[derive(Debug, Clone, Copy)]
pub struct JitPlan {
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity: u128,
}

/// Choose a one-tick-spacing range around the current price and the liquidity
/// that consumes at most `capital` of the token we hold.
///
/// `L = amount / (sqrt(Pb) - sqrt(Pa))` for token1, `L = amount / (1/sqrt(Pa) -
/// 1/sqrt(Pb))` for token0 — the standard concentrated-liquidity relations.
pub fn size_position(state: &V3State, capital: U256, zero_for_one: bool) -> Option<JitPlan> {
    let spacing = state.tick_spacing.max(1);
    let centre = (state.tick / spacing) * spacing;
    let lower = centre - spacing;
    let upper = centre + spacing;

    let sqrt_p = |tick: i32| -> f64 { (1.0001f64).powf(tick as f64 / 2.0) };
    let sa = sqrt_p(lower);
    let sb = sqrt_p(upper);
    if sb.partial_cmp(&sa) != Some(std::cmp::Ordering::Greater) {
        return None;
    }

    let cap = capital.min(U256::from(u128::MAX)).to::<u128>() as f64;
    // The victim is pushing the price in a known direction, so provide the token
    // they are buying with (that is the side that earns the fee first).
    let l = if zero_for_one {
        // We supply token1: L = amount1 / (sb - sa)
        cap / (sb - sa)
    } else {
        // We supply token0: L = amount0 / (1/sa - 1/sb)
        cap / ((1.0 / sa) - (1.0 / sb))
    };
    if !l.is_finite() || l <= 0.0 {
        return None;
    }
    let liquidity = l.min(u128::MAX as f64) as u128;
    if liquidity == 0 {
        return None;
    }

    Some(JitPlan {
        tick_lower: lower,
        tick_upper: upper,
        liquidity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::known;

    fn state() -> V3State {
        V3State {
            pool: Address::with_last_byte(3),
            token0: known::USDC,
            token1: known::WETH,
            sqrt_price_x96: U256::from(1u8) << 96,
            tick: 201_450,
            tick_spacing: 60,
            liquidity: 5_000_000_000_000_000_000u128,
        }
    }

    #[test]
    fn range_is_snapped_to_the_tick_spacing() {
        let plan = size_position(&state(), U256::from(10u128.pow(19)), true).unwrap();
        assert_eq!(plan.tick_lower % 60, 0);
        assert_eq!(plan.tick_upper % 60, 0);
        assert_eq!(plan.tick_upper - plan.tick_lower, 120);
        // int24 packing round-trips through two's complement.
        assert_eq!(
            to_i24(-120).into_raw(),
            alloy_primitives::aliases::U24::from(0xff_ff88u32)
        );
    }

    #[test]
    fn liquidity_scales_with_capital() {
        let small = size_position(&state(), U256::from(10u128.pow(18)), true).unwrap();
        let large = size_position(&state(), U256::from(10u128.pow(20)), true).unwrap();
        assert!(large.liquidity > small.liquidity);
    }

    #[test]
    fn decodes_negative_ticks() {
        let mut word = [0xffu8; 32];
        word[31] = 0xfe; // -2
        assert_eq!(i256_word_to_i32(&word), -2);
    }

    fn pending(data: Vec<u8>, value: U256) -> PendingTx {
        use crate::types::{now_ms, TxSource};
        PendingTx {
            hash: alloy_primitives::B256::ZERO,
            from: None,
            to: Some(known::UNIV3_SWAP_ROUTER_02),
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
    fn decodes_swap_router_02_exact_input_single() {
        // Captured-shape fixture: SwapRouter02 has no deadline field.
        // Selector must be 0x04e45aaf — verified against the sol! definition.
        assert_eq!(
            crate::dex::ISwapRouter02::exactInputSingleCall::SELECTOR,
            [0x04, 0xe4, 0x5a, 0xaf]
        );
        let data = crate::dex::ISwapRouter02::exactInputSingleCall {
            params: crate::dex::ISwapRouter02::ExactInputSingleParams {
                tokenIn: known::WETH,
                tokenOut: known::USDC,
                fee: alloy_primitives::aliases::U24::from(500u32),
                recipient: Address::with_last_byte(9),
                amountIn: U256::from(10u128.pow(18)),
                amountOutMinimum: U256::from(1_234u64),
                sqrtPriceLimitX96: alloy_primitives::aliases::U160::ZERO,
            },
        }
        .abi_encode();
        let intent = decode_v3_swap(&pending(data, U256::ZERO)).expect("SwapRouter02 decodes");
        assert_eq!(intent.token_in, known::WETH);
        assert_eq!(intent.token_out, known::USDC);
        assert_eq!(intent.fee, 500);
        assert_eq!(intent.amount_in, U256::from(10u128.pow(18)));
        assert_eq!(intent.amount_out_min, U256::from(1_234u64));
    }

    #[test]
    fn decodes_original_swap_router_exact_input_single() {
        assert_eq!(
            ISwapRouter::exactInputSingleCall::SELECTOR,
            [0x41, 0x4b, 0xf3, 0x89]
        );
        let data = ISwapRouter::exactInputSingleCall {
            params: ISwapRouter::ExactInputSingleParams {
                tokenIn: known::WETH,
                tokenOut: known::USDC,
                fee: alloy_primitives::aliases::U24::from(3_000u32),
                recipient: Address::with_last_byte(9),
                deadline: U256::from(1_900_000_000u64),
                amountIn: U256::from(2u128.pow(18)),
                amountOutMinimum: U256::from(99u64),
                sqrtPriceLimitX96: alloy_primitives::aliases::U160::ZERO,
            },
        }
        .abi_encode();
        let intent = decode_v3_swap(&pending(data, U256::ZERO)).expect("SwapRouter decodes");
        assert_eq!(intent.fee, 3_000);
        assert_eq!(intent.amount_out_min, U256::from(99u64));
        assert_eq!(intent.amount_in, U256::from(2u128.pow(18)));
    }

    #[test]
    fn ignores_unrelated_v3_calldata() {
        assert!(decode_v3_swap(&pending(vec![0xde, 0xad, 0xbe, 0xef], U256::ZERO)).is_none());
        assert!(decode_v3_swap(&pending(vec![], U256::ZERO)).is_none());
    }
}
