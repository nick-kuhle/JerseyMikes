// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @title JerseyMikesFeeRouter
/// @notice Atomic 1% fee wrapper for the dashboard's manual V2 trades.
///
/// This contract is intentionally separate from MevExecutor and SniperVault.
/// It is not used by either automated lane. It holds no standing trading
/// inventory: each call pulls/receives the user's input, pays the fee, invokes
/// an owner-approved router, and leaves the swap output to the recipient
/// encoded in the router calldata.
///
/// `swapCalldata` is not accepted from arbitrary targets. The owner must first
/// allowlist the exact router deployment for the chain. The dashboard only
/// constructs calldata for those known router ABIs and blocks fee-enabled
/// execution when this wrapper is not configured.
contract JerseyMikesFeeRouter {
    uint256 public constant PLATFORM_FEE_BPS = 100;
    uint256 public constant BPS = 10_000;

    address public immutable feeRecipient;
    address public owner;
    mapping(address => bool) public allowedRouters;
    bool private entered;

    modifier nonReentrant() {
        if (entered) revert Reentrancy();
        entered = true;
        _;
        entered = false;
    }

    error NotOwner();
    error Reentrancy();
    error InvalidRecipient();
    error InvalidRouter();
    error RouterNotAllowed();
    error ValueMismatch();
    error TransferFailed();
    error SwapFailed(bytes returndata);
    error UnexpectedInputAmount(uint256 received, uint256 expected);

    event OwnerChanged(address indexed previousOwner, address indexed newOwner);
    event RouterSet(address indexed router, bool allowed);
    event TradeExecuted(
        address indexed trader,
        address indexed router,
        address indexed tokenIn,
        address tokenOut,
        uint256 grossAmountIn,
        uint256 feeAmount,
        uint256 netAmountIn,
        uint256 minAmountOut
    );
    event Swept(address indexed token, address indexed to, uint256 amount);

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    constructor(address feeRecipient_) {
        if (feeRecipient_ == address(0)) revert InvalidRecipient();
        owner = msg.sender;
        feeRecipient = feeRecipient_;
        emit OwnerChanged(address(0), msg.sender);
    }

    receive() external payable {}

    function setOwner(address newOwner) external onlyOwner {
        if (newOwner == address(0)) revert InvalidRecipient();
        emit OwnerChanged(owner, newOwner);
        owner = newOwner;
    }

    function setRouter(address router, bool allowed) external onlyOwner {
        if (router == address(0) || router == address(this)) revert InvalidRouter();
        allowedRouters[router] = allowed;
        emit RouterSet(router, allowed);
    }

    /// @dev `minAmountOut` is recorded for auditability; the allowed router's
    /// calldata must contain the same slippage floor. Generic calldata cannot
    /// be introspected safely without coupling this wrapper to every router ABI.
    function executeSwapWithFee(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 minAmountOut,
        address router,
        bytes calldata swapCalldata
    ) external payable nonReentrant returns (bytes memory result) {
        if (!allowedRouters[router] || router == address(this)) {
            revert RouterNotAllowed();
        }
        if (amountIn == 0) revert ValueMismatch();

        uint256 fee = (amountIn * PLATFORM_FEE_BPS) / BPS;
        uint256 netAmount = amountIn - fee;

        if (tokenIn == address(0)) {
            if (msg.value != amountIn) revert ValueMismatch();
            _sendNative(feeRecipient, fee);
            (bool ok, bytes memory returned) = router.call{value: netAmount}(swapCalldata);
            if (!ok) revert SwapFailed(returned);
            result = returned;
        } else {
            if (msg.value != 0) revert ValueMismatch();
            uint256 beforeBalance = _balance(tokenIn, address(this));
            _callToken(
                tokenIn,
                abi.encodeWithSignature(
                    "transferFrom(address,address,uint256)", msg.sender, address(this), amountIn
                )
            );
            uint256 received = _balance(tokenIn, address(this)) - beforeBalance;
            // Fee-on-transfer input tokens are ambiguous at the fee boundary;
            // reject them instead of silently charging a different gross amount.
            if (received != amountIn) revert UnexpectedInputAmount(received, amountIn);
            _callToken(tokenIn, abi.encodeWithSignature("transfer(address,uint256)", feeRecipient, fee));
            _callToken(tokenIn, abi.encodeWithSignature("approve(address,uint256)", router, netAmount));
            (bool ok, bytes memory returned) = router.call(swapCalldata);
            if (!ok) revert SwapFailed(returned);
            // Do not leave an allowance on the wrapper after the call.
            _callToken(tokenIn, abi.encodeWithSignature("approve(address,uint256)", router, 0));
            result = returned;
        }

        emit TradeExecuted(msg.sender, router, tokenIn, tokenOut, amountIn, fee, netAmount, minAmountOut);
    }

    function sweep(address token, address to, uint256 amount) external onlyOwner nonReentrant {
        if (to == address(0)) revert InvalidRecipient();
        if (token == address(0)) {
            _sendNative(to, amount);
        } else {
            _callToken(token, abi.encodeWithSignature("transfer(address,uint256)", to, amount));
        }
        emit Swept(token, to, amount);
    }

    function _balance(address token, address account) private view returns (uint256) {
        (bool ok, bytes memory data) =
            token.staticcall(abi.encodeWithSignature("balanceOf(address)", account));
        if (!ok || data.length < 32) revert TransferFailed();
        return abi.decode(data, (uint256));
    }

    function _callToken(address token, bytes memory data) private {
        (bool ok, bytes memory returned) = token.call(data);
        if (!ok || (returned.length != 0 && !abi.decode(returned, (bool)))) revert TransferFailed();
    }

    function _sendNative(address to, uint256 amount) private {
        (bool ok,) = to.call{value: amount}("");
        if (!ok) revert TransferFailed();
    }
}
