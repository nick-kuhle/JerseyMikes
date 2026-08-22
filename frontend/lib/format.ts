/** Formatting helpers shared by every panel. */

const WEI_PER_ETH = 1_000_000_000_000_000_000;

export function weiToEth(wei: string | number | bigint, decimals = 6): string {
  let n: number;
  if (typeof wei === "bigint") n = Number(wei);
  else if (typeof wei === "string") n = Number(wei || "0");
  else n = wei;
  if (!Number.isFinite(n)) return "0";
  return (n / WEI_PER_ETH).toFixed(decimals);
}

export function signedEth(wei: number, decimals = 6): string {
  const v = wei / WEI_PER_ETH;
  return `${v >= 0 ? "+" : ""}${v.toFixed(decimals)}`;
}

export function gwei(wei: string | number): string {
  const n = typeof wei === "string" ? Number(wei || "0") : wei;
  return (n / 1e9).toFixed(2);
}

export function shortHash(h: string | null | undefined, size = 6): string {
  if (!h) return "—";
  if (h.length <= size * 2 + 2) return h;
  return `${h.slice(0, size + 2)}…${h.slice(-4)}`;
}

export function ago(ms: number): string {
  const d = Date.now() - ms;
  if (d < 1_000) return "now";
  if (d < 60_000) return `${Math.floor(d / 1000)}s`;
  if (d < 3_600_000) return `${Math.floor(d / 60_000)}m`;
  return `${Math.floor(d / 3_600_000)}h`;
}

export function clock(ms: number): string {
  const d = new Date(ms);
  return d.toLocaleTimeString("en-US", {hour12: false});
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
