import {NextRequest} from "next/server";
import {botFetch, BOT_API_URL} from "@/lib/bot";
import {
  demoCompetition,
  demoEvent,
  demoLatency,
  demoOpportunities,
  demoPnl,
  demoRelayBids,
  demoRelayBlocks,
  demoRelayTxs,
  demoReorgs,
  demoSeries,
  demoSimulations,
  demoStatus,
} from "@/lib/demo";

export const dynamic = "force-dynamic";

/** Demo runtime mode. Flipped by POST /api/bot/mode in demo mode only. */
let demoLive = false;

/** Fallbacks keyed by the bot's own route names. */
function demoFor(path: string, search: URLSearchParams): unknown {
  const limit = Number(search.get("limit") ?? 100);
  switch (path) {
    case "status":
      return {...demoStatus(), mode: demoLive ? "live" : "simulation", liveArmed: true};
    case "mode":
      return {mode: demoLive ? "live" : "simulation", liveArmed: true, demo: true};
    case "pnl":
      return demoPnl();
    case "pnl/series":
      return demoSeries(limit);
    case "opportunities":
      return demoOpportunities(limit);
    case "simulations": {
      const strategy = search.get("strategy");
      const rows = demoSimulations(limit);
      return strategy ? rows.filter((r) => r.strategy === strategy) : rows;
    }
    case "relay-bids":
      return demoRelayBids(limit);
    case "relay-blocks":
      return demoRelayBlocks(limit);
    case "relay-txs": {
      const blockNumber = search.get("blockNumber");
      return demoRelayTxs(blockNumber ? Number(blockNumber) : undefined, limit);
    }
    case "config":
      return {
        chainId: 1,
        executor: demoStatus().executor,
        liveExecution: demoLive,
        liveArmed: true,
        demo: true,
      };
    case "funnel":
      // The funnel is also embedded in `/api/bot/status` under `stats.funnel`,
      // but exposing it as a standalone endpoint makes polling cheaper for
      // dashboards that want to refresh the funnel separately from the rest
      // of the status.
      return {funnel: demoStatus().stats.funnel};
    case "latency":
      return demoLatency();
    case "competition":
      return demoCompetition();
    case "reorgs":
      return demoReorgs();
    default:
      return {error: `unknown endpoint ${path}`};
  }
}

function jsonResponse(data: unknown, demo: boolean) {
  const body = Array.isArray(data) ? data : {...(data as object), demo};
  return new Response(JSON.stringify(body), {
    headers: {"content-type": "application/json", "x-data-source": demo ? "demo" : "bot"},
  });
}

export async function GET(req: NextRequest, {params}: {params: Promise<{path: string[]}>}) {
  const {path} = await params;
  const route = path.join("/");
  const search = req.nextUrl.searchParams;
  const qs = search.toString();

  if (route === "stream") {
    return streamResponse(qs);
  }

  const upstream = await botFetch(`/api/${route}${qs ? `?${qs}` : ""}`);
  if (upstream.ok) return jsonResponse(upstream.data, false);
  return jsonResponse(demoFor(route, search), true);
}

/**
 * Mutating bot endpoints. Only `/api/mode` (the simulation ⇄ live switch) is
 * proxied; when the bot is unreachable the demo mode state flips instead so
 * the dashboard's switch flow is exercisable without a running bot — always
 * flagged with `demo: true` so nobody mistakes it for a real mode change.
 */
export async function POST(req: NextRequest, {params}: {params: Promise<{path: string[]}>}) {
  const {path} = await params;
  const route = path.join("/");
  if (route !== "mode") {
    return jsonResponse({error: `unknown endpoint ${route}`}, true);
  }

  let body: {live?: boolean} = {};
  try {
    body = (await req.json()) as {live?: boolean};
  } catch {
    return jsonResponse({error: "invalid JSON body"}, true);
  }
  if (typeof body.live !== "boolean") {
    return jsonResponse({error: "body must be {live: boolean}"}, true);
  }

  try {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 2500);
    const upstream = await fetch(`${BOT_API_URL}/api/mode`, {
      method: "POST",
      signal: controller.signal,
      headers: {"content-type": "application/json"},
      body: JSON.stringify({live: body.live}),
    });
    clearTimeout(timer);
    if (upstream.ok || upstream.status === 409) {
      const data = (await upstream.json()) as Record<string, unknown>;
      return jsonResponse({...data, ok: upstream.ok}, false);
    }
  } catch {
    /* fall through to demo */
  }

  // Demo fallback: the bot is unreachable, so this cannot touch anything real.
  demoLive = body.live;
  return jsonResponse(
    {ok: true, mode: demoLive ? "live" : "simulation", liveArmed: true, demo: true},
    true
  );
}

/**
 * SSE: proxy the bot's live feed, or synthesise one in demo mode so the UI can
 * be developed and reviewed without a running bot.
 */
async function streamResponse(qs: string): Promise<Response> {
  const headers = {
    "content-type": "text/event-stream",
    "cache-control": "no-cache, no-transform",
    connection: "keep-alive",
    "x-accel-buffering": "no",
  };

  try {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 2000);
    const upstream = await fetch(`${BOT_API_URL}/api/stream${qs ? `?${qs}` : ""}`, {
      signal: controller.signal,
      headers: {accept: "text/event-stream"},
    });
    clearTimeout(timer);
    if (upstream.ok && upstream.body) {
      return new Response(upstream.body, {headers: {...headers, "x-data-source": "bot"}});
    }
  } catch {
    // fall through to the demo stream
  }

  let i = 0;
  let handle: ReturnType<typeof setInterval> | undefined;
  const stream = new ReadableStream({
    start(controller) {
      const enc = new TextEncoder();
      const tick = () => {
        const burst = 1 + Math.floor(Math.random() * 3);
        try {
          for (let k = 0; k < burst; k++) {
            controller.enqueue(enc.encode(`data: ${JSON.stringify(demoEvent(i++))}\n\n`));
          }
        } catch {
          // The client disconnected between ticks.
          if (handle) clearInterval(handle);
        }
      };
      tick();
      handle = setInterval(tick, 700);
      setTimeout(() => handle && clearInterval(handle), 1000 * 60 * 30);
    },
    cancel() {
      if (handle) clearInterval(handle);
    },
  });

  return new Response(stream, {headers: {...headers, "x-data-source": "demo"}});
}
