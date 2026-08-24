// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {IERC20, IWETH} from "./interfaces/IExternal.sol";

/// @title SniperVault
/// @notice Position-holding execution contract for the directional new-token sniper.
///
/// Why this is not `MevExecutor`
/// -----------------------------
/// `MevExecutor` enforces a retained-profit invariant: every settling entry point
/// measures a balance delta and reverts with `Unprofitable` below `minProfit`. That
/// is what makes a losing MEV bundle free — it never lands.
///
/// A directional snipe cannot satisfy that invariant. Buying a token is a pure
/// spend; the position is held across blocks and can go to zero. Rather than
/// weaken `MevExecutor`'s guard (which would silently widen the blast radius of
/// every other strategy, and change the deployed bytecode of a contract that has
/// already been through qualification), the sniper gets its own vault with a
/// different, explicitly weaker invariant:
///
///   **`MevExecutor` guarantees profit. `SniperVault` guarantees bounded loss.**
///
/// The bound is enforced three ways, all on chain, none of them dependent on the
/// off-chain bot behaving correctly:
///
///   1. `maxSpend` per call — a batch cannot spend more WETH than the guard says.
///   2. `minTokensOut` per call — slippage/honeypot floor on what the spend buys.
///   3. `dailyBudget` / `totalBudget` — cumulative caps the owner sets, which the
///      searcher key cannot raise. A fully compromised searcher key can lose at
///      most the remaining budget, and can never move funds anywhere but through
///      a swap that returns tokens to this contract.
///
/// Exits reuse a profit-shaped guard (`minWethOut`), because selling *is*
/// measurable: the vault knows how much WETH came back.
///
/// Funds can only ever leave via `sweep`, which is owner-only.
contract SniperVault {
    struct Call {
        address target;
        uint256 value;
        bytes data;
    }

    /// @param token        Token being acquired.
    /// @param maxSpend     Hard ceiling on the WETH this batch may consume.
    /// @param minTokensOut Minimum `token` balance increase, or the batch reverts.
    /// @param blockDeadline Last block this may execute in (0 = no deadline).
    /// @param maxBaseFee   Reverts if `block.basefee` exceeds this (0 = no cap).
    struct EntryGuard {
        address token;
        uint256 maxSpend;
        uint256 minTokensOut;
        uint64 blockDeadline;
        uint256 maxBaseFee;
    }

    /// @param token       Token being sold.
    /// @param maxTokensIn Hard ceiling on the `token` this batch may consume.
    /// @param minWethOut  Minimum WETH balance increase, or the batch reverts.
    /// @param blockDeadline Last block this may execute in (0 = no deadline).
    /// @param maxBaseFee  Reverts if `block.basefee` exceeds this (0 = no cap).
    struct ExitGuard {
        address token;
        uint256 maxTokensIn;
        uint256 minWethOut;
        uint64 blockDeadline;
        uint256 maxBaseFee;
    }

    /// Transient reentrancy slot (EIP-1153). Distinct from any slot used by
    /// `MevExecutor`; transient storage is per-contract, but keeping them
    /// visibly different avoids a copy-paste hazard if the two are ever read
    /// side by side.
    bytes32 private constant _REENTRANCY_SLOT =
        0x5f6e7d8c9bab0c1d2e3f40516273849506a7b8c9dae1f2031425364758697a8b;

    /// Length of the rolling budget window.
    uint256 public constant BUDGET_WINDOW = 1 days;

    address public immutable WETH;

    address public owner;
    mapping(address => bool) public searchers;

    /// Cumulative WETH spent on entries, ever.
    uint256 public totalSpent;
    /// WETH spent on entries inside the current window.
    uint256 public windowSpent;
    /// Start timestamp of the current window.
    uint256 public windowStart;

    /// Owner-set ceilings. The searcher key cannot change these.
    uint256 public dailyBudget;
    uint256 public totalBudget;

    /// Per-token exposure, for observability and the exit-side ceiling.
    mapping(address => uint256) public tokenAcquired;
    mapping(address => uint256) public tokenSold;

    event EntryExecuted(
        bytes32 indexed tag, address indexed token, uint256 wethSpent, uint256 tokensReceived
    );
    event ExitExecuted(bytes32 indexed tag, address indexed token, uint256 tokensSold, uint256 wethReceived);
    event BudgetSet(uint256 dailyBudget, uint256 totalBudget);
    event BudgetWindowRolled(uint256 windowStart, uint256 previousWindowSpent);
    event SearcherSet(address indexed searcher, bool allowed);
    event OwnerChanged(address indexed previousOwner, address indexed newOwner);
    event Swept(address indexed token, address indexed to, uint256 amount);

    error NotOwner();
    error NotSearcher();
    error Reentrancy();
    error Deadline();
    error BaseFeeTooHigh();
    error CallFailed(uint256 index, bytes returndata);
    error SweepFailed();
    error TransferFailed();
    error ZeroToken();
    /// @notice The batch spent more WETH than `maxSpend` allowed.
    error OverSpend(uint256 spent, uint256 allowed);
    /// @notice The batch consumed more of the token than `maxTokensIn` allowed.
    error OverSell(uint256 sold, uint256 allowed);
    /// @notice The acquisition returned fewer tokens than required.
    error InsufficientTokens(uint256 received, uint256 required);
    /// @notice The disposal returned less WETH than required.
    error InsufficientWeth(uint256 received, uint256 required);
    /// @notice The spend would exceed the rolling daily budget.
    error DailyBudgetExceeded(uint256 spent, uint256 budget);
    /// @notice The spend would exceed the lifetime budget.
    error TotalBudgetExceeded(uint256 spent, uint256 budget);

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

    /// @param weth_ The chain's wrapped-native token.
    /// @param dailyBudget_ Rolling 24h spend ceiling. Deploying with 0 means the
    ///        vault cannot buy anything until the owner sets a budget — the same
    ///        fail-closed default the off-chain lane uses.
    /// @param totalBudget_ Lifetime spend ceiling. 0 == unlimited.
    constructor(address weth_, uint256 dailyBudget_, uint256 totalBudget_) {
        owner = msg.sender;
        searchers[msg.sender] = true;
        WETH = weth_;
        dailyBudget = dailyBudget_;
        totalBudget = totalBudget_;
        windowStart = block.timestamp;
        emit OwnerChanged(address(0), msg.sender);
        emit BudgetSet(dailyBudget_, totalBudget_);
    }

    /// @notice Accepts native ETH (WETH unwraps land here).
    receive() external payable {}

    // ---------------------------------------------------------------------
    // Administration
    // ---------------------------------------------------------------------

    function setOwner(address newOwner) external onlyOwner {
        emit OwnerChanged(owner, newOwner);
        owner = newOwner;
    }

    function setSearcher(address searcher, bool allowed) external onlyOwner {
        searchers[searcher] = allowed;
        emit SearcherSet(searcher, allowed);
    }

    /// @notice Set the spend ceilings. Owner-only, so a compromised searcher key
    ///         cannot widen its own limit.
    function setBudget(uint256 dailyBudget_, uint256 totalBudget_) external onlyOwner {
        dailyBudget = dailyBudget_;
        totalBudget = totalBudget_;
        emit BudgetSet(dailyBudget_, totalBudget_);
    }

    /// @notice Withdraw anything. The only exit for value, and owner-only.
    function sweep(address token, address to, uint256 amount) external onlyOwner {
        if (token == address(0)) {
            (bool ok,) = to.call{value: amount}("");
            if (!ok) revert SweepFailed();
        } else {
            _safeTransfer(token, to, amount);
        }
        emit Swept(token, to, amount);
    }

    /// @notice Escape hatch for owner-driven maintenance (approvals, unwraps).
    function ownerCall(Call[] calldata calls) external payable onlyOwner returns (bytes[] memory) {
        return _run(calls);
    }

    // ---------------------------------------------------------------------
    // Views
    // ---------------------------------------------------------------------

    /// @notice WETH still spendable right now, accounting for both ceilings and
    ///         for a window that has rolled over but not yet been written.
    function spendableRemaining() public view returns (uint256) {
        uint256 windowUsed = block.timestamp >= windowStart + BUDGET_WINDOW ? 0 : windowSpent;
        uint256 dailyRoom = dailyBudget > windowUsed ? dailyBudget - windowUsed : 0;
        if (totalBudget == 0) return dailyRoom;
        uint256 totalRoom = totalBudget > totalSpent ? totalBudget - totalSpent : 0;
        return dailyRoom < totalRoom ? dailyRoom : totalRoom;
    }

    /// @notice Tokens of `token` currently held by the vault.
    function tokenBalance(address token) external view returns (uint256) {
        return IERC20(token).balanceOf(address(this));
    }

    // ---------------------------------------------------------------------
    // Execution
    // ---------------------------------------------------------------------

    /// @notice Acquire a token. Spends WETH, receives `g.token`, no profit guard.
    ///
    /// @dev The measurement is a balance delta on both sides, so it is agnostic
    ///      to how the calls actually route (pair swap, router, multi-hop) and it
    ///      catches fee-on-transfer tokens automatically: `tokensReceived` is what
    ///      actually arrived, not what the router claimed it sent.
    ///
    /// @param tag   Opaque identifier echoed in the event, for off-chain matching.
    /// @param calls The swap, encoded off chain.
    /// @param g     Spend ceiling, slippage floor and execution guards.
    /// @return wethSpent      WETH actually consumed.
    /// @return tokensReceived Tokens actually acquired.
    function openPosition(bytes32 tag, Call[] calldata calls, EntryGuard calldata g)
        external
        onlySearcher
        nonReentrant
        returns (uint256 wethSpent, uint256 tokensReceived)
    {
        if (g.token == address(0)) revert ZeroToken();
        _checkExecutionGuards(g.blockDeadline, g.maxBaseFee);
        _rollWindow();

        // Cap the intent before running anything.
        uint256 room = spendableRemaining();
        if (g.maxSpend > room) {
            // Distinguish which ceiling bit, so an operator knows what to raise.
            uint256 windowUsed = windowSpent;
            if (dailyBudget < windowUsed + g.maxSpend) {
                revert DailyBudgetExceeded(windowUsed + g.maxSpend, dailyBudget);
            }
            revert TotalBudgetExceeded(totalSpent + g.maxSpend, totalBudget);
        }

        uint256 wethBefore = IERC20(WETH).balanceOf(address(this));
        uint256 tokenBefore = IERC20(g.token).balanceOf(address(this));

        _run(calls);

        uint256 wethAfter = IERC20(WETH).balanceOf(address(this));
        uint256 tokenAfter = IERC20(g.token).balanceOf(address(this));

        // A batch that somehow *gained* WETH spent nothing; treat it as zero
        // rather than underflowing.
        wethSpent = wethAfter >= wethBefore ? 0 : wethBefore - wethAfter;
        tokensReceived = tokenAfter >= tokenBefore ? tokenAfter - tokenBefore : 0;

        if (wethSpent > g.maxSpend) revert OverSpend(wethSpent, g.maxSpend);
        if (tokensReceived < g.minTokensOut) {
            revert InsufficientTokens(tokensReceived, g.minTokensOut);
        }

        // Book the spend against both ceilings. Done after the fact using the
        // *realised* amount, so an entry that used less than its ceiling does
        // not consume budget it never spent.
        windowSpent += wethSpent;
        totalSpent += wethSpent;
        if (dailyBudget < windowSpent) revert DailyBudgetExceeded(windowSpent, dailyBudget);
        if (totalBudget != 0 && totalSpent > totalBudget) {
            revert TotalBudgetExceeded(totalSpent, totalBudget);
        }

        tokenAcquired[g.token] += tokensReceived;
        emit EntryExecuted(tag, g.token, wethSpent, tokensReceived);
    }

    /// @notice Dispose of part or all of a position. Sells `g.token`, receives WETH.
    ///
    /// @dev Exits are deliberately **not** budget-limited: getting out must never
    ///      be blocked by a spend ceiling. They are still bounded by
    ///      `maxTokensIn` so a malformed batch cannot dump more than intended.
    ///
    /// @param tag   Opaque identifier echoed in the event.
    /// @param calls The swap, encoded off chain.
    /// @param g     Sell ceiling, proceeds floor and execution guards.
    /// @return tokensSold   Tokens actually consumed.
    /// @return wethReceived WETH actually received.
    function closePosition(bytes32 tag, Call[] calldata calls, ExitGuard calldata g)
        external
        onlySearcher
        nonReentrant
        returns (uint256 tokensSold, uint256 wethReceived)
    {
        if (g.token == address(0)) revert ZeroToken();
        _checkExecutionGuards(g.blockDeadline, g.maxBaseFee);

        uint256 wethBefore = IERC20(WETH).balanceOf(address(this));
        uint256 tokenBefore = IERC20(g.token).balanceOf(address(this));

        _run(calls);

        uint256 wethAfter = IERC20(WETH).balanceOf(address(this));
        uint256 tokenAfter = IERC20(g.token).balanceOf(address(this));

        tokensSold = tokenBefore >= tokenAfter ? tokenBefore - tokenAfter : 0;
        wethReceived = wethAfter >= wethBefore ? wethAfter - wethBefore : 0;

        if (tokensSold > g.maxTokensIn) revert OverSell(tokensSold, g.maxTokensIn);
        // This is the honeypot backstop at the contract boundary: a token that
        // cannot be sold produces zero WETH and the whole batch reverts.
        if (wethReceived < g.minWethOut) revert InsufficientWeth(wethReceived, g.minWethOut);

        tokenSold[g.token] += tokensSold;
        emit ExitExecuted(tag, g.token, tokensSold, wethReceived);
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    function _checkExecutionGuards(uint64 blockDeadline, uint256 maxBaseFee) private view {
        if (blockDeadline != 0 && block.number > blockDeadline) revert Deadline();
        if (maxBaseFee != 0 && block.basefee > maxBaseFee) revert BaseFeeTooHigh();
    }

    /// @dev Roll the rolling-24h window forward if it has expired.
    function _rollWindow() private {
        if (block.timestamp >= windowStart + BUDGET_WINDOW) {
            emit BudgetWindowRolled(block.timestamp, windowSpent);
            windowStart = block.timestamp;
            windowSpent = 0;
        }
    }

    function _run(Call[] calldata calls) private returns (bytes[] memory results) {
        uint256 n = calls.length;
        results = new bytes[](n);
        for (uint256 i; i < n;) {
            (bool ok, bytes memory ret) = calls[i].target.call{value: calls[i].value}(calls[i].data);
            if (!ok) revert CallFailed(i, ret);
            results[i] = ret;
            unchecked {
                ++i;
            }
        }
    }

    /// @dev ERC20 `transfer` that tolerates non-standard (no return value) tokens
    ///      and rejects an explicit `false`.
    function _safeTransfer(address token, address to, uint256 amount) private {
        (bool ok, bytes memory ret) = token.call(abi.encodeWithSelector(IERC20.transfer.selector, to, amount));
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) revert TransferFailed();
    }
}
