//! Mark-to-market source for the new-token sniper lane.
//!
//! Reads pair reserves on chain for each live position, computes raw AMM spot value
//! for held token quantities, and updates position marks with staleness tracking.

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolCall;

use super::portfolio::Mark;
use super::position::Position;
use super::SniperLane;
use crate::rpc::RpcClient;

alloy_sol_types::sol! {
    interface IUniswapV2Pair {
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
        function token0() external view returns (address);
        function token1() external view returns (address);
    }
}

/// Compute raw AMM mark value in WETH wei for `remaining_qty` of `token`.
///
/// V1 uses raw AMM spot price: `(remaining_qty * weth_reserve) / token_reserve`.
/// Fee-on-transfer tokens: the mark ignores the tax; `ExitGuard.minWethOut` is the backstop.
pub fn compute_mark_value(
    weth: Address,
    token: Address,
    reserve0: U256,
    reserve1: U256,
    remaining_qty: U256,
) -> Option<U256> {
    if remaining_qty.is_zero() {
        return Some(U256::ZERO);
    }
    let is_weth_token0 = weth < token;
    let (weth_reserve, token_reserve) = if is_weth_token0 {
        (reserve0, reserve1)
    } else {
        (reserve1, reserve0)
    };

    if token_reserve.is_zero() {
        return None;
    }

    Some((remaining_qty * weth_reserve) / token_reserve)
}

/// Read a pair's reserves. Shared by marking and by exit sizing: a sell's
/// optimistic swap output must be the constant-product output for the input,
/// never the spot mark (which ignores price impact and fails the pair's K
/// invariant).
pub async fn pair_reserves(
    rpc: &RpcClient,
    pair: Address,
    head_block: u64,
) -> Option<(U256, U256)> {
    let call_data = IUniswapV2Pair::getReservesCall {}.abi_encode();
    let bytes = rpc.eth_call(pair, call_data, head_block).await.ok()?;
    let decoded = IUniswapV2Pair::getReservesCall::abi_decode_returns(&bytes, true).ok()?;
    Some((U256::from(decoded.reserve0), U256::from(decoded.reserve1)))
}

/// The UniswapV2 constant-product output (0.3% fee) for `amount_in` against
/// `(reserve_in, reserve_out)` — the exact amount a swap leg may request as
/// its optimistic output without tripping the K invariant.
pub fn v2_amount_out(amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256 {
    if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
        return U256::ZERO;
    }
    let in_with_fee = amount_in * U256::from(997u64);
    (in_with_fee * reserve_out) / (reserve_in * U256::from(1000u64) + in_with_fee)
}

/// Query reserves for a position's pair and update its mark in the lane.
///
/// On RPC read failure: does NOT set a new fresh mark. Stale-mark policy kicks in
/// when missing or > 12 blocks old, suppressing price-based exits while keeping
/// Honeypot, Manual, and Risk stops active.
pub async fn update_position_mark(
    rpc: &RpcClient,
    lane: &SniperLane,
    position: &Position,
    weth: Address,
    head_block: u64,
    now_ms: u64,
) -> Option<Mark> {
    let (reserve0, reserve1) = pair_reserves(rpc, position.pair, head_block).await?;
    let value_wei = compute_mark_value(
        weth,
        position.token,
        reserve0,
        reserve1,
        position.remaining_qty,
    )?;
    let mark = Mark::fresh(value_wei, head_block, now_ms);
    lane.set_mark(&position.id, mark);
    Some(mark)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eth(n: u64) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
    }

    #[test]
    fn fixed_reserves_produce_exact_mark() {
        let weth = Address::repeat_byte(1);
        let token = Address::repeat_byte(2); // weth < token
        let reserve0 = eth(10); // 10 WETH
        let reserve1 = U256::from(1_000_000u64); // 1M tokens (1 ETH = 100k tokens)
        let remaining_qty = U256::from(100_000u64); // 100k tokens

        let mark_wei = compute_mark_value(weth, token, reserve0, reserve1, remaining_qty).unwrap();
        assert_eq!(mark_wei, eth(1));
    }

    #[test]
    fn moving_reserves_produce_moving_mark() {
        let weth = Address::repeat_byte(1);
        let token = Address::repeat_byte(2);
        let remaining_qty = U256::from(100_000u64);

        // Price doubles: WETH reserve rises to 20 ETH
        let mark_wei_1 = compute_mark_value(
            weth,
            token,
            eth(10),
            U256::from(1_000_000u64),
            remaining_qty,
        )
        .unwrap();
        let mark_wei_2 = compute_mark_value(
            weth,
            token,
            eth(20),
            U256::from(1_000_000u64),
            remaining_qty,
        )
        .unwrap();

        assert_eq!(mark_wei_1, eth(1));
        assert_eq!(mark_wei_2, eth(2));
    }

    #[test]
    fn zero_token_reserve_returns_none() {
        let weth = Address::repeat_byte(1);
        let token = Address::repeat_byte(2);
        assert!(compute_mark_value(weth, token, eth(10), U256::ZERO, U256::from(100u64)).is_none());
    }

    #[test]
    fn stale_policy_checks_age_and_freshness() {
        let fresh_mark = Mark::fresh(eth(1), 100, 1000);
        assert!(!fresh_mark.is_stale(100));
        assert!(!fresh_mark.is_stale(112));
        assert!(
            fresh_mark.is_stale(113),
            "mark older than 12 blocks is stale"
        );

        let explicit_stale = Mark::stale(eth(1), 100, 1000);
        assert!(explicit_stale.is_stale(100));
    }
}
