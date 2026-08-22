// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Test, console2} from "forge-std/Test.sol";
import {MevExecutor} from "../src/MevExecutor.sol";
import {MockERC20} from "./mocks/MockERC20.sol";
import {MockWETH} from "./mocks/MockWETH.sol";
import {MockUniV2Pair} from "./mocks/MockUniV2Pair.sol";
import {MockBalancerVault} from "./mocks/MockBalancerVault.sol";

contract MevExecutorTest is Test {
    MevExecutor internal exec;
    MockWETH internal weth;
    MockERC20 internal usdc;
    MockBalancerVault internal vault;

    address internal searcher = address(0xBEEF);
    address internal stranger = address(0xDEAD);

    function setUp() public {
        weth = new MockWETH();
        usdc = new MockERC20("USD Coin", "USDC");
        vault = new MockBalancerVault();
        exec = new MevExecutor(address(vault), address(weth));
        exec.setSearcher(searcher, true);
        vm.deal(address(exec), 10 ether);
        vm.deal(address(this), 100 ether);
    }

    function _guard(address token, uint256 minProfit) internal pure returns (MevExecutor.Guard memory) {
        return MevExecutor.Guard({
            profitToken: token, minProfit: minProfit, bribeBps: 0, blockDeadline: 0, maxBaseFee: 0, phase: 0
        });
    }

    // -----------------------------------------------------------------
    // Access control
    // -----------------------------------------------------------------

    function test_onlySearcherCanExecute() public {
        MevExecutor.Call[] memory calls = new MevExecutor.Call[](0);
        vm.prank(stranger);
        vm.expectRevert(MevExecutor.NotSearcher.selector);
        exec.execute(bytes32(0), calls, _guard(address(0), 0));
    }

    function test_onlyOwnerCanSweep() public {
        vm.prank(stranger);
        vm.expectRevert(MevExecutor.NotOwner.selector);
        exec.sweep(address(0), stranger, 1 ether);
    }

    function test_ownerCanSweepEth() public {
        uint256 before = address(this).balance;
        exec.sweep(address(0), address(this), 1 ether);
        assertEq(address(this).balance, before + 1 ether);
    }

    // -----------------------------------------------------------------
    // The core invariant: no profit => revert => the builder drops the
    // bundle => zero gas burned.
    // -----------------------------------------------------------------

    function test_revertsWhenUnprofitable() public {
        MevExecutor.Call[] memory calls = new MevExecutor.Call[](1);
        // Burn 1 wei of ETH: guaranteed negative delta.
        calls[0] =
            MevExecutor.Call({target: address(weth), value: 1, data: abi.encodeWithSignature("deposit()")});

        vm.prank(searcher);
        vm.expectRevert(abi.encodeWithSelector(MevExecutor.Unprofitable.selector, 0, 1));
        exec.execute(bytes32("arb"), calls, _guard(address(0), 1));
    }

    function test_revertsWhenProfitBelowThreshold() public {
        usdc.mint(address(this), 100e6);
        usdc.approve(address(exec), type(uint256).max);

        MevExecutor.Call[] memory calls = new MevExecutor.Call[](1);
        calls[0] = MevExecutor.Call({
            target: address(usdc),
            value: 0,
            data: abi.encodeWithSignature(
                "transferFrom(address,address,uint256)", address(this), address(exec), 10e6
            )
        });

        vm.prank(searcher);
        vm.expectRevert(abi.encodeWithSelector(MevExecutor.Unprofitable.selector, 10e6, 25e6));
        exec.execute(bytes32("arb"), calls, _guard(address(usdc), 25e6));
    }

    function test_succeedsWhenProfitable() public {
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

        vm.prank(searcher);
        uint256 profit = exec.execute(bytes32("arb"), calls, _guard(address(usdc), 25e6));
        assertEq(profit, 30e6);
        assertEq(usdc.balanceOf(address(exec)), 30e6);
    }

    function test_bubblesUpInnerRevert() public {
        MevExecutor.Call[] memory calls = new MevExecutor.Call[](1);
        // transfer more USDC than the executor owns -> inner revert
        calls[0] = MevExecutor.Call({
            target: address(usdc),
            value: 0,
            data: abi.encodeWithSignature("transfer(address,uint256)", stranger, 1e6)
        });
        vm.prank(searcher);
        vm.expectRevert();
        exec.execute(bytes32("arb"), calls, _guard(address(usdc), 0));
    }

    // -----------------------------------------------------------------
    // Guards
    // -----------------------------------------------------------------

    function test_blockDeadline() public {
        MevExecutor.Call[] memory calls = new MevExecutor.Call[](0);
        MevExecutor.Guard memory g = _guard(address(0), 0);
        g.blockDeadline = uint64(block.number);
        vm.roll(block.number + 1);
        vm.prank(searcher);
        vm.expectRevert(MevExecutor.Deadline.selector);
        exec.execute(bytes32(0), calls, g);
    }

    function test_maxBaseFee() public {
        MevExecutor.Call[] memory calls = new MevExecutor.Call[](0);
        MevExecutor.Guard memory g = _guard(address(0), 0);
        g.maxBaseFee = 10 gwei;
        vm.fee(50 gwei);
        vm.prank(searcher);
        vm.expectRevert(MevExecutor.BaseFeeTooHigh.selector);
        exec.execute(bytes32(0), calls, g);
    }

    function test_coinbaseBribeIsPaidFromProfit() public {
        address builder = address(0xC0FFEE);
        vm.coinbase(builder);
        uint256 builderBefore = builder.balance;

        MevExecutor.Call[] memory calls = new MevExecutor.Call[](1);
        // Donate 1 ETH into the executor to create a positive ETH delta.
        calls[0] =
            MevExecutor.Call({target: address(this), value: 0, data: abi.encodeWithSignature("donate()")});

        // `minProfit` is retained profit after the builder payment. One ETH
        // gross at 90% leaves exactly 0.1 ETH in the executor.
        MevExecutor.Guard memory g = _guard(address(0), 0.1 ether);
        g.bribeBps = 9000;

        vm.prank(searcher);
        uint256 profit = exec.execute(bytes32("arb"), calls, g);
        assertEq(profit, 1 ether);
        assertEq(builder.balance, builderBefore + 0.9 ether);
    }

    function donate() external {
        (bool ok,) = msg.sender.call{value: 1 ether}("");
        require(ok, "donate failed");
    }

    function test_twoLegSettlementDoesNotCallReturnedPrincipalProfit() public {
        bytes32 tag = keccak256("two-leg-loss");
        MevExecutor.Call[] memory front = new MevExecutor.Call[](1);
        front[0] = MevExecutor.Call({
            target: address(weth), value: 1 ether, data: abi.encodeWithSignature("deposit()")
        });
        MevExecutor.Guard memory open = _guard(address(0), 0);
        open.phase = 1;
        vm.prank(searcher);
        exec.execute(tag, front, open);
        assertEq(address(exec).balance, 9 ether, "front spent one ETH");

        // Returning exactly the one ETH of principal used by the front leg is
        // zero total profit. The historical per-leg guard called it +1 ETH.
        MevExecutor.Call[] memory back = new MevExecutor.Call[](1);
        back[0] =
            MevExecutor.Call({target: address(this), value: 0, data: abi.encodeWithSignature("donate()")});
        MevExecutor.Guard memory close = _guard(address(0), 1);
        close.phase = 2;
        vm.prank(searcher);
        vm.expectRevert(abi.encodeWithSelector(MevExecutor.Unprofitable.selector, 0, 1));
        exec.execute(tag, back, close);
    }

    function test_twoLegBribeCanNeverConsumePrincipal() public {
        bytes32 tag = keccak256("two-leg-bribe");
        MevExecutor.Call[] memory front = new MevExecutor.Call[](1);
        front[0] = MevExecutor.Call({
            target: address(weth), value: 1 ether, data: abi.encodeWithSignature("deposit()")
        });
        MevExecutor.Guard memory open = _guard(address(0), 0);
        open.phase = 1;
        vm.prank(searcher);
        exec.execute(tag, front, open);

        // Return principal + 1 ETH gross profit. A 90% bribe leaves 0.1 ETH,
        // so a 0.2 ETH retained-profit floor must reject the close. The bribe
        // is calculated on the 1 ETH total profit, never the 2 ETH back-leg
        // proceeds.
        MevExecutor.Call[] memory back = new MevExecutor.Call[](2);
        back[0] =
            MevExecutor.Call({target: address(this), value: 0, data: abi.encodeWithSignature("donate()")});
        back[1] =
            MevExecutor.Call({target: address(this), value: 0, data: abi.encodeWithSignature("donate()")});
        MevExecutor.Guard memory close = _guard(address(0), 0.2 ether);
        close.phase = 2;
        close.bribeBps = 9000;
        vm.prank(searcher);
        vm.expectRevert(abi.encodeWithSelector(MevExecutor.Unprofitable.selector, 0.1 ether, 0.2 ether));
        exec.execute(tag, back, close);
    }

    // -----------------------------------------------------------------
    // Atomic arbitrage across two mock pools + flash loan
    // -----------------------------------------------------------------

    function test_flashLoanAtomicArb() public {
        MockERC20 tokenA = new MockERC20("A", "A");
        MockERC20 tokenB = new MockERC20("B", "B");

        // Pool 1 is mispriced: B is cheap there.
        MockUniV2Pair p1 = new MockUniV2Pair(tokenA, tokenB, 1_000_000e18, 2_000_000e18, 30);
        MockUniV2Pair p2 = new MockUniV2Pair(tokenA, tokenB, 1_000_000e18, 1_000_000e18, 30);

        tokenA.mint(address(vault), 100_000e18);

        uint256 loan = 10_000e18;
        address[] memory tokens = new address[](1);
        tokens[0] = address(tokenA);
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = loan;

        uint256 outB = p1.getAmountOut(loan, true);

        MevExecutor.Call[] memory calls = new MevExecutor.Call[](4);
        calls[0] = MevExecutor.Call({
            target: address(tokenA),
            value: 0,
            data: abi.encodeWithSignature("approve(address,uint256)", address(p1), type(uint256).max)
        });
        calls[1] = MevExecutor.Call({
            target: address(p1),
            value: 0,
            data: abi.encodeWithSignature("swap(uint256,bool,address)", loan, true, address(exec))
        });
        calls[2] = MevExecutor.Call({
            target: address(tokenB),
            value: 0,
            data: abi.encodeWithSignature("approve(address,uint256)", address(p2), type(uint256).max)
        });
        calls[3] = MevExecutor.Call({
            target: address(p2),
            value: 0,
            data: abi.encodeWithSignature("swap(uint256,bool,address)", outB, false, address(exec))
        });

        vm.prank(searcher);
        exec.flashExecute(bytes32("arb"), tokens, amounts, calls, _guard(address(tokenA), 1));

        assertGt(tokenA.balanceOf(address(exec)), 0, "arb should be profitable");
        console2.log("flash arb profit (tokenA wei)", tokenA.balanceOf(address(exec)));
    }

    function test_flashLoanRevertsWhenArbIsUnprofitable() public {
        MockERC20 tokenA = new MockERC20("A", "A");
        MockERC20 tokenB = new MockERC20("B", "B");
        // Both pools priced identically -> round trip loses the 2x30bps fee.
        MockUniV2Pair p1 = new MockUniV2Pair(tokenA, tokenB, 1_000_000e18, 1_000_000e18, 30);
        MockUniV2Pair p2 = new MockUniV2Pair(tokenA, tokenB, 1_000_000e18, 1_000_000e18, 30);
        tokenA.mint(address(vault), 100_000e18);

        uint256 loan = 10_000e18;
        address[] memory tokens = new address[](1);
        tokens[0] = address(tokenA);
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = loan;
        uint256 outB = p1.getAmountOut(loan, true);

        MevExecutor.Call[] memory calls = new MevExecutor.Call[](4);
        calls[0] = MevExecutor.Call({
            target: address(tokenA),
            value: 0,
            data: abi.encodeWithSignature("approve(address,uint256)", address(p1), type(uint256).max)
        });
        calls[1] = MevExecutor.Call({
            target: address(p1),
            value: 0,
            data: abi.encodeWithSignature("swap(uint256,bool,address)", loan, true, address(exec))
        });
        calls[2] = MevExecutor.Call({
            target: address(tokenB),
            value: 0,
            data: abi.encodeWithSignature("approve(address,uint256)", address(p2), type(uint256).max)
        });
        calls[3] = MevExecutor.Call({
            target: address(p2),
            value: 0,
            data: abi.encodeWithSignature("swap(uint256,bool,address)", outB, false, address(exec))
        });

        vm.prank(searcher);
        vm.expectRevert(); // repayment fails / unprofitable — nothing lands on chain
        exec.flashExecute(bytes32("arb"), tokens, amounts, calls, _guard(address(tokenA), 1));
    }

    // -----------------------------------------------------------------
    // Sandwich shape: front-run, victim, back-run in one batch
    // -----------------------------------------------------------------

    function test_sandwichShapeIsProfitable() public {
        MockERC20 tokenA = new MockERC20("A", "A");
        MockERC20 tokenB = new MockERC20("B", "B");
        MockUniV2Pair pool = new MockUniV2Pair(tokenA, tokenB, 1_000_000e18, 1_000_000e18, 30);

        // Executor is funded with the front-run capital.
        uint256 frontRun = 5_000e18;
        tokenA.mint(address(exec), frontRun);

        // 1. front-run: buy B
        MevExecutor.Call[] memory front = new MevExecutor.Call[](2);
        front[0] = MevExecutor.Call({
            target: address(tokenA),
            value: 0,
            data: abi.encodeWithSignature("approve(address,uint256)", address(pool), type(uint256).max)
        });
        front[1] = MevExecutor.Call({
            target: address(pool),
            value: 0,
            data: abi.encodeWithSignature("swap(uint256,bool,address)", frontRun, true, address(exec))
        });
        vm.prank(searcher);
        exec.execute(bytes32("sandwich-front"), front, _guard(address(tokenB), 1));
        uint256 gotB = tokenB.balanceOf(address(exec));

        // 2. victim buys B, pushing the price up further
        address victim = address(0x5151);
        tokenA.mint(victim, 20_000e18);
        vm.startPrank(victim);
        tokenA.approve(address(pool), type(uint256).max);
        pool.swap(20_000e18, true, victim);
        vm.stopPrank();

        // 3. back-run: sell B for A, must net more A than we spent
        MevExecutor.Call[] memory back = new MevExecutor.Call[](2);
        back[0] = MevExecutor.Call({
            target: address(tokenB),
            value: 0,
            data: abi.encodeWithSignature("approve(address,uint256)", address(pool), type(uint256).max)
        });
        back[1] = MevExecutor.Call({
            target: address(pool),
            value: 0,
            data: abi.encodeWithSignature("swap(uint256,bool,address)", gotB, false, address(exec))
        });
        vm.prank(searcher);
        uint256 profitA = exec.execute(bytes32("sandwich-back"), back, _guard(address(tokenA), frontRun + 1));
        assertGt(profitA, frontRun, "sandwich must return more than it risked");
        console2.log("sandwich gross (tokenA wei)", profitA - frontRun);
    }

    // -----------------------------------------------------------------
    // receive() is load-bearing, not decorative
    // -----------------------------------------------------------------

    /// The WETH-profit bribe path unwraps via `IWETH.withdraw`, and WETH9 pays
    /// out by sending ETH straight back to the executor. If `receive()` ever
    /// reverts, `withdraw` fails, the whole batch reverts, and every
    /// WETH-denominated bribe silently stops working. This test pins that
    /// dependency so the "make receive() revert" refactor cannot land
    /// unnoticed.
    function test_wethProfitBribeRoutesEthThroughReceive() public {
        weth.mint(address(this), 1 ether);
        weth.approve(address(exec), type(uint256).max);
        // Back the WETH contract so withdraw() can actually pay out.
        vm.deal(address(weth), 1 ether);

        MevExecutor.Call[] memory calls = new MevExecutor.Call[](1);
        calls[0] = MevExecutor.Call({
            target: address(weth),
            value: 0,
            data: abi.encodeWithSignature(
                "transferFrom(address,address,uint256)", address(this), address(exec), 1 ether
            )
        });

        MevExecutor.Guard memory g = _guard(address(weth), 1);
        g.bribeBps = 5000; // half the profit to the builder

        address builder = address(0xC0FFEE);
        vm.coinbase(builder);
        uint256 before = builder.balance;

        vm.prank(searcher);
        uint256 profit = exec.execute(bytes32("weth-bribe"), calls, g);

        assertEq(profit, 1 ether, "profit measured in WETH");
        assertEq(builder.balance, before + 0.5 ether, "builder paid in unwrapped ETH");
    }

    /// A plain ETH transfer into the executor must succeed: routers refund
    /// unspent ETH this way, and `sweep` is how it comes back out.
    function test_plainEthTransferIsAccepted() public {
        uint256 before = address(exec).balance;
        (bool ok,) = address(exec).call{value: 1 ether}("");
        assertTrue(ok, "executor must accept native ETH");
        assertEq(address(exec).balance, before + 1 ether);
    }

    /// There is no fallback(): an unknown selector must revert rather than
    /// silently succeed.
    function test_unknownSelectorReverts() public {
        (bool ok,) = address(exec).call(abi.encodeWithSignature("noSuchFunction()"));
        assertFalse(ok, "unknown selectors must revert (no fallback)");
    }

    receive() external payable {}
}
