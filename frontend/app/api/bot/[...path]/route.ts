import {NextRequest} from "next/server";
import {botAuthHeaders, botFetch, botUpstreamUrl} from "@/lib/bot";
import {chainBySlug} from "@/lib/chains";
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
  demoSniperParams,
  demoSniperPortfolio,
  demoStatus,
} from "@/lib/demo";

export const dynamic = "force-dynamic";

/** Demo runtime mode. Flipped by POST /api/bot/mode in demo mode only. */
let demoLive = false;
const DEMO_MUTATIONS = process.env.ENABLE_DEMO_MUTATIONS === "true";

/** Demo runtime risk envelope — the shape of GET /api/risk. */
let demoRisk = {
  effective: {
    minNetProfitWei: "1",
    maxPositionWei: "100000000000000000000",
    maxBaseFeeWei: "500000000000",
    maxDrawdownWei: "0",
    bribeBps: 9000,
    maxGasPerBundle: 3000000,
    maxInflightPerStrategy: 32,
  },
  boot: {
    minNetProfitWei: "1",
    maxPositionWei: "100000000000000000000",
    maxBaseFeeWei: "500000000000",
    maxDrawdownWei: "0",
    bribeBps: 9000,
    maxGasPerBundle: 3000000,
    maxInflightPerStrategy: 32,
  },
  strategies: [
    "sandwich", "sandwich_v3", "jit", "atomic_arb", "liquidation",
    "liquidation_compound", "liquidation_morpho", "liquidation_maker",
    "oracle_frontrun", "sniper",
  ].map((name) => ({name, enabled: true, bootEnabled: true})),
  killSwitch: {tripped: false, cumulativeNetWei: "-1200000000000000"},
};

/**
 * Strategy eligibility as the bot reports it on `GET /api/config`.
 *
 * Shadow-only is an engineering verdict, not a maturity one: these three are
 * blocked by how their opportunities settle or how they must be ordered, and
 * no amount of soak time changes that. The reason strings are copied verbatim
 * from `Strategy::shadow_only_reason()` so the demo view cannot drift into
 * saying something the bot would not.
 */
const DEMO_ELIGIBILITY = [
  {name: "sandwich", liveCandidate: true, shadowOnlyReason: null},
  {name: "sandwich_v3", liveCandidate: true, shadowOnlyReason: null},
  {name: "atomic_arb", liveCandidate: true, shadowOnlyReason: null},
  {name: "liquidation", liveCandidate: true, shadowOnlyReason: null},
  {name: "liquidation_compound", liveCandidate: true, shadowOnlyReason: null},
  {name: "liquidation_morpho", liveCandidate: true, shadowOnlyReason: null},
  {name: "liquidation_maker", liveCandidate: true, shadowOnlyReason: null},
  {
    name: "jit",
    liveCandidate: false,
    shadowOnlyReason: "position is not yet unwound to one profit token",
  },
  {
    name: "sniper",
    liveCandidate: false,
    shadowOnlyReason: "round-trip probe is not a certified profitable execution strategy",
  },
  {
    name: "oracle_frontrun",
    liveCandidate: false,
    shadowOnlyReason:
      "requires guaranteed pre-update ordering: no builder market or express-lane bid is wired",
  },
];

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
        searcher: "0x00000000000000000000000000000000000f0000",
        liveExecution: demoLive,
        liveArmed: true,
        // Mirrors `Strategy::live_candidate()` / `shadow_only_reason()` in the
        // bot. Kept in sync by hand because the demo generator exists precisely
        // for when no bot is reachable to ask; the reasons are the shipped ones,
        // not placeholders, so the panel reviewed here is the panel operators get.
        strategyEligibility: DEMO_ELIGIBILITY,
        demo: true,
      };
    case "risk":
      return demoRisk;
    case "risk/reset":
      return {ok: true, wasTripped: demoRisk.killSwitch.tripped, tripped: false};
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
    case "actual-mev":
      return {summary: {matches: 0, highConfidence: 0}, matches: []};
    case "reorgs":
      return demoReorgs();
    // Directional sniper lane. The demo view shows a realistic mixed book but
    // always reports the lane as disarmed, matching the shipped defaults.
    case "sniper/portfolio":
      return demoSniperPortfolio();
    case "sniper/params":
      return demoSniperParams();
    case "sniper/positions":
      return {positions: []};
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
  // Multi-chain: the `?chain=` slug selects which bot instance to proxy to.
  // It is stripped before forwarding so the bot never sees it.
  const chainSlug = search.get("chain");
  const rest = new URLSearchParams(search);
  rest.delete("chain");
  const qs = rest.toString();

  if (route === "stream") {
    return streamResponse(qs, chainSlug);
  }

  const upstream = await botFetch(`/api/${route}${qs ? `?${qs}` : ""}`, 2500, chainSlug);
  if (upstream.ok) return jsonResponse(upstream.data, false);
  return jsonResponse(demoFor(route, search), true);
}

