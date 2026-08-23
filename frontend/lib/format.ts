/** Formatting helpers shared by every panel. */

const WEI_PER_ETH = 1_000_000_000_000_000_000n;

function decimalUnits(value: string | number | bigint, scale: bigint, decimals: number): string {
  let amount: bigint;
  try {
    amount = typeof value === "bigint" ? value : BigInt(String(value || 0));
  } catch {
    return "0";
  }
  const negative = amount < 0n;
  const absolute = negative ? -amount : amount;
  const whole = absolute / scale;
  const fraction = absolute % scale;
  const scaleDigits = scale.toString().length - 1;
  const shown = Math.max(0, Math.min(decimals, scaleDigits));
  if (shown === 0) return `${negative ? "-" : ""}${whole}`;
  const padded = fraction.toString().padStart(scaleDigits, "0").slice(0, shown);
  return `${negative ? "-" : ""}${whole}.${padded}`;
}

export function weiToEth(wei: string | number | bigint, decimals = 6): string {
  return decimalUnits(wei, WEI_PER_ETH, decimals);
}

export function signedEth(wei: string | number | bigint, decimals = 6): string {
  const rendered = weiToEth(wei, decimals);
  return rendered.startsWith("-") ? rendered : `+${rendered}`;
}

export function gwei(wei: string | number | bigint): string {
  return decimalUnits(wei, 1_000_000_000n, 2);
}

export function shortHash(h: string | null | undefined, size = 6): string {
  if (!h) return "—";
  if (h.length <= size * 2 + 2) return h;
  return `${h.slice(0, size + 2)}…${h.slice(-4)}`;
}

export function ago(ms: number): string {
  const s = Math.max(0, Math.floor((Date.now() - ms) / 1000));
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  return `${h}h ago`;
}

export function clock(ms: number): string {
  return new Date(ms).toLocaleTimeString([], {hour12: false});
}

export const STRATEGY_LABEL: Record<string, string> = {
  sandwich: "Sandwich",
  sandwich_v3: "Sandwich V3",
  jit: "JIT liquidity",
  atomic_arb: "Atomic arb",
  liquidation: "Liquidation (Aave)",
  liquidation_compound: "Liquidation (Compound V3)",
  liquidation_morpho: "Liquidation (Morpho)",
  liquidation_maker: "Liquidation (Maker)",
  oracle_frontrun: "Oracle front-run",
  sniper: "Token sniper",
};

export const STRATEGY_COLOR: Record<string, string> = {
  sandwich: "#f97316",
  sandwich_v3: "#fb7185",
  jit: "#a855f7",
  atomic_arb: "#22d3ee",
  liquidation: "#ef4444",
  liquidation_compound: "#f87171",
  liquidation_morpho: "#fb923c",
  liquidation_maker: "#eab308",
  oracle_frontrun: "#38bdf8",
  sniper: "#84cc16",
};
