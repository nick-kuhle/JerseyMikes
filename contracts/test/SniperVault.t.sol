// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {SniperVault} from "../src/SniperVault.sol";
import {MockERC20} from "./mocks/MockERC20.sol";
import {MockWETH} from "./mocks/MockWETH.sol";

/// @dev A pool that swaps WETH → token at a fixed rate, and token → WETH back.
///      Deliberately simple: these tests are about the vault's guards, not AMM math.
contract MockPool {
    MockWETH public weth;
    MockERC20 public token;
    /// Tokens minted per 1 WETH in.
    uint256 public rate;
    /// WETH paid per 1 token in, scaled by 1e18.
    uint256 public sellRate;
    /// When true, selling reverts — a honeypot.
    bool public sellBlocked;

    constructor(MockWETH w, MockERC20 t, uint256 r, uint256 sr) {
        weth = w;
        token = t;
        rate = r;
        sellRate = sr;
    }

    function setSellBlocked(bool v) external {
        sellBlocked = v;
    }

    function setSellRate(uint256 v) external {
        sellRate = v;
    }

    /// Pull `amount` WETH from the caller, mint them tokens.
    function buy(uint256 amount) external {
        weth.transferFrom(msg.sender, address(this), amount);
        token.mint(msg.sender, (amount * rate) / 1e18);
    }

    /// Pull `amount` tokens from the caller, pay out WETH.
    function sell(uint256 amount) external {
        require(!sellBlocked, "honeypot: transfer disabled");
        token.transferFrom(msg.sender, address(this), amount);
        weth.transfer(msg.sender, (amount * sellRate) / 1e18);
    }
}