/**
 * Same-site guard for the mutating endpoints below, compared on
 * **authority** (host[:port]), not on the full origin.
 *
 * The previous version rejected any request whose `Origin` header did not
 * exactly equal `req.nextUrl.origin` — including the scheme. That broke
 * legitimate control requests the moment the dashboard sat behind a
 * TLS-terminating reverse proxy or a hosted dev preview: the page is served
 * over `https://…` while the dev server speaks plain `http://…`, so the two
 * origins can never be string-equal even though page and route live on the
 * same host and port.
 *
 * What must still hold: the request's site of origin is the dashboard itself.
 * A browser POSTing from another site always sends an `Origin` whose
 * authority is *that site*, so comparing the authority against the request
 * host (or the first `X-Forwarded-Host` hop a proxy sets) rejects cross-site
 * postings without pinning the scheme. `sec-fetch-site: cross-site` remains
 * an independent rejection where the browser supplies it.
 */
function isAllowedControlOrigin(req: NextRequest): boolean {
  if (req.headers.get("sec-fetch-site") === "cross-site") return false;
  const origin = req.headers.get("origin");
  if (!origin) return true;
  let authority: string;
  try {
    authority = new URL(origin).host;
  } catch {
    return false;
  }
  const candidates = [
    req.nextUrl.host,
    req.headers.get("x-forwarded-host")?.split(",")[0].trim(),
    req.headers.get("host"),
  ].filter((h): h is string => Boolean(h));
  return candidates.includes(authority);
}

/**
 * Mutating bot endpoints. Only `/api/mode` (the simulation ⇄ live switch) is
 * proxied; when the bot is unreachable the demo mode state flips instead so
 * the dashboard's switch flow is exercisable without a running bot — always
 * flagged with `demo: true` so nobody mistakes it for a real mode change.
 */
