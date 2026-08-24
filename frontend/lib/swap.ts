/**
 * DEX Aggregator & Swapping utilities for the New Token Sniper.
 *
 * Provides deep links to top DEX aggregators (1inch, Uniswap, KyberSwap, DexScreener, Odos)
 * and ABI definitions for ERC20 approvals and on-chain swap interactions.
 */

export interface AggregatorLinks {
  oneInch: string;
  uniswap: string;
  kyberswap: string;
  dexscreener: string;
}

/** Generate pre-filled aggregator swap links for a given token and chain */
export function getAggregatorLinks(
  tokenAddress: string,
  chainId = 1,
  chainSlug = "ethereum",
): AggregatorLinks {
  const addr = tokenAddress.toLowerCase();
  const cId = chainId || 1;

  // 1inch deep link format
  const oneInch = `https://app.1inch.io/#/${cId}/simple/swap/${addr}/ETH`;

  // Uniswap deep link format
  const uniswapChain = cId === 8453 ? "base" : "ethereum";
  const uniswap = `https://app.uniswap.org/swap?chain=${uniswapChain}&inputCurrency=${addr}&outputCurrency=ETH`;

  // KyberSwap deep link format
  const kyberChain = cId === 8453 ? "base" : "ethereum";
  const kyberswap = `https://kyberswap.com/swap/${kyberChain}?inputCurrency=${addr}&outputCurrency=ETH`;

  // DexScreener deep link format
  const dexscreenerChain = cId === 8453 ? "base" : "ethereum";
  const dexscreener = `https://dexscreener.com/${dexscreenerChain}/${addr}`;

  return {
    oneInch,
    uniswap,
    kyberswap,
    dexscreener,
  };
}

/** Standard ERC20 ABI for balance checks and approval */
export const ERC20_ABI = [
  {
    inputs: [{internalType: "address", name: "account", type: "address"}],
    name: "balanceOf",
    outputs: [{internalType: "uint256", name: "", type: "uint256"}],
    stateMutability: "view",
    type: "function",
  },
  {
    inputs: [],
    name: "decimals",
    outputs: [{internalType: "uint8", name: "", type: "uint8"}],
    stateMutability: "view",
    type: "function",
  },
  {
    inputs: [],
    name: "symbol",
    outputs: [{internalType: "string", name: "", type: "string"}],
    stateMutability: "view",
    type: "function",
  },
  {
    inputs: [],
    name: "name",
    outputs: [{internalType: "string", name: "", type: "string"}],
    stateMutability: "view",
    type: "function",
  },
  {
    inputs: [
      {internalType: "address", name: "spender", type: "address"},
      {internalType: "uint256", name: "amount", type: "uint256"},
    ],
    name: "approve",
    outputs: [{internalType: "bool", name: "", type: "bool"}],
    stateMutability: "nonpayable",
    type: "function",
  },
  {
    inputs: [
      {internalType: "address", name: "owner", type: "address"},
      {internalType: "address", name: "spender", type: "address"},
    ],
    name: "allowance",
    outputs: [{internalType: "uint256", name: "", type: "uint256"}],
    stateMutability: "view",
    type: "function",
  },
] as const;
