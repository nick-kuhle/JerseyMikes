/**
 * Block-explorer URL helpers.
 *
 * Every transaction, block and address the console renders is a real on-chain
 * object; each one links out to the chain's explorer (Etherscan on mainnet,
 * the chain's own explorer on L2s) so a row can always be verified against
 * the chain itself. Chains are keyed by the same `chainId` the bot reports in
 * `/api/status`; an unknown chain renders plain text (no link) rather than a
 * wrong link.
 */

interface Explorer {
  name: string;
  base: string;
}

const EXPLORERS: Record<number, Explorer> = {
  1: {name: "Etherscan", base: "https://etherscan.io"},
  8453: {name: "Basescan", base: "https://basescan.org"},
  42161: {name: "Arbiscan", base: "https://arbiscan.io"},
  10: {name: "Optimistic Etherscan", base: "https://optimistic.etherscan.io"},
  137: {name: "PolygonScan", base: "https://polygonscan.com"},
  56: {name: "BscScan", base: "https://bscscan.com"},
  43114: {name: "SnowTrace", base: "https://snowtrace.io"},
  // Testnets, so the console is honest when someone points it at one.
  11155111: {name: "Etherscan (Sepolia)", base: "https://sepolia.etherscan.io"},
  84532: {name: "Basescan (Sepolia)", base: "https://sepolia.basescan.org"},
  421614: {name: "Arbiscan (Sepolia)", base: "https://sepolia.arbiscan.io"},
};

export function explorerName(chainId: number | undefined): string {
  return (chainId && EXPLORERS[chainId]?.name) || "explorer";
}

function explorerBase(chainId: number | undefined): string | null {
  if (!chainId) return null;
  return EXPLORERS[chainId]?.base ?? null;
}

function isHashLike(h: string | null | undefined): h is string {
  return Boolean(h) && /^0x[0-9a-fA-F]{64}$/.test(h as string);
}

function isAddressLike(a: string | null | undefined): a is string {
  return Boolean(a) && /^0x[0-9a-fA-F]{40}$/.test(a as string);
}

export function txUrl(chainId: number | undefined, hash: string | null | undefined): string | null {
  if (!isHashLike(hash)) return null;
  const base = explorerBase(chainId);
  return base ? `${base}/tx/${hash}` : null;
}

export function addressUrl(
  chainId: number | undefined,
  addr: string | null | undefined
): string | null {
  if (!isAddressLike(addr)) return null;
  const base = explorerBase(chainId);
  return base ? `${base}/address/${addr}` : null;
}

export function blockUrl(chainId: number | undefined, block: number | string): string | null {
  const n = typeof block === "string" ? Number(block) : block;
  if (!Number.isFinite(n) || n < 0) return null;
  const base = explorerBase(chainId);
  return base ? `${base}/block/${n}` : null;
}
