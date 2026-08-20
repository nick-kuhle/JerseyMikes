// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Script, console2} from "forge-std/Script.sol";
import {MevExecutor} from "../src/MevExecutor.sol";

/// @notice Deploys MevExecutor.
///
/// Usage (dry run against a fork — no key required):
///   forge script script/Deploy.s.sol --fork-url $ETH_HTTP_URL
///
/// Usage (real deployment):
///   forge script script/Deploy.s.sol --rpc-url $ETH_HTTP_URL --broadcast --verify
contract Deploy is Script {
    // Balancer V2 vault and WETH9, identical addresses on most EVM chains that host Balancer.
    address constant BALANCER_VAULT = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;

    function run() external returns (MevExecutor executor) {
        address vault = vm.envOr("BALANCER_VAULT", BALANCER_VAULT);
        address weth = vm.envOr("WETH_ADDRESS", WETH);

        uint256 pk = vm.envOr("DEPLOYER_PRIVATE_KEY", uint256(0));
        if (pk != 0) vm.startBroadcast(pk);
        else vm.startBroadcast();

        executor = new MevExecutor(vault, weth);

        address searcher = vm.envOr("SEARCHER_ADDRESS", address(0));
        if (searcher != address(0)) executor.setSearcher(searcher, true);

        vm.stopBroadcast();

        console2.log("MevExecutor deployed at", address(executor));
        console2.log("  vault   ", vault);
        console2.log("  weth    ", weth);
        console2.log("  searcher", searcher);
    }
}
