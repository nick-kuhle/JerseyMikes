/**
 * In-app swap, chart and platform-fee utilities.
 *
 * Direct wallet execution is deliberately limited to the chain's configured
 * Uniswap V2-compatible router. Arbitrary calldata is not accepted from the
 * UI. If a platform treasury is configured, execution requires the deployed
 * JerseyMikes fee router so the 1% fee and swap are atomic.
 */

import {encodeFunctionData, type Address} from "viem";

export type TradeSide = "buy" | "sell";
export type TradeRoutePreference = "fastest" | "mev_safe" | "best_price";

export const PLATFORM_FEE_BPS = 100;
export const BPS_DENOMINATOR = 10_000n;

export function calculatePlatformFee(amountIn: bigint, feeBps = PLATFORM_FEE_BPS): bigint {
  if (amountIn <= 0n || feeBps <= 0) return 0n;
  return (amountIn * BigInt(feeBps)) / BPS_DENOMINATOR;
}

export function amountAfterPlatformFee(amountIn: bigint, feeBps = PLATFORM_FEE_BPS): bigint {
  return amountIn - calculatePlatformFee(amountIn, feeBps);
}

export interface PlatformFeeConfig {
  feeRecipient: Address | null;
  feeRouter: Address | null;
}

export function platformFeeConfig(): PlatformFeeConfig {
  const recipient = process.env.NEXT_PUBLIC_PLATFORM_FEE_RECIPIENT;
  const router = process.env.NEXT_PUBLIC_PLATFORM_FEE_ROUTER_ADDRESS;
  return {
    feeRecipient: recipient && /^0x[0-9a-fA-F]{40}$/.test(recipient) ? recipient as Address : null,
    feeRouter: router && /^0x[0-9a-fA-F]{40}$/.test(router) ? router as Address : null,
  };
}

export interface AggregatorLinks {
  oneInch: string;
  uniswap: string;
  kyberswap: string;
  dexscreener: string;
}

/** Generate safe reference links; these are fallback research links, not execution. */
export function getAggregatorLinks(tokenAddress: string, chainId = 1, chainSlug = "ethereum"): AggregatorLinks {
  const addr = tokenAddress.toLowerCase();
  const cId = chainId || 1;
  const chain = cId === 8453 ? "base" : chainSlug === "base" ? "base" : "ethereum";
  return {
    oneInch: `https://app.1inch.io/#/${cId}/simple/swap/${addr}/ETH`,
    uniswap: `https://app.uniswap.org/swap?chain=${chain}&inputCurrency=${addr}&outputCurrency=ETH`,
    kyberswap: `https://kyberswap.com/swap/${chain}?inputCurrency=${addr}&outputCurrency=ETH`,
    dexscreener: `https://dexscreener.com/${chain}/${addr}`,
  };
}

export function dexScreenerEmbedUrl(chainSlug: string, pairOrToken: string): string {
  return `https://dexscreener.com/${chainSlug}/${pairOrToken}?embed=1&theme=dark&trades=0&info=0`;
}

export function dexToolsEmbedUrl(chainSlug: string, pairAddress: string): string {
  return `https://www.dextools.io/widget-chart/en/${chainSlug}/pe-light/${pairAddress}?theme=dark`;
}

export const V2_ROUTER_BY_CHAIN: Record<string, Address> = {
  ethereum: "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D",
  base: "0x4752ba5DBc23f44D87826276BF6Fd6b1C372aD24",
};

export const V3_ROUTER_BY_CHAIN: Record<string, Address> = {
  ethereum: "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45",
  base: "0x2626664c2603336E57B271c5C0b26F421741e481",
};

export const AERODROME_ROUTER_BY_CHAIN: Record<string, Address> = {
  base: "0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43",
};

export const V2_FACTORY_BY_CHAIN: Record<string, Address> = {
  ethereum: "0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f",
  base: "0x8909Dc15e40173Ff4699343b6eB8132c65e18eC6",
};

export const V2_FACTORY_ABI = [
  {type: "function", name: "getPair", stateMutability: "view", inputs: [{name: "tokenA", type: "address"}, {name: "tokenB", type: "address"}], outputs: [{name: "pair", type: "address"}]},
] as const;

export const V2_ROUTER_ABI = [
  {type: "function", name: "swapExactETHForTokensSupportingFeeOnTransferTokens", stateMutability: "payable", inputs: [{name: "amountOutMin", type: "uint256"}, {name: "path", type: "address[]"}, {name: "to", type: "address"}, {name: "deadline", type: "uint256"}], outputs: []},
  {type: "function", name: "swapExactTokensForETHSupportingFeeOnTransferTokens", stateMutability: "nonpayable", inputs: [{name: "amountIn", type: "uint256"}, {name: "amountOutMin", type: "uint256"}, {name: "path", type: "address[]"}, {name: "to", type: "address"}, {name: "deadline", type: "uint256"}], outputs: []},
] as const;