contract SniperVaultTest is Test {
    SniperVault vault;
    MockWETH weth;
    MockERC20 token;
    MockPool pool;

    address searcher = address(0xBEEF);
    address outsider = address(0xBAD);

    uint256 constant DAILY = 10 ether;
    uint256 constant TOTAL = 100 ether;

    function setUp() public {
        weth = new MockWETH();
        token = new MockERC20("Launch", "LNCH");
        // 1 WETH buys 1000 tokens; 1 token sells for 0.001 WETH (flat, no impact).
        pool = new MockPool(weth, token, 1000e18, 1e15);

        vault = new SniperVault(address(weth), DAILY, TOTAL);
        vault.setSearcher(searcher, true);

        // Fund the vault and the pool.
        weth.mint(address(vault), 50 ether);
        weth.mint(address(pool), 500 ether);
        vm.warp(1_000_000);
    }

    // --- helpers ----------------------------------------------------------

    function _buyCalls(uint256 amount) internal view returns (SniperVault.Call[] memory calls) {
        calls = new SniperVault.Call[](2);
        calls[0] = SniperVault.Call({
            target: address(weth),
            value: 0,
            data: abi.encodeWithSignature("approve(address,uint256)", address(pool), amount)
        });
        calls[1] = SniperVault.Call({
            target: address(pool), value: 0, data: abi.encodeWithSignature("buy(uint256)", amount)
        });
    }

    function _sellCalls(uint256 qty) internal view returns (SniperVault.Call[] memory calls) {
        calls = new SniperVault.Call[](2);
        calls[0] = SniperVault.Call({
            target: address(token),
            value: 0,
            data: abi.encodeWithSignature("approve(address,uint256)", address(pool), qty)
        });
        calls[1] = SniperVault.Call({
            target: address(pool), value: 0, data: abi.encodeWithSignature("sell(uint256)", qty)
        });
    }

    function _entry(uint256 maxSpend, uint256 minOut) internal view returns (SniperVault.EntryGuard memory) {
        return SniperVault.EntryGuard({
            token: address(token), maxSpend: maxSpend, minTokensOut: minOut, blockDeadline: 0, maxBaseFee: 0
        });
    }

    function _exit(uint256 maxIn, uint256 minWeth) internal view returns (SniperVault.ExitGuard memory) {
        return SniperVault.ExitGuard({
            token: address(token), maxTokensIn: maxIn, minWethOut: minWeth, blockDeadline: 0, maxBaseFee: 0
        });
    }

    function _open(uint256 amount) internal returns (uint256 spent, uint256 got) {
        vm.prank(searcher);
        return vault.openPosition("tag", _buyCalls(amount), _entry(amount, 0));
    }

    // --- entry ------------------------------------------------------------

    function test_OpenPositionAcquiresTokens() public {
        (uint256 spent, uint256 got) = _open(1 ether);
        assertEq(spent, 1 ether, "spend measured by balance delta");
        assertEq(got, 1000e18, "tokens measured by balance delta");
        assertEq(token.balanceOf(address(vault)), 1000e18);
        assertEq(vault.totalSpent(), 1 ether);
        assertEq(vault.windowSpent(), 1 ether);
        assertEq(vault.tokenAcquired(address(token)), 1000e18);
    }

    /// The defining difference from MevExecutor: a pure spend is allowed.
    function test_OpenPositionSucceedsDespiteBeingUnprofitable() public {
        uint256 before = weth.balanceOf(address(vault));
        _open(1 ether);
        assertLt(weth.balanceOf(address(vault)), before, "the vault is down WETH and that is fine");
    }

    function test_OpenPositionRevertsOverMaxSpend() public {
        // Guard allows 0.5 but the calls spend 1.
        vm.prank(searcher);
        vm.expectRevert(abi.encodeWithSelector(SniperVault.OverSpend.selector, 1 ether, 0.5 ether));
        vault.openPosition("tag", _buyCalls(1 ether), _entry(0.5 ether, 0));
    }

    function test_OpenPositionRevertsBelowMinTokensOut() public {
        vm.prank(searcher);
        vm.expectRevert(abi.encodeWithSelector(SniperVault.InsufficientTokens.selector, 1000e18, 2000e18));
        vault.openPosition("tag", _buyCalls(1 ether), _entry(1 ether, 2000e18));
    }

    function test_OpenPositionRevertsOnZeroToken() public {
        SniperVault.EntryGuard memory g = _entry(1 ether, 0);
        g.token = address(0);
        vm.prank(searcher);
        vm.expectRevert(SniperVault.ZeroToken.selector);
        vault.openPosition("tag", _buyCalls(1 ether), g);
    }

    // --- budget -----------------------------------------------------------

    function test_DailyBudgetBlocksAnOversizedEntry() public {
        vm.prank(searcher);
        vm.expectRevert(abi.encodeWithSelector(SniperVault.DailyBudgetExceeded.selector, 11 ether, DAILY));
        vault.openPosition("tag", _buyCalls(11 ether), _entry(11 ether, 0));
    }

    function test_DailyBudgetAccumulatesAcrossEntries() public {
        for (uint256 i; i < 10; i++) {
            _open(1 ether);
        }
        assertEq(vault.windowSpent(), 10 ether);
        assertEq(vault.spendableRemaining(), 0, "daily budget is exhausted");

        vm.prank(searcher);
        vm.expectRevert();
        vault.openPosition("tag", _buyCalls(1 ether), _entry(1 ether, 0));
    }

    function test_BudgetWindowRollsAfter24Hours() public {
        _open(10 ether);
        assertEq(vault.spendableRemaining(), 0);

        vm.warp(block.timestamp + 1 days);
        assertEq(vault.spendableRemaining(), DAILY, "the view reflects a rolled window");

        _open(1 ether);
        assertEq(vault.windowSpent(), 1 ether, "the window reset on write");
    }

    function test_TotalBudgetIsRespectedAcrossWindows() public {
        // Burn 10 ETH/day for 10 days == the 100 ETH lifetime cap.
        for (uint256 d; d < 10; d++) {
            _open(10 ether);
            vm.warp(block.timestamp + 1 days);
            weth.mint(address(vault), 10 ether); // keep the vault funded
        }
        assertEq(vault.totalSpent(), TOTAL);
        assertEq(vault.spendableRemaining(), 0, "lifetime budget is the binding constraint");

        vm.prank(searcher);
        vm.expectRevert();
        vault.openPosition("tag", _buyCalls(1 ether), _entry(1 ether, 0));
    }

    function test_ZeroTotalBudgetMeansUnlimited() public {
        SniperVault v = new SniperVault(address(weth), DAILY, 0);
        assertEq(v.spendableRemaining(), DAILY);
    }

    /// A vault deployed with no budget is inert — the same fail-closed default
    /// the off-chain lane uses.
    function test_ZeroDailyBudgetCannotBuy() public {
        SniperVault v = new SniperVault(address(weth), 0, 0);
        weth.mint(address(v), 10 ether);
        assertEq(v.spendableRemaining(), 0);
        vm.expectRevert();
        v.openPosition("tag", _buyCalls(1 ether), _entry(1 ether, 0));
    }

    function test_OnlyOwnerCanRaiseTheBudget() public {
        vm.prank(searcher);
        vm.expectRevert(SniperVault.NotOwner.selector);
        vault.setBudget(1000 ether, 1000 ether);

        vault.setBudget(20 ether, 200 ether);
        assertEq(vault.dailyBudget(), 20 ether);
    }

    /// Budget is booked on the *realised* spend, so an entry that used less
    /// than its ceiling does not consume budget it never spent.
    function test_BudgetBooksRealisedSpendNotTheCeiling() public {
        vm.prank(searcher);
        vault.openPosition("tag", _buyCalls(1 ether), _entry(5 ether, 0));
        assertEq(vault.windowSpent(), 1 ether, "only the actual spend is booked");
        assertEq(vault.spendableRemaining(), DAILY - 1 ether);
    }

    // --- exit -------------------------------------------------------------

    function test_ClosePositionReturnsWeth() public {
        _open(1 ether);
        uint256 before = weth.balanceOf(address(vault));

        vm.prank(searcher);
        (uint256 sold, uint256 received) = vault.closePosition("tag", _sellCalls(1000e18), _exit(1000e18, 0));

        assertEq(sold, 1000e18);
        assertEq(received, 1 ether);
        assertEq(weth.balanceOf(address(vault)), before + 1 ether);
        assertEq(token.balanceOf(address(vault)), 0);
        assertEq(vault.tokenSold(address(token)), 1000e18);
    }

    function test_PartialExitLeavesTheRemainder() public {
        _open(1 ether);
        vm.prank(searcher);
        (uint256 sold,) = vault.closePosition("tag", _sellCalls(400e18), _exit(400e18, 0));
        assertEq(sold, 400e18);
        assertEq(token.balanceOf(address(vault)), 600e18, "the runner is still held");
    }

    function test_CloseRevertsBelowMinWethOut() public {
        _open(1 ether);
        // Price halved; demand the full 1 ETH back.
        pool.setSellRate(5e14);
        vm.prank(searcher);
        vm.expectRevert(abi.encodeWithSelector(SniperVault.InsufficientWeth.selector, 0.5 ether, 1 ether));
        vault.closePosition("tag", _sellCalls(1000e18), _exit(1000e18, 1 ether));
    }

    function test_CloseRevertsOverMaxTokensIn() public {
        _open(1 ether);
        vm.prank(searcher);
        vm.expectRevert(abi.encodeWithSelector(SniperVault.OverSell.selector, 1000e18, 500e18));
        vault.closePosition("tag", _sellCalls(1000e18), _exit(500e18, 0));
    }

    /// The honeypot backstop at the contract boundary: an unsellable token
    /// reverts the whole batch rather than silently doing nothing.
    function test_HoneypotSellRevertsTheBatch() public {
        _open(1 ether);
        pool.setSellBlocked(true);
        vm.prank(searcher);
        vm.expectRevert(); // CallFailed, bubbling the pool's revert
        vault.closePosition("tag", _sellCalls(1000e18), _exit(1000e18, 0.9 ether));
    }

    /// Exits must never be blocked by a spend ceiling — being trapped in a
    /// position because the budget ran out would be the worst possible bug.
    function test_ExitWorksWithTheBudgetFullyExhausted() public {
        _open(10 ether); // exhausts the daily budget
        assertEq(vault.spendableRemaining(), 0);
        vm.prank(searcher);
        (, uint256 received) = vault.closePosition("t", _sellCalls(10000e18), _exit(10000e18, 0));
        assertEq(received, 10 ether, "getting out is always allowed");
    }

    // --- access control ---------------------------------------------------

    function test_OutsiderCannotOpenOrClose() public {
        vm.prank(outsider);
        vm.expectRevert(SniperVault.NotSearcher.selector);
        vault.openPosition("tag", _buyCalls(1 ether), _entry(1 ether, 0));

        _open(1 ether);
        vm.prank(outsider);
        vm.expectRevert(SniperVault.NotSearcher.selector);
        vault.closePosition("tag", _sellCalls(1000e18), _exit(1000e18, 0));
    }

    function test_RevokedSearcherLosesAccess() public {
        vault.setSearcher(searcher, false);
        vm.prank(searcher);
        vm.expectRevert(SniperVault.NotSearcher.selector);
        vault.openPosition("tag", _buyCalls(1 ether), _entry(1 ether, 0));
    }

    function test_OnlyOwnerCanSweep() public {
        _open(1 ether);
        vm.prank(searcher);
        vm.expectRevert(SniperVault.NotOwner.selector);
        vault.sweep(address(token), searcher, 1);

        vault.sweep(address(token), address(this), 1000e18);
        assertEq(token.balanceOf(address(this)), 1000e18);
    }

    function test_SweepNativeEth() public {
        vm.deal(address(vault), 1 ether);
        uint256 before = address(this).balance;
        vault.sweep(address(0), address(this), 1 ether);
        assertEq(address(this).balance, before + 1 ether);
    }

    function test_OwnerTransfer() public {
        vault.setOwner(searcher);
        assertEq(vault.owner(), searcher);
        vm.expectRevert(SniperVault.NotOwner.selector);
        vault.setBudget(1, 1);
    }

    /// The worst realistic case: the searcher key is fully compromised. It can
    /// burn the remaining budget on garbage, but it cannot move value out.
    function test_CompromisedSearcherCannotExfiltrateFunds() public {
        SniperVault.Call[] memory steal = new SniperVault.Call[](1);
        steal[0] = SniperVault.Call({
            target: address(weth),
            value: 0,
            data: abi.encodeWithSignature("transfer(address,uint256)", outsider, 10 ether)
        });

        // A raw transfer out is not an acquisition: no tokens arrive, so the
        // minTokensOut floor stops it.
        vm.prank(searcher);
        vm.expectRevert(abi.encodeWithSelector(SniperVault.InsufficientTokens.selector, 0, 1));
        vault.openPosition("steal", steal, _entry(10 ether, 1));

        // And it can never exceed the budget the owner set.
        assertLe(vault.spendableRemaining(), DAILY);
        assertEq(weth.balanceOf(outsider), 0);
    }

    // --- execution guards -------------------------------------------------

    function test_BlockDeadlineIsEnforced() public {
        // Roll forward first: a deadline of 0 means "no deadline", so the
        // default block 1 would make `block.number - 1` a no-op guard.
        vm.roll(100);
        SniperVault.EntryGuard memory g = _entry(1 ether, 0);
        g.blockDeadline = uint64(block.number - 1);
        vm.prank(searcher);
        vm.expectRevert(SniperVault.Deadline.selector);
        vault.openPosition("tag", _buyCalls(1 ether), g);
    }

    function test_BlockDeadlineAllowsTheTargetBlock() public {
        vm.roll(100);
        SniperVault.EntryGuard memory g = _entry(1 ether, 0);
        g.blockDeadline = uint64(block.number);
        vm.prank(searcher);
        (uint256 spent,) = vault.openPosition("tag", _buyCalls(1 ether), g);
        assertEq(spent, 1 ether, "the deadline block itself is still valid");
    }

    function test_MaxBaseFeeIsEnforced() public {
        vm.fee(100 gwei);
        SniperVault.EntryGuard memory g = _entry(1 ether, 0);
        g.maxBaseFee = 50 gwei;
        vm.prank(searcher);
        vm.expectRevert(SniperVault.BaseFeeTooHigh.selector);
        vault.openPosition("tag", _buyCalls(1 ether), g);
    }

    function test_FailingCallBubblesTheIndex() public {
        SniperVault.Call[] memory calls = new SniperVault.Call[](1);
        calls[0] = SniperVault.Call({
            target: address(pool), value: 0, data: abi.encodeWithSignature("doesNotExist()")
        });
        vm.prank(searcher);
        vm.expectRevert();
        vault.openPosition("tag", calls, _entry(1 ether, 0));
    }

    // --- accounting invariants -------------------------------------------

    /// Round trip: what goes out and comes back is measured purely by balance
    /// deltas, so fee-on-transfer tokens are handled without special cases.
    function testFuzz_RoundTripAccounting(uint96 amount) public {
        amount = uint96(bound(amount, 0.001 ether, 5 ether));
        (uint256 spent, uint256 got) = _open(amount);
        assertEq(spent, amount);
        assertEq(got, (uint256(amount) * 1000e18) / 1e18);

        vm.prank(searcher);
        (uint256 sold, uint256 received) = vault.closePosition("t", _sellCalls(got), _exit(got, 0));
        assertEq(sold, got);
        assertEq(received, amount, "flat-rate pool round trips exactly");
    }

    function testFuzz_SpendNeverExceedsTheBudget(uint96 a, uint96 b, uint96 c) public {
        uint256[3] memory amounts = [bound(a, 0, 6 ether), bound(b, 0, 6 ether), bound(c, 0, 6 ether)];
        for (uint256 i; i < 3; i++) {
            if (amounts[i] == 0) continue;
            vm.prank(searcher);
            try vault.openPosition("f", _buyCalls(amounts[i]), _entry(amounts[i], 0)) {} catch {}
        }
        assertLe(vault.windowSpent(), DAILY, "the daily ceiling is never breached");
        assertLe(vault.totalSpent(), TOTAL, "the lifetime ceiling is never breached");
    }

    receive() external payable {}
}
