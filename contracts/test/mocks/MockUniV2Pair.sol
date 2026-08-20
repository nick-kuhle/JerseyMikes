// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {MockERC20} from "./MockERC20.sol";

/// @dev Constant-product pool with a configurable fee, mirroring UniswapV2 math so the
///      Rust `dex::v2` module and the Solidity side can be cross-checked in tests.
contract MockUniV2Pair {
    MockERC20 public immutable token0;
    MockERC20 public immutable token1;
    uint256 public reserve0;
    uint256 public reserve1;
    uint256 public immutable feeBps; // e.g. 30 == 0.30%

    constructor(MockERC20 t0, MockERC20 t1, uint256 r0, uint256 r1, uint256 fee) {
        token0 = t0;
        token1 = t1;
        reserve0 = r0;
        reserve1 = r1;
        feeBps = fee;
        t0.mint(address(this), r0);
        t1.mint(address(this), r1);
    }

    function getReserves() external view returns (uint112, uint112, uint32) {
        return (uint112(reserve0), uint112(reserve1), uint32(block.timestamp));
    }

    function getAmountOut(uint256 amountIn, bool zeroForOne) public view returns (uint256) {
        (uint256 rIn, uint256 rOut) = zeroForOne ? (reserve0, reserve1) : (reserve1, reserve0);
        uint256 amountInWithFee = amountIn * (10_000 - feeBps);
        return (amountInWithFee * rOut) / (rIn * 10_000 + amountInWithFee);
    }

    /// @dev Pull-based swap (simpler than the real V2 flash-swap flow, same pricing).
    function swap(uint256 amountIn, bool zeroForOne, address to) external returns (uint256 amountOut) {
        amountOut = getAmountOut(amountIn, zeroForOne);
        if (zeroForOne) {
            token0.transferFrom(msg.sender, address(this), amountIn);
            token1.transfer(to, amountOut);
            reserve0 += amountIn;
            reserve1 -= amountOut;
        } else {
            token1.transferFrom(msg.sender, address(this), amountIn);
            token0.transfer(to, amountOut);
            reserve1 += amountIn;
            reserve0 -= amountOut;
        }
    }
}
