// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";
import {JerseyMikesFeeRouter} from "../src/JerseyMikesFeeRouter.sol";
import {MockERC20} from "./mocks/MockERC20.sol";

contract FeeRouterTargetMock {
    uint256 public nativeReceived;

    function returnNative(address payable recipient) external payable {
        nativeReceived += msg.value;
        (bool ok,) = recipient.call{value: msg.value}("");
        require(ok, "native return failed");
    }

    function returnToken(address token, address recipient, uint256 amount) external {
        (bool ok, bytes memory data) = token.call(
            abi.encodeWithSignature("transferFrom(address,address,uint256)", msg.sender, recipient, amount)
        );
        require(ok && (data.length == 0 || abi.decode(data, (bool))), "token return failed");
    }
}

contract JerseyMikesFeeRouterTest is Test {
    JerseyMikesFeeRouter internal feeRouter;
    FeeRouterTargetMock internal target;
    FailingTarget internal failingTarget;
    MockERC20 internal token;
    address internal treasury = address(0xBEEF);

    function setUp() external {
        feeRouter = new JerseyMikesFeeRouter(treasury);
        target = new FeeRouterTargetMock();
        failingTarget = new FailingTarget();
        token = new MockERC20("Test", "TST");
        feeRouter.setRouter(address(target), true);
    }

    function test_nativeFeeIsAtomicAndExact() external {
        uint256 gross = 1 ether;
        bytes memory callData = abi.encodeCall(target.returnNative, (payable(address(this))));

        feeRouter.executeSwapWithFee{value: gross}(
            address(0), address(token), gross, 0, address(target), callData
        );

        assertEq(treasury.balance, 0.01 ether);
        assertEq(target.nativeReceived(), 0.99 ether);
    }

    function test_erc20FeeIsTakenFromGrossAndAllowanceIsCleared() external {
        uint256 gross = 1 ether;
        token.mint(address(this), gross);
        token.approve(address(feeRouter), gross);
        bytes memory callData =
            abi.encodeCall(target.returnToken, (address(token), address(this), 0.99 ether));

        feeRouter.executeSwapWithFee(address(token), address(0), gross, 0, address(target), callData);

        assertEq(token.balanceOf(treasury), 0.01 ether);
        assertEq(token.balanceOf(address(this)), 0.99 ether);
        assertEq(token.allowance(address(feeRouter), address(target)), 0);
    }

    function test_unapprovedRouterCannotExecute() external {
        vm.expectRevert(JerseyMikesFeeRouter.RouterNotAllowed.selector);
        feeRouter.executeSwapWithFee{value: 1 ether}(
            address(0), address(token), 1 ether, 0, address(0x1234), ""
        );
    }

    function test_feeRoundsDownOnOddAmounts() external {
        // 1% of 1.000000000000000099 ETH must round down to 0.01 ETH + 99 wei:
        // the treasury never gets more than the stated bps.
        uint256 gross = 1 ether + 99 wei;
        bytes memory callData = abi.encodeCall(target.returnNative, (payable(address(this))));
        feeRouter.executeSwapWithFee{value: gross}(
            address(0), address(token), gross, 0, address(target), callData
        );
        assertEq(treasury.balance, (gross * 100) / 10_000);
        assertEq(target.nativeReceived(), gross - (gross * 100) / 10_000);
    }

    function test_failingRouterCallBubblesSwapFailed() external {
        // A router that reverts must surface as SwapFailed — never as a
        // silent success that strands the trader's input in the wrapper.
        feeRouter.setRouter(address(failingTarget), true);
        (bool ok, bytes memory err) = address(feeRouter).call{value: 1 ether}(
            abi.encodeCall(
                feeRouter.executeSwapWithFee,
                (
                    address(0),
                    address(token),
                    1 ether,
                    0,
                    address(failingTarget),
                    abi.encodeCall(failingTarget.alwaysReverts, ())
                )
            )
        );
        assertFalse(ok, "a reverting router must revert the fee trade");
        assertTrue(containsSelector(err, JerseyMikesFeeRouter.SwapFailed.selector));
    }

    function test_reentrantExecutionIsRejected() external {
        // The target's "swap" tries to re-enter the fee router. The lock must
        // fire before any second fee can be taken.
        ReentrantRouter reentrant = new ReentrantRouter(feeRouter, address(target));
        feeRouter.setRouter(address(reentrant), true);
        bytes memory callData = abi.encodeCall(reentrant.attack, (payable(address(this))));
        (bool ok, bytes memory err) = address(feeRouter).call{value: 1 ether}(
            abi.encodeCall(
                feeRouter.executeSwapWithFee,
                (address(0), address(token), 1 ether, 0, address(reentrant), callData)
            )
        );
        assertFalse(ok, "the re-entrant swap must revert");
        // The outer frame wraps the router failure as SwapFailed; the inner
        // data must be the router's own Reentrancy() error.
        assertTrue(containsSelector(err, JerseyMikesFeeRouter.SwapFailed.selector));
        assertTrue(containsSelector(err, JerseyMikesFeeRouter.Reentrancy.selector));
    }

    function test_NativeValueMismatchIsRejected() external {
        vm.expectRevert(JerseyMikesFeeRouter.ValueMismatch.selector);
        feeRouter.executeSwapWithFee{value: 0.5 ether}(
            address(0), address(token), 1 ether, 0, address(target), ""
        );
    }

    function containsSelector(bytes memory haystack, bytes4 needle) internal pure returns (bool) {
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

    receive() external payable {}
}

contract FailingTarget {
    function alwaysReverts() external pure {
        revert("router exploded");
    }
}

/// @dev A "router" whose swap re-enters the fee router, trying to take a
///      second fee inside the first one's execution frame.
contract ReentrantRouter {
    JerseyMikesFeeRouter immutable feeRouter;
    address immutable inner;

    constructor(JerseyMikesFeeRouter r, address innerTarget) {
        feeRouter = r;
        inner = innerTarget;
    }

    function attack(address payable recipient) external payable {
        // First: the re-entrant call that must be rejected by the lock.
        feeRouter.executeSwapWithFee{value: msg.value / 2}(
            address(0), address(0), msg.value / 2, 0, inner, ""
        );
    }

    receive() external payable {}
}