export async function POST(req: NextRequest, {params}: {params: Promise<{path: string[]}>}) {
  if (!isAllowedControlOrigin(req)) {
    return new Response(JSON.stringify({ok: false, error: "cross-origin control request rejected"}), {
      status: 403,
      headers: {"content-type": "application/json"},
    });
  }
  const {path} = await params;
  const route = path.join("/");
  // Multi-chain: mutations apply to the selected chain's bot instance only —
  // the switcher always sends the active slug, and a missing slug means the
  // first (default) chain, never a cross-chain write.
  const chainSlug = req.nextUrl.searchParams.get("chain");

  // The runtime risk endpoints are forwarded verbatim (bar JSON parsing) —
  // validation is the bot's job and its 400 reason must reach the panel.
  // Sniper-lane mutations are forwarded verbatim exactly like the risk
  // endpoints: validation lives in the bot and its 400 reasons must reach the
  // panel unchanged. There is deliberately NO demo fallback that "applies" a
  // sniper patch — pretending to arm a lane that commits real capital is the
  // one place a convincing demo would be actively dangerous.
  const SNIPER_MUTATIONS = ["sniper/params", "sniper/halt", "sniper/resume"];
  if (SNIPER_MUTATIONS.includes(route)) {
    let body: unknown = {};
    try {
      body = await req.json();
    } catch {
      body = {};
    }
    try {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), 3000);
      const upstream = await fetch(botUpstreamUrl(`/api/${route}`, chainSlug), {
        method: "POST",
        signal: controller.signal,
        headers: {"content-type": "application/json", ...botAuthHeaders(chainSlug)},
        body: JSON.stringify(body),
      });
      clearTimeout(timer);
      const data = (await upstream
        .json()
        .catch(() => ({error: `bot returned HTTP ${upstream.status}`}))) as Record<string, unknown>;
      return new Response(JSON.stringify({...data, ok: upstream.ok, demo: false}), {
        status: upstream.status,
        headers: {"content-type": "application/json", "x-data-source": "bot"},
      });
    } catch {
      return new Response(
        JSON.stringify({
          ok: false,
          error: "bot control plane unreachable — the sniper lane was not changed",
          demo: false,
        }),
        {status: 503, headers: {"content-type": "application/json", "x-data-source": "bot"}},
      );
    }
  }

  if (route === "risk" || route === "risk/reset") {
    let body: unknown = {};
    try {
      body = await req.json();
    } catch {
      return jsonResponse({ok: false, error: "invalid JSON body"}, true);
    }
    try {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), 3000);
      const upstream = await fetch(botUpstreamUrl(`/api/${route}`, chainSlug), {
        method: "POST",
        signal: controller.signal,
        headers: {"content-type": "application/json", ...botAuthHeaders(chainSlug)},
        body: JSON.stringify(body),
      });
      clearTimeout(timer);
      const data = (await upstream.json().catch(() => ({error: `bot returned HTTP ${upstream.status}`}))) as Record<string, unknown>;
      return new Response(JSON.stringify({...data, ok: upstream.ok, demo: false}), {
        status: upstream.status,
        headers: {"content-type": "application/json", "x-data-source": "bot"},
      });
    } catch {
      if (!DEMO_MUTATIONS) {
        return new Response(JSON.stringify({ok: false, error: "bot control plane unreachable", demo: false}), {
          status: 503,
          headers: {"content-type": "application/json", "x-data-source": "bot"},
        });
      }
    }
    // Explicit development-only demo fallback: apply to the in-memory envelope so the instant-
    // apply flow is exercisable without a running bot (flagged demo:true).
    if (route === "risk/reset") {
      demoRisk = {...demoRisk, killSwitch: {...demoRisk.killSwitch, tripped: false}};
      return jsonResponse({ok: true, wasTripped: false, tripped: false}, true);
    }
    const patch = (body ?? {}) as Record<string, unknown>;
    if (typeof patch.minNetProfitWei === "string") {
      demoRisk.effective.minNetProfitWei = patch.minNetProfitWei;
    }
    if (typeof patch.bribeBps === "number") {
      demoRisk.effective.bribeBps = patch.bribeBps;
    }
    if (typeof patch.maxPositionWei === "string") {
      demoRisk.effective.maxPositionWei = patch.maxPositionWei;
    }
    if (typeof patch.maxBaseFeeWei === "string") {
      demoRisk.effective.maxBaseFeeWei = patch.maxBaseFeeWei;
    }
    if (typeof patch.maxGasPerBundle === "number") {
      demoRisk.effective.maxGasPerBundle = patch.maxGasPerBundle;
    }
    if (patch.strategies && typeof patch.strategies === "object") {
      for (const [name, on] of Object.entries(patch.strategies as Record<string, boolean>)) {
        const row = demoRisk.strategies.find((r) => r.name === name);
        if (row) row.enabled = Boolean(on);
      }
    }
    return jsonResponse({ok: true, effective: demoRisk.effective, demo: true}, true);
  }

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
    const upstream = await fetch(botUpstreamUrl("/api/mode", chainSlug), {
      method: "POST",
      signal: controller.signal,
      headers: {"content-type": "application/json", ...botAuthHeaders(chainSlug)},
      body: JSON.stringify({live: body.live}),
    });
    clearTimeout(timer);
    const data = (await upstream.json().catch(() => ({error: `bot returned HTTP ${upstream.status}`}))) as Record<string, unknown>;
    return new Response(JSON.stringify({...data, ok: upstream.ok, demo: false}), {
      status: upstream.status,
      headers: {"content-type": "application/json", "x-data-source": "bot"},
    });
  } catch {
    if (!DEMO_MUTATIONS) {
      return new Response(JSON.stringify({ok: false, error: "bot control plane unreachable", demo: false}), {
        status: 503,
        headers: {"content-type": "application/json", "x-data-source": "bot"},
      });
    }
  }

  // Development-only demo fallback. Production never reports a simulated halt as success.
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
async function streamResponse(qs: string, chainSlug?: string | null): Promise<Response> {
  const headers = {
    "content-type": "text/event-stream",
    "cache-control": "no-cache, no-transform",
    connection: "keep-alive",
    "x-accel-buffering": "no",
  };

  try {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 2000);
    const upstream = await fetch(botUpstreamUrl(`/api/stream${qs ? `?${qs}` : ""}`, chainSlug), {
      signal: controller.signal,
      headers: {accept: "text/event-stream", ...botAuthHeaders(chainSlug)},
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
