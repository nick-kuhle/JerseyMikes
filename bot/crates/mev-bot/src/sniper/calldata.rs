//! SniperVault calldata builder.
//!
//! Encodes `openPosition` and `closePosition` calls for `SniperVault`,
//! building the underlying ERC20 transfer and V2 swap calls plus the required
//! guards and deterministic tags.

use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use alloy_sol_types::{sol, SolCall};

sol! {
    interface ISniperVault {
        struct Call {
            address target;
            uint256 value;
            bytes data;
        }

        struct EntryGuard {
            address token;
            uint256 maxSpend;
            uint256 minTokensOut;
            uint64 blockDeadline;
            uint256 maxBaseFee;
        }

        struct ExitGuard {
            address token;
            uint256 maxTokensIn;
            uint256 minWethOut;
            uint64 blockDeadline;
            uint256 maxBaseFee;
        }

        function openPosition(bytes32 tag, Call[] calldata calls, EntryGuard calldata g)
            external
            returns (uint256 wethSpent, uint256 tokensReceived);

        function closePosition(bytes32 tag, Call[] calldata calls, ExitGuard calldata g)
            external
            returns (uint256 tokensSold, uint256 wethReceived);

        function spendableRemaining() external view returns (uint256);
        function dailyBudget() external view returns (uint256);
        function totalBudget() external view returns (uint256);
        function windowStart() external view returns (uint256);
    }

    interface IERC20 {
        function transfer(address to, uint256 value) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
    }

    interface IUniswapV2Pair {
        function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
        function token0() external view returns (address);
        function token1() external view returns (address);
    }
}

pub use ISniperVault::{Call, EntryGuard, ExitGuard};

/// Derive a deterministic, replayable tag for a (position_id, fill_index) pair.
pub fn make_tag(position_id: &str, fill_index: u32) -> B256 {
    keccak256(format!("{position_id}:{fill_index}").as_bytes())
}

/// Build `openPosition` calldata and guards for acquiring `token` with `size_wei` WETH.
pub fn build_entry(
    vault: Address,
    pair: Address,
    weth: Address,
    token: Address,
    is_weth_token0: bool,
    size_wei: U256,
    expected_tokens_out: U256,
    max_price_impact_bps: u32,
    target_block: u64,
    block_grace: u64,
    max_base_fee: U256,
    tag: B256,
) -> (Vec<Call>, EntryGuard, Vec<u8>) {
    let (amount0_out, amount1_out) = if is_weth_token0 {
        (U256::ZERO, expected_tokens_out)
    } else {
        (expected_tokens_out, U256::ZERO)
    };

    let min_tokens_out = expected_tokens_out
        * U256::from(10_000u64.saturating_sub(max_price_impact_bps as u64))
        / U256::from(10_000u64);

    let calls = vec![
        Call {
            target: weth,
            value: U256::ZERO,
            data: Bytes::from(
                IERC20::transferCall {
                    to: pair,
                    value: size_wei,
                }
                .abi_encode(),
            ),
        },
        Call {
            target: pair,
            value: U256::ZERO,
            data: Bytes::from(
                IUniswapV2Pair::swapCall {
                    amount0Out: amount0_out,
                    amount1Out: amount1_out,
                    to: vault,
                    data: Bytes::new(),
                }
                .abi_encode(),
            ),
        },
    ];

    let guard = EntryGuard {
        token,
        maxSpend: size_wei,
        minTokensOut: min_tokens_out,
        blockDeadline: target_block.saturating_add(block_grace),
        maxBaseFee: max_base_fee,
    };

    let calldata = ISniperVault::openPositionCall {
        tag,
        calls: calls.clone(),
        g: guard.clone(),
    }
    .abi_encode();

    (calls, guard, calldata)
}

