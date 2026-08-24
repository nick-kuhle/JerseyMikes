// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Script, console2} from "forge-std/Script.sol";
import {SniperVault} from "../src/SniperVault.sol";

/// @notice Deploys the isolated directional sniper vault.
///
/// Dry run:
///   forge script script/DeploySniperVault.s.sol --fork-url $ETH_HTTP_URL
///
/// Mainnet/Base deployment:
///   forge script script/DeploySniperVault.s.sol --rpc-url $RPC_URL --broadcast
///
/// The deployer is the initial owner and searcher. If a dedicated
/// `SNIPER_SEARCHER_ADDRESS` is supplied, it is allowlisted in the same
/// broadcast. The private key is read only by Foundry; it is never part of the
/// deployed bytecode or the dashboard configuration.
contract DeploySniperVault is Script {
    address constant MAINNET_WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant BASE_WETH = 0x4200000000000000000000000000000000000006;

    function run() external returns (SniperVault vault) {
        uint256 chainId = block.chainid;
        address defaultWeth = chainId == 8453 ? BASE_WETH : MAINNET_WETH;
        address weth = vm.envOr("WETH_ADDRESS", defaultWeth);
        uint256 dailyBudget = vm.envOr("SNIPER_DAILY_BUDGET_WEI", uint256(0));
        uint256 totalBudget = vm.envOr("SNIPER_TOTAL_BUDGET_WEI", uint256(0));
        uint256 pk = vm.envOr("DEPLOYER_PRIVATE_KEY", uint256(0));

        if (pk != 0) vm.startBroadcast(pk);
        else vm.startBroadcast();

        vault = new SniperVault(weth, dailyBudget, totalBudget);

        address dedicatedSearcher = vm.envOr("SNIPER_SEARCHER_ADDRESS", address(0));
        if (dedicatedSearcher != address(0)) {
            vault.setSearcher(dedicatedSearcher, true);
        }

        vm.stopBroadcast();

        console2.log("SniperVault deployed at", address(vault));
        console2.log("  weth", weth);
        console2.log("  daily budget", dailyBudget);
        console2.log("  total budget", totalBudget);
        console2.log("  dedicated searcher", dedicatedSearcher);
    }
}
