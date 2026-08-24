// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @dev Minimal IERC20 view/transfer surface, declared locally so this mock
///      stays import-free (the Rust simulation fixture deploys it by raw
///      creation bytecode).
interface IERC20Lite {
    function balanceOf(address) external view returns (uint256);
    function transfer(address to, uint256 amount) external returns (bool);
}

/// @dev A faithful-enough UniswapV2 pair for the sniper **simulation fixture**.
///
/// The bot's sniper calldata builder (`sniper/calldata.rs`) encodes the real
/// V2 flash-swap flow: transfer the input token to the pair first, then call
/// `swap(amount0Out, amount1Out, to, data)` with optimistic outputs and a
/// constant-product check on the back side. `MockUniV2Pair` is pull-based and
/// therefore cannot execute that calldata; this pair implements the real
/// shape, including the 0.3% fee in the K check, so the exact bytes the live
/// lane signs are what the simulation runs.
///
/// Test-only. Never deployed to a production chain.
contract SimV2Pair {
    address public immutable token0;
    address public immutable token1;

    uint112 private reserve0;
    uint112 private reserve1;
    uint32 private blockTimestampLast;

    event Sync(uint112 reserve0, uint112 reserve1);

    constructor(address t0, address t1) {
        require(t0 != address(0) && t1 != address(0) && t0 != t1, "SimV2Pair: IDENTICAL");
        (token0, token1) = t0 < t1 ? (t0, t1) : (t1, t0);
    }

    function getReserves() external view returns (uint112, uint112, uint32) {
        return (reserve0, reserve1, blockTimestampLast);
    }

    /// @notice Seed or refresh reserves from the pair's current balances.
    ///         The fixture mints/transfers the two reserves in and calls this.
    function sync() external {
        reserve0 = uint112(IERC20Lite(token0).balanceOf(address(this)));
        reserve1 = uint112(IERC20Lite(token1).balanceOf(address(this)));
        blockTimestampLast = uint32(block.timestamp);
        emit Sync(reserve0, reserve1);
    }

    /// @notice Optimistic-output swap with the UniswapV2 K invariant, including
    ///         the 0.3% fee (1000 - 3), matching the real pair's check.
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata) external {
        require(amount0Out > 0 || amount1Out > 0, "SimV2Pair: INSUFFICIENT_OUTPUT");
        uint256 r0 = reserve0;
        uint256 r1 = reserve1;
        require(amount0Out < r0 && amount1Out < r1, "SimV2Pair: LIQUIDITY");

        if (amount0Out > 0) _safeTransfer(token0, to, amount0Out);
        if (amount1Out > 0) _safeTransfer(token1, to, amount1Out);

        uint256 balance0 = IERC20Lite(token0).balanceOf(address(this));
        uint256 balance1 = IERC20Lite(token1).balanceOf(address(this));

        uint256 amount0In = balance0 > r0 - amount0Out ? balance0 - (r0 - amount0Out) : 0;
        uint256 amount1In = balance1 > r1 - amount1Out ? balance1 - (r1 - amount1Out) : 0;
        require(amount0In > 0 || amount1In > 0, "SimV2Pair: NO_INPUT");

        uint256 adjusted0 = balance0 * 1000 - amount0In * 3;
        uint256 adjusted1 = balance1 * 1000 - amount1In * 3;
        require(adjusted0 * adjusted1 >= r0 * r1 * 1000 * 1000, "SimV2Pair: K");

        reserve0 = uint112(balance0);
        reserve1 = uint112(balance1);
        blockTimestampLast = uint32(block.timestamp);
        emit Sync(uint112(balance0), uint112(balance1));
    }

    function _safeTransfer(address token, address to, uint256 amount) private {
        (bool ok, bytes memory ret) =
            token.call(abi.encodeWithSelector(IERC20Lite.transfer.selector, to, amount));
        require(ok && (ret.length == 0 || abi.decode(ret, (bool))), "SimV2Pair: TRANSFER");
    }
}