/// Build `closePosition` calldata and guards for selling `token_amount` of `token` for WETH.
pub fn build_exit(
    vault: Address,
    pair: Address,
    _weth: Address,
    token: Address,
    is_weth_token0: bool,
    token_amount: U256,
    expected_weth_out: U256,
    slippage_bps: u32,
    target_block: u64,
    block_grace: u64,
    max_base_fee: U256,
    tag: B256,
) -> (Vec<Call>, ExitGuard, Vec<u8>) {
    let (amount0_out, amount1_out) = if is_weth_token0 {
        (expected_weth_out, U256::ZERO)
    } else {
        (U256::ZERO, expected_weth_out)
    };

    let min_weth_out = expected_weth_out
        * U256::from(10_000u64.saturating_sub(slippage_bps as u64))
        / U256::from(10_000u64);

    let calls = vec![
        Call {
            target: token,
            value: U256::ZERO,
            data: Bytes::from(
                IERC20::transferCall {
                    to: pair,
                    value: token_amount,
                }
                .abi_encode(),
            ),
        },
        Call {
            target: pair,
            value: U256::ZERO,
            data: Bytes::from(
                IUniswapV2Pair::swapCall {
                    amount0Out: amount0_out,
                    amount1Out: amount1_out,
                    to: vault,
                    data: Bytes::new(),
                }
                .abi_encode(),
            ),
        },
    ];

    let guard = ExitGuard {
        token,
        maxTokensIn: token_amount,
        minWethOut: min_weth_out,
        blockDeadline: target_block.saturating_add(block_grace),
        maxBaseFee: max_base_fee,
    };

    let calldata = ISniperVault::closePositionCall {
        tag,
        calls: calls.clone(),
        g: guard.clone(),
    }
    .abi_encode();

    (calls, guard, calldata)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eth(n: u64) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u128)
    }

    #[test]
    fn make_tag_is_deterministic_and_unique() {
        let t1 = make_tag("pos1", 0);
        let t2 = make_tag("pos1", 0);
        let t3 = make_tag("pos1", 1);
        let t4 = make_tag("pos2", 0);

        assert_eq!(t1, t2);
        assert_ne!(t1, t3);
        assert_ne!(t1, t4);
    }

    #[test]
    fn entry_calldata_decodes_and_matches_guard_fields() {
        let vault = Address::repeat_byte(1);
        let pair = Address::repeat_byte(2);
        let weth = Address::repeat_byte(3);
        let token = Address::repeat_byte(4);
        let tag = make_tag("pos1", 0);

        let (calls, guard, calldata) = build_entry(
            vault,
            pair,
            weth,
            token,
            true, // weth is token0
            eth(1),
            U256::from(1_000_000u64),
            300, // 3% impact
            100, // target block
            2,   // grace
            U256::from(50_000_000_000u64),
            tag,
        );

        assert_eq!(calls.len(), 2);
        assert_eq!(guard.token, token);
        assert_eq!(guard.maxSpend, eth(1));
        assert_eq!(guard.minTokensOut, U256::from(970_000u64));
        assert_eq!(guard.blockDeadline, 102);
        assert_eq!(guard.maxBaseFee, U256::from(50_000_000_000u64));

        let decoded = ISniperVault::openPositionCall::abi_decode(&calldata, true).unwrap();
        assert_eq!(decoded.tag, tag);
        assert_eq!(decoded.calls.len(), 2);
        assert_eq!(decoded.g.token, token);
        assert_eq!(decoded.g.maxSpend, eth(1));
        assert_eq!(decoded.g.minTokensOut, U256::from(970_000u64));
        assert_eq!(decoded.g.blockDeadline, 102);
    }

    #[test]
    fn exit_calldata_decodes_and_matches_guard_fields() {
        let vault = Address::repeat_byte(1);
        let pair = Address::repeat_byte(2);
        let weth = Address::repeat_byte(3);
        let token = Address::repeat_byte(4);
        let tag = make_tag("pos1", 1);

        let (calls, guard, calldata) = build_exit(
            vault,
            pair,
            weth,
            token,
            false, // weth is token1
            U256::from(500_000u64),
            eth(2),
            500, // 5% slippage
            200,
            2,
            U256::ZERO,
            tag,
        );

        assert_eq!(calls.len(), 2);
        assert_eq!(guard.token, token);
        assert_eq!(guard.maxTokensIn, U256::from(500_000u64));
        assert_eq!(
            guard.minWethOut,
            eth(2) * U256::from(9500) / U256::from(10000)
        );
        assert_eq!(guard.blockDeadline, 202);
        assert_eq!(guard.maxBaseFee, U256::ZERO);

        let decoded = ISniperVault::closePositionCall::abi_decode(&calldata, true).unwrap();
        assert_eq!(decoded.tag, tag);
        assert_eq!(decoded.g.maxTokensIn, U256::from(500_000u64));
        assert_eq!(decoded.g.minWethOut, guard.minWethOut);
    }
}
