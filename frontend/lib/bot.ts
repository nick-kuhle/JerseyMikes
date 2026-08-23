import {chainBySlug, tokenForChain} from "./chains";

/**
 * Server-side bridge to the Rust bot(s).
 *
 * The browser never talks to a bot directly (it may be on a private network,
 * and in a sandboxed preview `localhost` means something different in the
 * browser than on the server). Everything goes through `/api/bot/*`, which
 * proxies to the selected chain's bot and falls back to the demo generator
 * when that bot is not reachable.
 *
 * Multi-chain: every helper takes an optional `chainSlug` (the `?chain=`
 * value). Omitted = the first configured chain, which for a single-chain
 * deployment is the only chain — back-compatible with the old
 * `BOT_API_URL`-only setup.
 */

/** Default (first) chain's bot URL — kept for back-compat and the
 * single-chain fallback in `chains()`. */
export const BOT_API_URL =
  process.env.BOT_API_URL ?? "http://127.0.0.1:8080";

/**
 * Bearer token for the bot's *mutating* endpoints, when it runs with
 * `API_AUTH_TOKEN` set (required whenever the bot binds to a non-loopback
 * address).
 *
 * Server-side only — deliberately **not** `NEXT_PUBLIC_`, so the secret stays
 * on the Next.js server and never reaches the browser bundle.
 */
export function botAuthHeaders(chainSlug?: string | null): Record<string, string> {
  const chain = chainBySlug(chainSlug);
  const token = tokenForChain(chain.slug);
  return token ? {authorization: `Bearer ${token}`} : {};
}

/** Absolute upstream URL for a bot route on a chain. */
export function botUpstreamUrl(path: string, chainSlug?: string | null): string {
  const chain = chainBySlug(chainSlug);
  return `${chain.botUrl}${path}`;
}

export async function botFetch(
  path: string,
  timeoutMs = 2500,
  chainSlug?: string | null,
): Promise<{ok: true; data: unknown} | {ok: false}> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const res = await fetch(botUpstreamUrl(path, chainSlug), {
      signal: controller.signal,
      cache: "no-store",
      headers: {accept: "application/json", ...botAuthHeaders(chainSlug)},
    });
    if (!res.ok) return {ok: false};
    return {ok: true, data: await res.json()};
  } catch {
    return {ok: false};
  } finally {
    clearTimeout(timer);
  }
}

/**
 * POST JSON to the bot through the same server-side bridge as `botFetch`.
 * Returns the parsed body (with its HTTP status folded in), or a network
 * error marker — callers decide how to surface failures.
 */
export async function botPost(
  path: string,
  body: unknown,
  timeoutMs = 3000,
  chainSlug?: string | null,
): Promise<{ok: true; data: Record<string, unknown>} | {ok: false; error?: string}> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const res = await fetch(botUpstreamUrl(path, chainSlug), {
      method: "POST",
      signal: controller.signal,
      cache: "no-store",
      headers: {
        "content-type": "application/json",
        accept: "application/json",
        ...botAuthHeaders(chainSlug),
      },
      body: JSON.stringify(body),
    });
    const data = (await res.json().catch(() => ({}))) as Record<string, unknown>;
    if (!res.ok) {
      const error = typeof data.error === "string" ? data.error : `HTTP ${res.status}`;
      return {ok: false, error};
    }
    return {ok: true, data};
  } catch (e) {
    return {ok: false, error: (e as Error).name === "AbortError" ? "timeout" : "network error"};
  } finally {
    clearTimeout(timer);
  }
}
