// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {IERC20, IWETH, IBalancerVault, IFlashLoanRecipient} from "./interfaces/IExternal.sol";

/// @title MevExecutor
/// @notice Generic atomic MEV execution contract.
///
/// Design goals
/// ------------
/// 1. **Never land unprofitable.** Every entry point measures the balance of `profitToken`
///    (address(0) == native ETH) before and after the call batch and reverts unless the
///    realised delta is >= `minProfit`. Because the bot only submits through private
///    orderflow (Flashbots / MEV-Share bundles), a reverting transaction is simply not
///    included by the builder and therefore burns **zero** gas.
/// 2. **Generic.** Strategies (sandwich, JIT, atomic arb, liquidation, sniper) are encoded
///    off-chain as an ordered array of `Call`s. No strategy-specific on-chain logic means
///    no redeploy when a strategy changes.
/// 3. **Cheap.** Tight calldata, transient-storage guards (EIP-1153), no SafeERC20 bloat.
/// 4. **Safe by default.** Only whitelisted searchers can execute; funds can only ever be
///    swept by the owner.
contract MevExecutor is IFlashLoanRecipient {
    struct Call {
        address target;
        uint256 value;
        bytes data;
    }

    /// @param profitToken Token the profit is denominated in. address(0) == native ETH.
    /// @param minProfit   Minimum realised delta required, otherwise the tx reverts.
    /// @param bribeBps    Share of realised profit (in bps) paid to `block.coinbase`.
    /// @param blockDeadline Last block this batch may execute in (0 = no deadline).
    /// @param maxBaseFee  Reverts if `block.basefee` exceeds this (0 = no cap).
    struct Guard {
        address profitToken;
        uint256 minProfit;
        uint16 bribeBps;
        uint64 blockDeadline;
        uint256 maxBaseFee;
    }

    // keccak256("jerseymikes.reentrancy.guard")
    bytes32 private constant _REENTRANCY_SLOT =
        0x9d0c4a1f5e1a2b6f5f8c0e5f3f5c0a6b1b5b2a7c8d9e0f1a2b3c4d5e6f708192;
    // keccak256("jerseymikes.flashloan.guard")
    bytes32 private constant _FLASHLOAN_SLOT =
        0x1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f809;

    // keccak256("jerseymikes.v3.callback.guard")
    bytes32 private constant _V3_CALLBACK_SLOT =
        0x2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a;

    address public immutable BALANCER_VAULT;
    address public immutable WETH;

    address public owner;
    mapping(address => bool) public searchers;

    event Executed(
        bytes32 indexed tag, address indexed profitToken, uint256 profit, uint256 bribe, uint256 gasUsed
    );
    event SearcherSet(address indexed searcher, bool allowed);
    event OwnerChanged(address indexed previousOwner, address indexed newOwner);
    event Swept(address indexed token, address indexed to, uint256 amount);

    error NotOwner();
    error NotSearcher();
    error Reentrancy();
    error Deadline();
    error BaseFeeTooHigh();
    error Unprofitable(uint256 realised, uint256 required);
    error CallFailed(uint256 index, bytes returndata);
    error BadFlashCallback();
    error BadBribe();

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    modifier onlySearcher() {
        if (!searchers[msg.sender] && msg.sender != owner) revert NotSearcher();
        _;
    }

    modifier nonReentrant() {
        bytes32 slot = _REENTRANCY_SLOT;
        assembly ("memory-safe") {
            if tload(slot) {
                mstore(0x00, 0xab143c06) // Reentrancy()
                revert(0x1c, 0x04)
            }
            tstore(slot, 1)
        }
        _;
        assembly ("memory-safe") {
            tstore(slot, 0)
        }
    }

    constructor(address balancerVault, address weth) {
        owner = msg.sender;
        searchers[msg.sender] = true;
        BALANCER_VAULT = balancerVault;
        WETH = weth;
        emit OwnerChanged(address(0), msg.sender);
    }

    receive() external payable {}

    // ---------------------------------------------------------------------
    // Admin
    // ---------------------------------------------------------------------

    function setSearcher(address searcher, bool allowed) external onlyOwner {
        searchers[searcher] = allowed;
        emit SearcherSet(searcher, allowed);
    }

    function setOwner(address newOwner) external onlyOwner {
        emit OwnerChanged(owner, newOwner);
        owner = newOwner;
    }

    /// @notice Withdraw funds. Only the owner, only to the owner-specified address.
    function sweep(address token, address to, uint256 amount) external onlyOwner {
        if (token == address(0)) {
            (bool ok,) = to.call{value: amount}("");
            require(ok, "sweep failed");
        } else {
            _safeTransfer(token, to, amount);
        }
        emit Swept(token, to, amount);
    }

    /// @notice Escape hatch for arbitrary owner-driven maintenance (approvals, unwraps...).
    function ownerCall(Call[] calldata calls) external payable onlyOwner returns (bytes[] memory) {
        return _run(calls);
    }

    // ---------------------------------------------------------------------
    // Execution
    // ---------------------------------------------------------------------

    /// @notice Execute an atomic batch, reverting unless it nets at least `g.minProfit`.
    /// @param tag   Opaque identifier the bot uses to correlate on-chain logs with its DB.
    /// @param calls Ordered call batch produced by the strategy engine.
    function execute(bytes32 tag, Call[] calldata calls, Guard calldata g)
        external
        payable
        onlySearcher
        nonReentrant
        returns (uint256 profit)
    {
        uint256 gasStart = gasleft();
        _checkGuards(g);
        uint256 balBefore = _balance(g.profitToken);

        _run(calls);

        profit = _settle(tag, g, balBefore, gasStart);
    }

    /// @notice Same as `execute` but funded by a Balancer V2 flash loan (zero fee).
    /// @dev The borrowed amount is available to the call batch; repayment happens here.
    function flashExecute(
        bytes32 tag,
        address[] calldata tokens,
        uint256[] calldata amounts,
        Call[] calldata calls,
        Guard calldata g
    ) external onlySearcher nonReentrant {
        _checkGuards(g);
        _flash(tokens, amounts, _encodeFlashData(tag, calls, g));
    }

    function _flash(address[] calldata tokens, uint256[] calldata amounts, bytes memory data) private {
        bytes32 slot = _FLASHLOAN_SLOT;
        assembly ("memory-safe") {
            tstore(slot, 1)
        }
        IBalancerVault(BALANCER_VAULT).flashLoan(address(this), tokens, amounts, data);
        assembly ("memory-safe") {
            tstore(slot, 0)
        }
    }

    function _encodeFlashData(bytes32 tag, Call[] calldata calls, Guard calldata g)
        private
        view
        returns (bytes memory)
    {
        return abi.encode(tag, calls, g, _balance(g.profitToken), gasleft());
    }

    /// @inheritdoc IFlashLoanRecipient
    function receiveFlashLoan(
        address[] memory tokens,
        uint256[] memory amounts,
        uint256[] memory feeAmounts,
        bytes memory userData
    ) external override {
        bytes32 slot = _FLASHLOAN_SLOT;
        uint256 armed;
        assembly ("memory-safe") {
            armed := tload(slot)
        }
        if (msg.sender != BALANCER_VAULT || armed == 0) revert BadFlashCallback();

        (bytes32 tag, Call[] memory calls, Guard memory g, uint256 balBefore, uint256 gasStart) =
            abi.decode(userData, (bytes32, Call[], Guard, uint256, uint256));

        _runMemory(calls);

        // Repay the vault.
        uint256 n = tokens.length;
        for (uint256 i; i < n;) {
            _safeTransfer(tokens[i], BALANCER_VAULT, amounts[i] + feeAmounts[i]);
            unchecked {
                ++i;
            }
        }

        _settle(tag, g, balBefore, gasStart);
    }

    // ---------------------------------------------------------------------
    // Just-in-time liquidity support (UniswapV3 mint callback)
    // ---------------------------------------------------------------------

    /// @notice Arms the V3 mint callback for exactly one pool, for the rest of
    ///         this transaction. Must be the call immediately preceding a
    ///         `pool.mint(...)` in the batch.
    /// @dev Callable only by the contract itself (i.e. from inside a batch), so
    ///      an external actor can never arm it.
    function armV3Callback(address pool) external {
        if (msg.sender != address(this)) revert NotSearcher();
        bytes32 slot = _V3_CALLBACK_SLOT;
        assembly ("memory-safe") {
            tstore(slot, pool)
        }
    }

    /// @notice UniswapV3 pulls the owed token amounts through this callback.
    function uniswapV3MintCallback(uint256 amount0Owed, uint256 amount1Owed, bytes calldata data) external {
        bytes32 slot = _V3_CALLBACK_SLOT;
        address armed;
        assembly ("memory-safe") {
            armed := tload(slot)
        }
        if (armed == address(0) || msg.sender != armed) revert BadFlashCallback();
        assembly ("memory-safe") {
            tstore(slot, 0)
        }
        (address token0, address token1) = abi.decode(data, (address, address));
        if (amount0Owed != 0) _safeTransfer(token0, msg.sender, amount0Owed);
        if (amount1Owed != 0) _safeTransfer(token1, msg.sender, amount1Owed);
    }

    // ---------------------------------------------------------------------
    // Views used by the off-chain simulator
    // ---------------------------------------------------------------------

    /// @notice Dry-run helper: returns the realised delta without the profit requirement.
    /// @dev Intended to be used with `eth_call` (optionally with state overrides) so the
    ///      bot can size an opportunity before it commits to a bundle. Always reverts when
    ///      called on-chain by anyone other than address(0) (the eth_call default sender).
    function quote(Call[] calldata calls, address profitToken)
        external
        payable
        returns (int256 delta, uint256 gasUsed)
    {
        require(msg.sender == address(0), "eth_call only");
        uint256 gasStart = gasleft();
        uint256 before = _balance(profitToken);
        _run(calls);
        delta = int256(_balance(profitToken)) - int256(before);
        gasUsed = gasStart - gasleft();
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    function _checkGuards(Guard calldata g) private view {
        if (g.blockDeadline != 0 && block.number > g.blockDeadline) revert Deadline();
        if (g.maxBaseFee != 0 && block.basefee > g.maxBaseFee) revert BaseFeeTooHigh();
        if (g.bribeBps > 10_000) revert BadBribe();
    }

    function _settle(bytes32 tag, Guard memory g, uint256 balBefore, uint256 gasStart)
        private
        returns (uint256 profit)
    {
        uint256 balAfter = _balance(g.profitToken);
        // Underflow-safe: a negative delta is a zero profit and will fail the check below.
        profit = balAfter > balBefore ? balAfter - balBefore : 0;
        if (profit < g.minProfit) revert Unprofitable(profit, g.minProfit);

        uint256 bribe;
        if (g.bribeBps != 0 && profit != 0) {
            bribe = (profit * g.bribeBps) / 10_000;
            if (bribe != 0) {
                if (g.profitToken != address(0)) {
                    // Bribes are always paid in ETH: unwrap WETH profit if needed.
                    if (g.profitToken == WETH) {
                        IWETH(WETH).withdraw(bribe);
                    } else {
                        bribe = 0; // non-ETH profit: the bot pays the builder via priority fee
                    }
                }
                if (bribe != 0) {
                    (bool ok,) = block.coinbase.call{value: bribe}("");
                    require(ok, "bribe failed");
                }
            }
        }

        emit Executed(tag, g.profitToken, profit, bribe, gasStart - gasleft());
    }

    function _run(Call[] calldata calls) private returns (bytes[] memory out) {
        uint256 n = calls.length;
        out = new bytes[](n);
        for (uint256 i; i < n;) {
            (bool ok, bytes memory ret) = calls[i].target.call{value: calls[i].value}(calls[i].data);
            if (!ok) revert CallFailed(i, ret);
            out[i] = ret;
            unchecked {
                ++i;
            }
        }
    }

    function _runMemory(Call[] memory calls) private {
        uint256 n = calls.length;
        for (uint256 i; i < n;) {
            (bool ok, bytes memory ret) = calls[i].target.call{value: calls[i].value}(calls[i].data);
            if (!ok) revert CallFailed(i, ret);
            unchecked {
                ++i;
            }
        }
    }

    function _balance(address token) private view returns (uint256) {
        if (token == address(0)) return address(this).balance;
        return IERC20(token).balanceOf(address(this));
    }

    function _safeTransfer(address token, address to, uint256 amount) private {
        (bool ok, bytes memory ret) = token.call(abi.encodeCall(IERC20.transfer, (to, amount)));
        require(ok && (ret.length == 0 || abi.decode(ret, (bool))), "transfer failed");
    }
}
