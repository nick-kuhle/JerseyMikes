// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {Vm} from "forge-std/Vm.sol";
import {SniperVault} from "../src/SniperVault.sol";
import {MockERC20} from "./mocks/MockERC20.sol";
import {MockWETH} from "./mocks/MockWETH.sol";
import {SimV2Pair} from "./mocks/SimV2Pair.sol";

/// @dev Exercises the SniperVault exactly the way the bot's **simulation
///      fixture** does: the entry/exit calldata shape is the real UniswapV2
///      flash-swap flow (transfer input to the pair, then
///      `swap(amount0Out, amount1Out, to, "")`), and the assertions pin the
///      event values the Rust simulator books its paper ledger from.
contract SniperVaultSimFixtureTest is Test {
    // Canonical wrapped-native addresses, one per supported chain. The
    // fixture must bind whichever the deployment chain uses — never reuse one
    // chain's WETH on another (work order §12.7).
    address constant MAINNET_WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant BASE_WETH = 0x4200000000000000000000000000000000000006;

    MockWETH weth;
    MockERC20 token;
    SimV2Pair pair;
    SniperVault vault;

    address searcher = address(0x5EAE);
    address owner;
    /// Cached token ordering: the calldata helpers must stay free of
    /// external calls so they can be evaluated inside a prank/expectRevert
    /// frame without consuming it.
    bool wethToken0;

    uint256 constant DAILY = 1 ether;
    uint256 constant TOTAL = 5 ether;
    uint256 constant WETH_RESERVE = 10 ether;
    uint256 constant TOKEN_RESERVE = 1_000_000 ether;

    function setUp() public {
        owner = address(this);
        weth = new MockWETH();
        token = new MockERC20("Sim Launch", "SIM");
        pair = new SimV2Pair(address(weth), address(token));

        // Seed deterministic liquidity: transfer reserves in, then sync.
        weth.mint(address(pair), WETH_RESERVE);
        token.mint(address(pair), TOKEN_RESERVE);
        pair.sync();

        vault = new SniperVault(address(weth), DAILY, TOTAL);
        vault.setSearcher(searcher, true);
        weth.mint(address(vault), 10 ether);
        wethToken0 = pair.token0() == address(weth);

        vm.warp(1_000_000);
    }

    // --- helpers ----------------------------------------------------------

    /// The V2 output formula the bot uses off-chain (0.3% fee).
    function amountOut(uint256 amountIn, uint256 reserveIn, uint256 reserveOut)
        internal
        pure
        returns (uint256)
    {
        uint256 amountInWithFee = amountIn * 997;
        return (amountInWithFee * reserveOut) / (reserveIn * 1000 + amountInWithFee);
    }

    function wethIsToken0() internal view returns (bool) {
        return wethToken0;
    }

    /// Entry calldata in the exact shape `sniper::calldata::build_entry`
    /// produces: WETH transfer to the pair, then an optimistic-output swap
    /// back to the vault.
    function entryCalls(uint256 sizeWei, uint256 tokensOut)
        internal
        view
        returns (SniperVault.Call[] memory calls)
    {
        calls = new SniperVault.Call[](2);
        calls[0] = SniperVault.Call({
            target: address(weth),
            value: 0,
            data: abi.encodeWithSignature("transfer(address,uint256)", address(pair), sizeWei)
        });
        (uint256 out0, uint256 out1) = wethIsToken0() ? (uint256(0), tokensOut) : (tokensOut, uint256(0));
        calls[1] = SniperVault.Call({
            target: address(pair),
            value: 0,
            data: abi.encodeWithSignature(
                "swap(uint256,uint256,address,bytes)", out0, out1, address(vault), ""
            )
        });
    }

    function exitCalls(uint256 qty, uint256 wethOut) internal view returns (SniperVault.Call[] memory calls) {
        calls = new SniperVault.Call[](2);
        calls[0] = SniperVault.Call({
            target: address(token),
            value: 0,
            data: abi.encodeWithSignature("transfer(address,uint256)", address(pair), qty)
        });
        (uint256 out0, uint256 out1) = wethIsToken0() ? (wethOut, uint256(0)) : (uint256(0), wethOut);
        calls[1] = SniperVault.Call({
            target: address(pair),
            value: 0,
            data: abi.encodeWithSignature(
                "swap(uint256,uint256,address,bytes)", out0, out1, address(vault), ""
            )
        });
    }

    function entryGuard(uint256 maxSpend, uint256 minOut, uint64 deadline, uint256 maxBaseFee)
        internal
        view
        returns (SniperVault.EntryGuard memory)
    {
        return SniperVault.EntryGuard({
            token: address(token),
            maxSpend: maxSpend,
            minTokensOut: minOut,
            blockDeadline: deadline,
            maxBaseFee: maxBaseFee
        });
    }

    function exitGuard(uint256 maxIn, uint256 minWeth, uint64 deadline, uint256 maxBaseFee)
        internal
        view
        returns (SniperVault.ExitGuard memory)
    {
        return SniperVault.ExitGuard({
            token: address(token),
            maxTokensIn: maxIn,
            minWethOut: minWeth,
            blockDeadline: deadline,
            maxBaseFee: maxBaseFee
        });
    }

    function openAsSearcher(uint256 sizeWei, uint256 minOut)
        internal
        returns (uint256 spent, uint256 received)
    {
        // Quote from the pair's *current* reserves, exactly as the bot's
        // mark-to-market does — the constants only hold for the first trade.
        (uint112 r0, uint112 r1,) = pair.getReserves();
        (uint256 wethReserve, uint256 tokenReserve) =
            wethIsToken0() ? (uint256(r0), uint256(r1)) : (uint256(r1), uint256(r0));
        uint256 expected = amountOut(sizeWei, wethReserve, tokenReserve);
        vm.prank(searcher);
        return
            vault.openPosition(
                bytes32("tag"), entryCalls(sizeWei, expected), entryGuard(sizeWei, minOut, 0, 0)
            );
    }

    // --- constructor / chain WETH binding ---------------------------------

    function test_ConstructorBindsTheChainWeth() public view {
        assertEq(vault.WETH(), address(weth), "fixture must bind the chain WETH given at deploy");
        assertEq(vault.owner(), owner);
        assertEq(vault.dailyBudget(), DAILY);
        assertEq(vault.totalBudget(), TOTAL);
        assertTrue(vault.searchers(owner), "the deployer starts allowlisted");
    }

    function test_ConstructorBindingIsChainSpecific() public {
        // Two fixture deployments, one per chain, with that chain's canonical
        // WETH: each vault must report its own binding, proving the address is
        // a constructor choice and never a shared constant.
        vm.prank(address(0xA11CE));
        SniperVault mainnetVault = new SniperVault(MAINNET_WETH, DAILY, TOTAL);
        vm.prank(address(0xA11CE));
        SniperVault baseVault = new SniperVault(BASE_WETH, DAILY, TOTAL);
        assertEq(mainnetVault.WETH(), MAINNET_WETH);
        assertEq(baseVault.WETH(), BASE_WETH);
        assertTrue(mainnetVault.WETH() != baseVault.WETH());
    }

    function test_ConstructorEmitsBudgetAndOwnerEvents() public {
        vm.recordLogs();
        SniperVault fresh = new SniperVault(address(weth), DAILY, TOTAL);
        Vm.Log[] memory logs = vm.getRecordedLogs();
        bool sawBudget;
        bool sawOwner;
        for (uint256 i; i < logs.length; i++) {
            if (logs[i].emitter == address(fresh)) {
                if (logs[i].topics[0] == keccak256("BudgetSet(uint256,uint256)")) {
                    sawBudget = true;
                    (uint256 d, uint256 t) = abi.decode(logs[i].data, (uint256, uint256));
                    assertEq(d, DAILY);
                    assertEq(t, TOTAL);
                }
                if (logs[i].topics[0] == keccak256("OwnerChanged(address,address)")) {
                    sawOwner = true;
                }
            }
        }
        assertTrue(sawBudget && sawOwner);
    }

    function test_ZeroBudgetFixtureCannotBuyUntilOwnerSetsOne() public {
        SniperVault locked = new SniperVault(address(weth), 0, 0);
        locked.setSearcher(searcher, true);
        weth.mint(address(locked), 1 ether);
        uint256 expected = amountOut(0.1 ether, WETH_RESERVE, TOKEN_RESERVE);
        vm.prank(searcher);
        vm.expectRevert(
            abi.encodeWithSelector(SniperVault.DailyBudgetExceeded.selector, 0.1 ether, uint256(0))
        );
        locked.openPosition(bytes32("tag"), entryCalls(0.1 ether, expected), entryGuard(0.1 ether, 0, 0, 0));
    }

    // --- entry/exit event values used by the simulator --------------------

    function test_EntryEventCarriesTheExactValuesTheSimulatorBooks() public {
        uint256 sizeWei = 0.1 ether;
        uint256 expected = amountOut(sizeWei, WETH_RESERVE, TOKEN_RESERVE);
        vm.expectEmit(true, true, false, true, address(vault));
        emit SniperVault.EntryExecuted(bytes32("tag"), address(token), sizeWei, expected);

        (uint256 spent, uint256 received) = openAsSearcher(sizeWei, 0);
        assertEq(spent, sizeWei, "wethSpent must equal the realised transfer");
        assertEq(received, expected, "tokensReceived must equal the swap output");
        assertEq(token.balanceOf(address(vault)), expected);
        assertEq(vault.totalSpent(), sizeWei);
    }

    function test_ExitEventCarriesTheExactValuesTheSimulatorBooks() public {
        (uint256 spent, uint256 received) = openAsSearcher(0.1 ether, 0);

        // Mark-to-market style quote: what the reserve math says the tokens fetch.
        (uint112 r0, uint112 r1,) = pair.getReserves();
        (uint256 wethReserve, uint256 tokenReserve) =
            wethIsToken0() ? (uint256(r0), uint256(r1)) : (uint256(r1), uint256(r0));
        uint256 wethOut = amountOut(received, tokenReserve, wethReserve);

        vm.expectEmit(true, true, false, true, address(vault));
        emit SniperVault.ExitExecuted(bytes32("tag"), address(token), received, wethOut);

        vm.prank(searcher);
        (uint256 sold, uint256 got) = vault.closePosition(
            bytes32("tag"), exitCalls(received, wethOut), exitGuard(received, wethOut, 0, 0)
        );
        assertEq(sold, received);
        assertEq(got, wethOut);
        assertEq(token.balanceOf(address(vault)), 0, "a full exit empties the vault");
        // A round trip through a flat pool costs the 0.3% fee twice.
        assertTrue(got < spent, "the round trip must net less than the entry");
    }

    function test_PartialExitLeavesAVaultBalanceTheSimulatorCanMark() public {
        (, uint256 received) = openAsSearcher(0.2 ether, 0);
        uint256 half = received / 2;

        (uint112 r0, uint112 r1,) = pair.getReserves();
        (uint256 wethReserve, uint256 tokenReserve) =
            wethIsToken0() ? (uint256(r0), uint256(r1)) : (uint256(r1), uint256(r0));
        uint256 wethOut = amountOut(half, tokenReserve, wethReserve);

        vm.prank(searcher);
        (uint256 sold, uint256 got) =
            vault.closePosition(bytes32("tag"), exitCalls(half, wethOut), exitGuard(half, wethOut, 0, 0));
        assertEq(sold, half);
        assertEq(got, wethOut);
        assertEq(token.balanceOf(address(vault)), received - half);
    }

    // --- guards the simulation harness relies on ---------------------------

    function test_EntryAboveDailyBudgetRevertsWithTheCeiling() public {
        uint256 sizeWei = 2 ether; // daily budget is 1 ETH
        uint256 expected = amountOut(sizeWei, WETH_RESERVE, TOKEN_RESERVE);
        vm.prank(searcher);
        vm.expectRevert(abi.encodeWithSelector(SniperVault.DailyBudgetExceeded.selector, sizeWei, DAILY));
        vault.openPosition(bytes32("tag"), entryCalls(sizeWei, expected), entryGuard(sizeWei, 0, 0, 0));
    }

    function test_TotalBudgetRevertsAfterTheWindowRolls() public {
        // Spend up to the total budget across rolled windows.
        openAsSearcher(1 ether, 0);
        vm.warp(block.timestamp + 1 days + 1);
        openAsSearcher(1 ether, 0);
        vm.warp(block.timestamp + 1 days + 1);
        openAsSearcher(1 ether, 0);
        vm.warp(block.timestamp + 1 days + 1);
        openAsSearcher(1 ether, 0);
        vm.warp(block.timestamp + 1 days + 1);
        openAsSearcher(1 ether, 0); // total 5 ETH == TOTAL

        // Roll the daily window so the daily ceiling has fresh room and the
        // *lifetime* ceiling is the one that bites.
        vm.warp(block.timestamp + 1 days + 1);
        uint256 sizeWei = 0.1 ether;
        uint256 expected = amountOut(sizeWei, WETH_RESERVE, TOKEN_RESERVE);
        SniperVault.Call[] memory calls = entryCalls(sizeWei, expected);
        SniperVault.EntryGuard memory guard_ = entryGuard(sizeWei, 0, 0, 0);
        vm.expectRevert(
            abi.encodeWithSelector(SniperVault.TotalBudgetExceeded.selector, TOTAL + sizeWei, TOTAL)
        );
        vm.prank(searcher);
        vault.openPosition(bytes32("tag"), calls, guard_);
    }

    function test_SlippageFloorRevertsWhenThePoolMoved() public {
        uint256 sizeWei = 0.1 ether;
        uint256 expected = amountOut(sizeWei, WETH_RESERVE, TOKEN_RESERVE);
        // Demand more tokens than the curve can give: the harness's
        // minTokensOut equivalent of a moved pool.
        vm.prank(searcher);
        vm.expectRevert(
            abi.encodeWithSelector(SniperVault.InsufficientTokens.selector, expected, expected + 1)
        );
        vault.openPosition(
            bytes32("tag"), entryCalls(sizeWei, expected), entryGuard(sizeWei, expected + 1, 0, 0)
        );
    }

    function test_BlockDeadlineRevertsALateEntry() public {
        vm.roll(1_000);
        uint256 sizeWei = 0.1 ether;
        uint256 expected = amountOut(sizeWei, WETH_RESERVE, TOKEN_RESERVE);
        vm.prank(searcher);
        vm.expectRevert(SniperVault.Deadline.selector);
        vault.openPosition(bytes32("tag"), entryCalls(sizeWei, expected), entryGuard(sizeWei, 0, 999, 0));
    }

    function test_MaxBaseFeeRevertsACongestedEntry() public {
        vm.fee(50 gwei);
        uint256 sizeWei = 0.1 ether;
        uint256 expected = amountOut(sizeWei, WETH_RESERVE, TOKEN_RESERVE);
        vm.prank(searcher);
        vm.expectRevert(SniperVault.BaseFeeTooHigh.selector);
        vault.openPosition(bytes32("tag"), entryCalls(sizeWei, expected), entryGuard(sizeWei, 0, 0, 1 gwei));
    }

    function test_ExitMinWethOutRevertsAMovedSell() public {
        (, uint256 received) = openAsSearcher(0.1 ether, 0);
        (uint112 r0, uint112 r1,) = pair.getReserves();
        (uint256 wethReserve, uint256 tokenReserve) =
            wethIsToken0() ? (uint256(r0), uint256(r1)) : (uint256(r1), uint256(r0));
        uint256 wethOut = amountOut(received, tokenReserve, wethReserve);

        vm.prank(searcher);
        vm.expectRevert(abi.encodeWithSelector(SniperVault.InsufficientWeth.selector, wethOut, wethOut + 1));
        vault.closePosition(
            bytes32("tag"), exitCalls(received, wethOut), exitGuard(received, wethOut + 1, 0, 0)
        );
    }

    function test_HoneypotTokenRevertsTheExitBatch() public {
        openAsSearcher(0.1 ether, 0);
        // The launch turns hostile: the token blocks transfers out of the vault.
        token.setBlockedSender(address(vault), true);

        uint256 held = token.balanceOf(address(vault));
        vm.prank(searcher);
        vm.expectRevert(); // CallFailed wrapping the ERC20 revert
        vault.closePosition(bytes32("tag"), exitCalls(held, 1), exitGuard(held, 0, 0, 0));
        // Nothing was credited or moved.
        assertEq(token.balanceOf(address(vault)), held);
    }

    function test_FailedEntryLeavesBudgetAndBalancesUntouched() public {
        uint256 spentBefore = vault.totalSpent();
        uint256 wethBefore = weth.balanceOf(address(vault));
        uint256 sizeWei = 0.1 ether;
        uint256 expected = amountOut(sizeWei, WETH_RESERVE, TOKEN_RESERVE);

        vm.prank(searcher);
        vm.expectRevert(
            abi.encodeWithSelector(SniperVault.InsufficientTokens.selector, expected, expected + 1)
        );
        vault.openPosition(
            bytes32("tag"), entryCalls(sizeWei, expected), entryGuard(sizeWei, expected + 1, 0, 0)
        );

        assertEq(vault.totalSpent(), spentBefore, "a reverted entry books no budget");
        assertEq(weth.balanceOf(address(vault)), wethBefore, "a reverted entry moves no WETH");
    }
}

