/**
 * Client-side active-chain state (the browser half of the multi-chain
 * console).
 *
 * The active chain lives in localStorage (persists across reloads) and every
 * `/api/bot/*` / `/api/eth` request carries `?chain=<slug>`. The server
 * falls back to the first configured chain when the slug is missing or
 * stale, so a first visit (no localStorage yet) and an unknown slug both
 * work — but a saved *valid* slug is never overridden by the fallback.
 *
 * Cross-chain data bleed is the one unacceptable bug class here (work order
 * R6): the Console remounts the whole panel tree on chain change (React
 * `key`), so no panel can ever render another chain's state.
 */

const CHAIN_KEY = "jm-active-chain";
const CHAIN_EVENT = "jm:chain-change";

/** The slug the user last selected, or null (server falls back to default). */
export function readActiveChain(): string | null {
  if (typeof window === "undefined") return null;
  try {
    return window.localStorage.getItem(CHAIN_KEY);
  } catch {
    return null; // sandboxed preview: no localStorage, default chain
  }
}

/** Persist the selection and notify subscribers (all panels re-key). */
export function setActiveChain(slug: string | null): void {
  if (typeof window === "undefined") return;
  try {
    if (slug) window.localStorage.setItem(CHAIN_KEY, slug);
    else window.localStorage.removeItem(CHAIN_KEY);
  } catch {
    // storage unavailable — the in-memory event still keeps this session
    // consistent
  }
  window.dispatchEvent(new CustomEvent(CHAIN_EVENT));
}

/** Subscribe to chain changes; returns the unsubscribe function. */
export function onChainChange(cb: (slug: string | null) => void): () => void {
  if (typeof window === "undefined") return () => {};
  const handler = () => cb(readActiveChain());
  window.addEventListener(CHAIN_EVENT, handler);
  return () => window.removeEventListener(CHAIN_EVENT, handler);
}

/** Append `?chain=<slug>` (or `&chain=`) to an API path. */
export function withChain(path: string, slug: string | null): string {
  if (!slug) return path;
  const sep = path.includes("?") ? "&" : "?";
  return `${path}${sep}chain=${encodeURIComponent(slug)}`;
}
