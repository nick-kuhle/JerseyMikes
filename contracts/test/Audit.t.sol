// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {MevExecutor} from "../src/MevExecutor.sol";
import {MockERC20} from "./mocks/MockERC20.sol";
import {MockWETH} from "./mocks/MockWETH.sol";
import {MockBalancerVault} from "./mocks/MockBalancerVault.sol";

/// Probes for behaviours a reviewer would want pinned. These are diagnostics,
/// not proposed changes.
contract AuditTest is Test {
    MevExecutor exec;
    MockWETH weth;
    MockERC20 usdc;
    MockBalancerVault vault;
    address searcher = address(0xBEEF);

    function setUp() public {
        weth = new MockWETH();
        usdc = new MockERC20("USD Coin", "USDC");
        vault = new MockBalancerVault();
        exec = new MevExecutor(address(vault), address(weth));
        exec.setSearcher(searcher, true);
        vm.deal(address(exec), 10 ether);
    }

    function _g(address t, uint256 m) internal pure returns (MevExecutor.Guard memory) {
        return MevExecutor.Guard({profitToken: t, minProfit: m, bribeBps: 0, blockDeadline: 0, maxBaseFee: 0});
    }

    /// Can a non-owner, non-searcher arm the V3 callback? (should be no)
    function test_armV3CallbackIsSelfOnly() public {
        vm.prank(searcher);
        vm.expectRevert(MevExecutor.NotSearcher.selector);
        exec.armV3Callback(address(0x1234));
    }

    /// Can anyone drive the mint callback without arming? (should be no)
    function test_v3MintCallbackRequiresArming() public {
        vm.prank(address(0xDEAD));
        vm.expectRevert(MevExecutor.BadFlashCallback.selector);
        exec.uniswapV3MintCallback(1, 1, abi.encode(address(usdc), address(weth)));
    }

    /// Can an outsider fake a flash-loan callback? (should be no)
    function test_receiveFlashLoanRejectsUnarmed() public {
        address[] memory t = new address[](0);
        uint256[] memory a = new uint256[](0);
        vm.prank(address(vault));
        vm.expectRevert(MevExecutor.BadFlashCallback.selector);
        exec.receiveFlashLoan(t, a, a, "");
    }

    /// bribeBps > 10000 must be rejected.
    function test_bribeBpsUpperBound() public {
        MevExecutor.Call[] memory calls = new MevExecutor.Call[](0);
        MevExecutor.Guard memory g = _g(address(0), 0);
        g.bribeBps = 10_001;
        vm.prank(searcher);
        vm.expectRevert(MevExecutor.BadBribe.selector);
        exec.execute(bytes32(0), calls, g);
    }

    /// quote() must be unreachable from a real sender — including a searcher.
    /// It runs an unguarded batch, so on-chain reachability would be a hole.
    function test_quoteRejectsRealSender() public {
        MevExecutor.Call[] memory calls = new MevExecutor.Call[](0);
        vm.prank(searcher);
        vm.expectRevert(MevExecutor.QuoteIsEthCallOnly.selector);
        exec.quote(calls, address(0));
    }

    /// quoteFrom() is the escape hatch for tooling that always sets `from`:
    /// allowlisted callers only.
    function test_quoteFromRejectsStrangers() public {
        MevExecutor.Call[] memory calls = new MevExecutor.Call[](0);
        vm.prank(address(0xDEAD));
        vm.expectRevert(MevExecutor.NotSearcher.selector);
        exec.quoteFrom(calls, address(0));
    }

    function test_quoteFromWorksForSearcher() public {
        MevExecutor.Call[] memory calls = new MevExecutor.Call[](0);
        vm.prank(searcher);
        (int256 delta,) = exec.quoteFrom(calls, address(0));
        assertEq(delta, 0);
    }

    /// Both entry points must agree: same batch, same reported delta.
    function test_quoteAndQuoteFromAgree() public {
        usdc.mint(address(this), 100e6);
        usdc.approve(address(exec), type(uint256).max);
        MevExecutor.Call[] memory calls = new MevExecutor.Call[](1);
        calls[0] = MevExecutor.Call({
            target: address(usdc),
            value: 0,
            data: abi.encodeWithSignature(
                "transferFrom(address,address,uint256)", address(this), address(exec), 7e6
            )
        });

        uint256 snap = vm.snapshotState();
        vm.prank(address(0));
        (int256 d1,) = exec.quote(calls, address(usdc));
        vm.revertToState(snap);

        vm.prank(searcher);
        (int256 d2,) = exec.quoteFrom(calls, address(usdc));
        vm.revertToState(snap);

        assertEq(d1, 7e6, "quote delta");
        assertEq(d2, d1, "quoteFrom must agree with quote");
    }

    /// A losing batch must report a negative delta, not revert.
    function test_quoteReportsNegativeDelta() public {
        MevExecutor.Call[] memory calls = new MevExecutor.Call[](1);
        // Burn 1 wei of ETH into WETH: a guaranteed negative native delta.
        calls[0] =
            MevExecutor.Call({target: address(weth), value: 1, data: abi.encodeWithSignature("deposit()")});
        vm.prank(address(0));
        (int256 delta,) = exec.quote(calls, address(0));
        assertEq(delta, -1, "losing batch must quote negative, not revert");
    }

    /// quote() works from address(0), the eth_call default.
    function test_quoteWorksFromZeroSender() public {
        MevExecutor.Call[] memory calls = new MevExecutor.Call[](0);
        vm.prank(address(0));
        (int256 delta,) = exec.quote(calls, address(0));
        assertEq(delta, 0);
    }

    /// Non-WETH token profit: bribe silently becomes 0 rather than reverting.
    function test_nonWethProfitSkipsBribe() public {
        usdc.mint(address(this), 100e6);
        usdc.approve(address(exec), type(uint256).max);
        MevExecutor.Call[] memory calls = new MevExecutor.Call[](1);
        calls[0] = MevExecutor.Call({
            target: address(usdc),
            value: 0,
            data: abi.encodeWithSignature(
                "transferFrom(address,address,uint256)", address(this), address(exec), 30e6
            )
        });
        MevExecutor.Guard memory g = _g(address(usdc), 1);
        g.bribeBps = 9000;
        vm.coinbase(address(0xC0FFEE));
        uint256 cb = address(0xC0FFEE).balance;
        vm.prank(searcher);
        exec.execute(bytes32("usdc"), calls, g);
        assertEq(address(0xC0FFEE).balance, cb, "no ETH bribe for non-WETH profit");
    }

    receive() external payable {}
}