/// @dev Reentrancy coverage for the vault's transient-storage lock. The only
///      way to reach the lock is a re-entrant `openPosition`/`closePosition`
///      from inside an outer execution batch, which is exactly what a hostile
///      token or router leg would attempt.
contract SniperVaultReentrancyTest is Test {
    SniperVault vault;
    MockWETH weth;
    Attacker attacker;

    function setUp() public {
        weth = new MockWETH();
        vault = new SniperVault(address(weth), 1 ether, 0);
        attacker = new Attacker(vault);
        vault.setSearcher(address(attacker), true);
        weth.mint(address(vault), 1 ether);
    }

    function test_ReentrantEntryIsRejected() public {
        // Outer frame: an entry whose single leg calls the attacker.
        SniperVault.Call[] memory calls = new SniperVault.Call[](1);
        calls[0] = SniperVault.Call({target: address(attacker), value: 0, data: ""});

        (bool ok, bytes memory err) = address(vault)
            .call(
                abi.encodeCall(
                    vault.openPosition,
                    (
                        bytes32("outer"),
                        calls,
                        SniperVault.EntryGuard({
                        token: address(weth), maxSpend: 0, minTokensOut: 0, blockDeadline: 0, maxBaseFee: 0
                    })
                    )
                )
            );
        assertFalse(ok, "the re-entrant batch must revert");
        // The outer revert is CallFailed(0, <inner>); the inner data must be
        // the vault's own Reentrancy() error — proof the lock fired rather
        // than some unrelated failure.
        assertTrue(contains(err, SniperVault.CallFailed.selector), "outer must be CallFailed");
        assertTrue(contains(err, SniperVault.Reentrancy.selector), "inner must be Reentrancy");
    }

    function contains(bytes memory haystack, bytes4 needle) internal pure returns (bool) {
        if (haystack.length < 4) return false;
        for (uint256 i; i + 4 <= haystack.length; i++) {
            if (
                haystack[i] == needle[0] && haystack[i + 1] == needle[1] && haystack[i + 2] == needle[2]
                    && haystack[i + 3] == needle[3]
            ) {
                return true;
            }
        }
        return false;
    }
}

contract Attacker {
    SniperVault immutable vault;

    constructor(SniperVault v) {
        vault = v;
    }

    fallback() external payable {
        // Try to re-enter while the outer execution frame holds the lock.
        SniperVault.Call[] memory calls = new SniperVault.Call[](0);
        vault.openPosition(
            bytes32("reentrancy"),
            calls,
            SniperVault.EntryGuard({
                token: address(0xBEEF), maxSpend: 0, minTokensOut: 0, blockDeadline: 0, maxBaseFee: 0
            })
        );
    }
}