/** ABI for the optional atomic 1% fee wrapper. */
export const V3_ROUTER_ABI = [
  {type: "function", name: "exactInputSingle", stateMutability: "payable", inputs: [{name: "params", type: "tuple", components: [
    {name: "tokenIn", type: "address"}, {name: "tokenOut", type: "address"}, {name: "fee", type: "uint24"},
    {name: "recipient", type: "address"}, {name: "amountIn", type: "uint256"}, {name: "amountOutMinimum", type: "uint256"},
    {name: "sqrtPriceLimitX96", type: "uint160"},
  ]}], outputs: [{name: "amountOut", type: "uint256"}]},
] as const;

export interface V3ExactInputSingleRequest {
  tokenIn: Address;
  tokenOut: Address;
  fee: number;
  recipient: Address;
  amountIn: bigint;
  amountOutMinimum: bigint;
  sqrtPriceLimitX96?: bigint;
}

export function buildV3ExactInputSingle(request: V3ExactInputSingleRequest): `0x${string}` {
  return encodeFunctionData({
    abi: V3_ROUTER_ABI,
    functionName: "exactInputSingle",
    args: [{
      tokenIn: request.tokenIn,
      tokenOut: request.tokenOut,
      fee: request.fee,
      recipient: request.recipient,
      amountIn: request.amountIn,
      amountOutMinimum: request.amountOutMinimum,
      sqrtPriceLimitX96: request.sqrtPriceLimitX96 ?? 0n,
    }],
  });
}

/** Aerodrome's Solidly route is explicit about stable/factory semantics. */
export const AERODROME_ROUTER_ABI = [
  {type: "function", name: "swapExactETHForTokens", stateMutability: "payable", inputs: [{name: "amountOutMin", type: "uint256"}, {name: "routes", type: "tuple[]", components: [{name: "from", type: "address"}, {name: "to", type: "address"}, {name: "stable", type: "bool"}, {name: "factory", type: "address"}]}, {name: "to", type: "address"}, {name: "deadline", type: "uint256"}], outputs: [{name: "amounts", type: "uint256[]"}]},
] as const;

export interface AerodromeRoute {
  from: Address;
  to: Address;
  stable: boolean;
  factory: Address;
}

export function buildAerodromeNativeBuyCalldata(amountOutMinimum: bigint, routes: AerodromeRoute[], to: Address, deadline: bigint): `0x${string}` {
  return encodeFunctionData({abi: AERODROME_ROUTER_ABI, functionName: "swapExactETHForTokens", args: [amountOutMinimum, routes, to, deadline]});
}

/** Build a read-only 1inch quote request; an API key is never shipped by default. */
export function oneInchQuoteUrl(chainId: number, tokenIn: Address, tokenOut: Address, amountIn: bigint): string {
  const query = new URLSearchParams({src: tokenIn, dst: tokenOut, amount: amountIn.toString()});
  return `https://api.1inch.dev/swap/v6.0/${chainId}/quote?${query.toString()}`;
}

export const PLATFORM_FEE_ROUTER_ABI = [
  {type: "function", name: "executeSwapWithFee", stateMutability: "payable", inputs: [
    {name: "tokenIn", type: "address"}, {name: "tokenOut", type: "address"}, {name: "amountIn", type: "uint256"},
    {name: "minAmountOut", type: "uint256"}, {name: "router", type: "address"}, {name: "swapCalldata", type: "bytes"},
  ], outputs: [{name: "result", type: "bytes"}]},
] as const;

export const ERC20_ABI = [
  {inputs: [{internalType: "address", name: "account", type: "address"}], name: "balanceOf", outputs: [{internalType: "uint256", name: "", type: "uint256"}], stateMutability: "view", type: "function"},
  {inputs: [], name: "decimals", outputs: [{internalType: "uint8", name: "", type: "uint8"}], stateMutability: "view", type: "function"},
  {inputs: [], name: "symbol", outputs: [{internalType: "string", name: "", type: "string"}], stateMutability: "view", type: "function"},
  {inputs: [], name: "name", outputs: [{internalType: "string", name: "", type: "string"}], stateMutability: "view", type: "function"},
  {inputs: [{internalType: "address", name: "spender", type: "address"}, {internalType: "uint256", name: "amount", type: "uint256"}], name: "approve", outputs: [{internalType: "bool", name: "", type: "bool"}], stateMutability: "nonpayable", type: "function"},
  {inputs: [{internalType: "address", name: "owner", type: "address"}, {internalType: "address", name: "spender", type: "address"}], name: "allowance", outputs: [{internalType: "uint256", name: "", type: "uint256"}], stateMutability: "view", type: "function"},
] as const;
