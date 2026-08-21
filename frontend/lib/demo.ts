/**
 * Demo data.
 *
 * The dashboard is useful before the Rust bot is running: when `BOT_API_URL` is
 * unreachable every endpoint falls back to this generator, and the UI shows a
 * "DEMO DATA" badge so nobody mistakes it for a live run. The shapes here are
 * exactly the shapes `mev-bot` serves.
 */
import type {
  FeedEvent,
  FunnelCounters,
  OpportunityRow,
  PnlResponse,
  RelayBid,
  SeriesPoint,
  SimulationRow,
  StatusResponse,
  Strategy,
} from "./types";

const STRATEGIES: Strategy[] = ["sandwich", "jit", "atomic_arb", "liquidation", "sniper"];
const START_BLOCK = 23_180_000;

// Deterministic PRNG so server and client stay consistent within a run.
function rng(seed: number) {
  let s = seed >>> 0;
  return () => {
    s = (s * 1_664_525 + 1_013_904_223) >>> 0;
    return s / 0xffffffff;
  };
}

const rand = rng(20260820);

function pick<T>(arr: T[], r = rand()): T {
  return arr[Math.floor(r * arr.length) % arr.length];
}

function hash(n: number): string {
  return "0x" + n.toString(16).padStart(8, "0").repeat(8).slice(0, 64);
}

function addr(n: number): string {
  return "0x" + n.toString(16).padStart(8, "0").repeat(5).slice(0, 40);
}

export function demoStatus(): StatusResponse {
  return {
    chain: {id: 1, name: "ethereum"},
    head: {
      number: START_BLOCK + 420,
      hash: hash(9911),
      baseFeeWei: "8320000000",
      gasUsed: 16_942_113,
      timestamp: Math.floor(Date.now() / 1000),
    },
    mode: "simulation",
    strategies: STRATEGIES,
    risk: {
      minNetProfitWei: "1",
      maxPositionWei: "100000000000000000000",
      maxBaseFeeWei: "500000000000",
      bribeBps: 9000,
      killSwitchTripped: false,
      cumulativeNetWei: "-41200000000000000",
    },
    executor: "0x00000000000000000000000000000000000e0000",
    pools: 38,
    stats: {
      pendingSeen: 184_223,
      hintsSeen: 3_401,
      blocksSeen: 420,
      opportunities: 1_284,
      simulations: 1_246,
      submittable: 96,
      rejected: 5_512,
      startedAtMs: Date.now() - 1000 * 60 * 84,
      funnel: demoFunnel(),
    },
    simBackends: {anvilFork: true, relayCallBundle: true},
    demo: true,
  };
}

/**
 * Synthesise plausible per-strategy funnel counters for the demo.
 * The numbers are designed to tell a story: sandwich sees a lot of
 * candidates but most get gated by risk; jit sees nothing because the
 * victim-notional floor is high; atomic_arb has the deepest funnel.
 */
export function demoFunnel(): Record<Strategy, FunnelCounters> {
  const f = (n: Partial<FunnelCounters>): FunnelCounters => ({
    candidatesEmitted: 0,
    candidatesSkipped: 0,
    gatedByRisk: 0,
    missingVictimRaw: 0,
    simulationsSucceeded: 0,
    simulationsReverted: 0,
    simulationsFailed: 0,
    submittable: 0,
    ...n,
  });
  return {
    sandwich: f({
      candidatesEmitted: 184,
      candidatesSkipped: 12_104,
      gatedByRisk: 142,
      missingVictimRaw: 31,
      simulationsSucceeded: 4,
      simulationsReverted: 7,
      simulationsFailed: 0,
      submittable: 4,
    }),
    jit: f({
      candidatesEmitted: 0,
      candidatesSkipped: 184_223,
      gatedByRisk: 0,
      missingVictimRaw: 0,
      simulationsSucceeded: 0,
      simulationsReverted: 0,
      simulationsFailed: 0,
      submittable: 0,
    }),
    atomic_arb: f({
      candidatesEmitted: 76,
      candidatesSkipped: 0,
      gatedByRisk: 12,
      simulationsSucceeded: 18,
      simulationsReverted: 41,
      simulationsFailed: 5,
      submittable: 14,
    }),
    liquidation: f({
      candidatesEmitted: 11,
      candidatesSkipped: 0,
      gatedByRisk: 0,
      simulationsSucceeded: 6,
      simulationsReverted: 4,
      simulationsFailed: 1,
      submittable: 5,
    }),
    sniper: f({
      candidatesEmitted: 0,
      candidatesSkipped: 47,
      gatedByRisk: 0,
      simulationsSucceeded: 0,
      simulationsReverted: 0,
      simulationsFailed: 0,
      submittable: 0,
    }),
  };
}

