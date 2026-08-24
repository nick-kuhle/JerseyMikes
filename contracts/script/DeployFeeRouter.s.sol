// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Script, console2} from "forge-std/Script.sol";
import {JerseyMikesFeeRouter} from "../src/JerseyMikesFeeRouter.sol";

/// @notice Deploys the optional atomic 1% manual-trade fee wrapper.
///
/// Required environment variables:
///   PLATFORM_FEE_RECIPIENT=0x...
///   DEPLOYER_PRIVATE_KEY=... (or Foundry's unlocked signer)
contract DeployFeeRouter is Script {
    function run() external returns (JerseyMikesFeeRouter router) {
        address recipient = vm.envAddress("PLATFORM_FEE_RECIPIENT");
        uint256 pk = vm.envOr("DEPLOYER_PRIVATE_KEY", uint256(0));
        if (pk != 0) vm.startBroadcast(pk);
        else vm.startBroadcast();
        router = new JerseyMikesFeeRouter(recipient);
        vm.stopBroadcast();
        console2.log("JerseyMikesFeeRouter deployed at", address(router));
        console2.log("  fee recipient", recipient);
        console2.log("  fee bps", router.PLATFORM_FEE_BPS());
    }
}
