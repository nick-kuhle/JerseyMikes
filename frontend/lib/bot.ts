/**
 * Server-side bridge to the Rust bot.
 *
 * The browser never talks to the bot directly (it may be on a private network,
 * and in a sandboxed preview `localhost` means something different in the
 * browser than on the server). Everything goes through `/api/bot/*`, which
 * proxies to `BOT_API_URL` and falls back to the demo generator when the bot is
 * not reachable.
 */
export const BOT_API_URL = process.env.BOT_API_URL ?? "http://127.0.0.1:8080";

export async function botFetch(path: string, timeoutMs = 2500): Promise<{ok: true; data: unknown} | {ok: false}> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const res = await fetch(`${BOT_API_URL}${path}`, {
      signal: controller.signal,
      cache: "no-store",
      headers: {accept: "application/json"},
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
): Promise<{ok: true; data: Record<string, unknown>} | {ok: false; error?: string}> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const res = await fetch(`${BOT_API_URL}${path}`, {
      method: "POST",
      signal: controller.signal,
      cache: "no-store",
      headers: {"content-type": "application/json", accept: "application/json"},
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