export function demoSimulations(limit = 60): SimulationRow[] {
  const out: SimulationRow[] = [];
  for (let i = 0; i < limit; i++) {
    const strategy = pick(STRATEGIES);
    const win = rand() > 0.62;
    const gross = win ? Math.floor(rand() * 9e16) + 2e15 : Math.floor(rand() * 4e15);
    const gasUsed = 180_000 + Math.floor(rand() * 400_000);
    const gasCost = gasUsed * 9.2e9;
    const net = Math.floor(gross - gasCost - (win ? gross * 0.9 : 0));
    out.push({
      opportunityId: hash(1000 + i).slice(0, 18),
      strategy,
      backend: rand() > 0.35 ? "anvil_fork" : "relay_call_bundle",
      success: win,
      grossWei: String(Math.floor(gross)),
      gasUsed,
      gasCostWei: String(Math.floor(gasCost)),
      bribeWei: String(Math.floor(win ? gross * 0.9 : 0)),
      netWei: net,
      revertReason: win ? null : pick(["Unprofitable(0, 1)", "victim replay failed: nonce too low", "tx reverted"]),
      targetBlock: START_BLOCK + 420 - Math.floor(i / 3),
      latencyMs: 40 + Math.floor(rand() * 260),
      createdAtMs: Date.now() - i * 9_000,
      notes: demoNote(strategy),
    });
  }
  return out;
}

function demoNote(s: Strategy): string {
  switch (s) {
    case "sandwich":
      return "sandwich WETH/PEPE on univ2 pair 0x9f3…: victim in 4.2 WETH min_out 0 -> front 1.81 WETH";
    case "jit":
      return "jit 0x88e…(USDC/WETH 500) ticks [201360, 201480] L 4.2e21 victim_in 42 WETH";
    case "atomic_arb":
      return "arb 0xb4e… -> 0x397… (univ2 -> sushiv2) in 12.4 WETH gross 0.031 WETH";
    case "liquidation":
      return "aave v3 liquidation user 0x51a… hf 0.972 cover 18,400 USDC";
    case "sniper":
      return "new pair 0x71c… token 0xdd2…; atomic round-trip probe (honeypot/tax check)";
  }
}

export function demoPnl(): PnlResponse {
  const sims = demoSimulations(240);
  const byStrategy = STRATEGIES.map((strategy) => {
    const rows = sims.filter((s) => s.strategy === strategy && s.backend === "anvil_fork");
    const net = rows.reduce((a, r) => a + r.netWei, 0);
    return {
      strategy,
      simulations: rows.length,
      wins: rows.filter((r) => r.netWei > 0).length,
      losses: rows.filter((r) => r.netWei <= 0).length,
      gross_profit_wei: String(rows.reduce((a, r) => a + Number(r.grossWei), 0)),
      gas_spent_wei: String(rows.reduce((a, r) => a + Number(r.gasCostWei), 0)),
      net_profit_wei: net,
      best_net_wei: rows.reduce((a, r) => Math.max(a, r.netWei), 0),
      worst_net_wei: rows.reduce((a, r) => Math.min(a, r.netWei), 0),
      avg_latency_ms: rows.length ? rows.reduce((a, r) => a + r.latencyMs, 0) / rows.length : 0,
    };
  });
  return {
    byStrategy,
    totalNetWei: byStrategy.reduce((a, r) => a + r.net_profit_wei, 0),
    demo: true,
  };
}

