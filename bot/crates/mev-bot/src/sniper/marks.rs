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
    let call_data = IUniswapV2Pair::getReservesCall {}.abi_encode();
    let res = rpc.eth_call(position.pair, call_data, head_block).await;

    match res {
        Ok(bytes) => {
            let Ok(decoded) = IUniswapV2Pair::getReservesCall::abi_decode_returns(&bytes, true)
            else {
                return None;
            };
            let reserve0 = U256::from(decoded.reserve0);
            let reserve1 = U256::from(decoded.reserve1);

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
        Err(_) => None,
    }
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
