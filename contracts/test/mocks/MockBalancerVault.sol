// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {MockERC20} from "./MockERC20.sol";

interface IRecipient {
    function receiveFlashLoan(
        address[] memory tokens,
        uint256[] memory amounts,
        uint256[] memory feeAmounts,
        bytes memory userData
    ) external;
}

/// @dev Balancer-V2-style zero-fee flash lender.
contract MockBalancerVault {
    function flashLoan(
        address recipient,
        address[] memory tokens,
        uint256[] memory amounts,
        bytes memory userData
    ) external {
        uint256[] memory fees = new uint256[](tokens.length);
        uint256[] memory pre = new uint256[](tokens.length);
        for (uint256 i; i < tokens.length; ++i) {
            pre[i] = MockERC20(tokens[i]).balanceOf(address(this));
            MockERC20(tokens[i]).transfer(recipient, amounts[i]);
        }
        IRecipient(recipient).receiveFlashLoan(tokens, amounts, fees, userData);
        for (uint256 i; i < tokens.length; ++i) {
            require(MockERC20(tokens[i]).balanceOf(address(this)) >= pre[i], "not repaid");
        }
    }
}