export function demoSeries(limit = 120): SeriesPoint[] {
  const out: SeriesPoint[] = [];
  for (let i = 0; i < limit; i++) {
    out.push({
      block: START_BLOCK + 300 + i,
      netWei: Math.floor((rand() - 0.42) * 3.2e16),
      count: 1 + Math.floor(rand() * 5),
    });
  }
  return out;
}

export function demoOpportunities(limit = 40): OpportunityRow[] {
  return demoSimulations(limit).map((s, i) => ({
    id: s.opportunityId,
    strategy: s.strategy,
    targetBlock: s.targetBlock,
    profitToken: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
    expectedWei: s.grossWei,
    notionalWei: String(Math.floor(Number(s.grossWei) * 40)),
    victims: i % 3 === 0 ? hash(500 + i) : "",
    notes: s.notes,
    createdAtMs: s.createdAtMs,
  }));
}

export function demoRelayBids(limit = 30): RelayBid[] {
  const relays = [
    "https://boost-relay.flashbots.net",
    "https://bloxroute.max-profit.blxrbdn.com",
    "https://agnostic-relay.net",
  ];
  const out: RelayBid[] = [];
  for (let i = 0; i < limit; i++) {
    out.push({
      relay: pick(relays),
      slot: 9_812_400 - i,
      builder: "0x" + (0xbe3f + i).toString(16).repeat(12).slice(0, 96),
      valueWei: String(Math.floor(rand() * 4e17 + 1e16)),
      seenAtMs: Date.now() - i * 12_000,
    });
  }
  return out;
}

/** One synthetic feed event, used by the demo SSE stream. */
export function demoEvent(i: number): FeedEvent {
  const r = rand();
  if (i % 24 === 0) {
    return {
      kind: "block",
      number: START_BLOCK + 420 + Math.floor(i / 24),
      hash: hash(i + 7),
      base_fee_per_gas: String(Math.floor(7e9 + rand() * 4e9)),
      gas_used: 14_000_000 + Math.floor(rand() * 4_000_000),
      timestamp: Math.floor(Date.now() / 1000),
    };
  }
  if (r < 0.08) {
    const strategy = pick(STRATEGIES);
    return {
      kind: "opportunity",
      id: hash(i).slice(0, 18),
      strategy,
      notes: demoNote(strategy),
      expected_profit_wei: String(Math.floor(rand() * 6e16)),
      target_block: START_BLOCK + 420,
    };
  }
  if (r < 0.16) {
    const strategy = pick(STRATEGIES);
    const win = rand() > 0.65;
    const gross = Math.floor(rand() * 8e16);
    return {
      kind: "simulation",
      opportunity_id: hash(i).slice(0, 18),
      strategy,
      backend: "anvil_fork",
      success: win,
      net_profit_wei: win ? Math.floor(gross * 0.08) : -Math.floor(rand() * 4e15),
      gas_used: 210_000 + Math.floor(rand() * 300_000),
      gross_profit_wei: String(gross),
      revert_reason: win ? null : "Unprofitable(0, 1)",
    };
  }
  if (r < 0.22) {
    return {
      kind: "mev_share_hint",
      hash: hash(i * 3),
      logs: Math.floor(rand() * 5),
      functions: [pick(["0x38ed1739", "0x7ff36ab5", "0x04e45aaf", "0x3593564c"])],
      seen_at_ms: Date.now(),
    };
  }
  return {
    kind: "pending",
    hash: hash(i * 11),
    from: addr(i + 3),
    to: pick([
      "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D",
      "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45",
      "0x3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD",
      addr(i + 99),
    ]),
    value: String(Math.floor(rand() * 3e18)),
    gas: 120_000 + Math.floor(rand() * 300_000),
    source: pick(["public_mempool", "mev_share", "external_stream"]),
    selector: pick(["0x38ed1739", "0x7ff36ab5", "0xa9059cbb", "0x04e45aaf"]),
    seen_at_ms: Date.now(),
  };
}
