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
    MockERC20 internal token;
    address internal treasury = address(0xBEEF);

    function setUp() external {
        feeRouter = new JerseyMikesFeeRouter(treasury);
        target = new FeeRouterTargetMock();
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

    receive() external payable {}
}
