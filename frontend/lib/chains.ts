/**
 * Multi-chain registry (server-side).
 *
 * One bot process per chain (see docs/DEPLOYMENT.md "Multi-chain layout");
 * the console multiplexes across them. The registry comes from the
 * server-only `CHAINS` env var (chains separated by commas, fields by
 * pipes — URLs contain `:` and `//`, so pipes are the field separator):
 *
 *   CHAINS="ethereum|http://127.0.0.1:8080,base|http://127.0.0.1:8081"
 *
 * an optional third field overrides the `/api/eth` RPC for that chain:
 *
 *   CHAINS="ethereum|http://127.0.0.1:8080|https://eth-rpc.example,base|http://127.0.0.1:8081|https://base-rpc.example"
 *
 * With `CHAINS` unset the console is single-chain: `BOT_API_URL` is the
 * (implicit) "ethereum" chain — zero-config back-compat with existing
 * deployments.
 */

export interface ChainDef {
  /** Stable key used in `?chain=` and localStorage. */
  slug: string;
  /** Human label for the switcher pills. */
  label: string;
  /** The bot instance this chain's panels talk to. */
  botUrl: string;
  /** Optional per-chain RPC for the /api/eth read proxy. */
  rpcUrl?: string;
}

const LABELS: Record<string, string> = {
  ethereum: "Ethereum",
  mainnet: "Ethereum",
  base: "Base",
  arbitrum: "Arbitrum",
  optimism: "Optimism",
  bsc: "BNB",
};

/** Parse the CHAINS value into a chain list. Malformed entries are dropped
 * (a broken entry must not take the whole console down); an all-malformed
 * value falls back to the single BOT_API_URL chain. */
export function chains(): ChainDef[] {
  const raw = process.env.CHAINS ?? "";
  const out: ChainDef[] = [];
  for (const entry of raw.split(",")) {
    const parts = entry.split("|").map((p) => p.trim());
    if (parts.length < 2) continue;
    const slug = parts[0].toLowerCase();
    const url = parts[1];
    if (!slug || !/^https?:\/\//.test(url)) continue;
    out.push({
      slug,
      label: LABELS[slug] ?? slug.charAt(0).toUpperCase() + slug.slice(1),
      botUrl: url,
      rpcUrl: parts[2] && /^https?:\/\//.test(parts[2]) ? parts[2] : undefined,
    });
  }
  if (out.length === 0) {
    const single = process.env.BOT_API_URL ?? "http://127.0.0.1:8080";
    out.push({ slug: "ethereum", label: "Ethereum", botUrl: single });
  }
  return out;
}

/** The chain a `?chain=` (or missing) parameter selects. Unknown slugs fall
 * back to the first chain — a stale localStorage value must never 404 the
 * whole console. */
export function chainBySlug(slug?: string | null): ChainDef {
  const all = chains();
  if (!slug) return all[0];
  return all.find((c) => c.slug === slug) ?? all[0];
}

/** Per-chain token: `BOT_API_TOKEN_<SLUG>` (upper) wins over the shared
 * `BOT_API_TOKEN`. */
export function tokenForChain(slug: string): string {
  const perChain = process.env[`BOT_API_TOKEN_${slug.toUpperCase()}`];
  if (perChain) return perChain;
  return process.env.BOT_API_TOKEN ?? "";
}
