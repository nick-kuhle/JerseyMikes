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
  RelayBlockRow,
  RelayBlockTxRow,
  SeriesPoint,
  SimulationRow,
  StatusResponse,
  Strategy,
} from "./types";

const STRATEGIES: Strategy[] = [
  "sandwich",
  "sandwich_v3",
  "jit",
  "atomic_arb",
  "liquidation",
  "liquidation_compound",
  "liquidation_morpho",
  "liquidation_maker",
  "oracle_frontrun",
  "sniper",
];
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
      reorgsSeen: 1,
      startedAtMs: Date.now() - 1000 * 60 * 84,
      funnel: demoFunnel(),
      funnelReplay: demoFunnelReplay(),
    },
    simBackends: {anvilFork: true, relayCallBundle: true},
    inventory: {
      nonce: 17,
      ethWei: "2500000000000000000",
      wethWei: "12000000000000000000",
      availableWei: "14500000000000000000",
      gate: false,
    },
    latency: demoLatency(),
    demo: true,
  };
}

export function demoLatency(): import("./types").LatencySnapshot {
  const hist = (count: number, p50: number, p95: number, p99: number) => ({
    count,
    meanMs: p50 + 8,
    minMs: 1,
    maxMs: p99 + 40,
    p50Ms: p50,
    p95Ms: p95,
    p99Ms: p99,
  });
  return {
    budgetMs: 150,
    withinBudget: true,
    stages: {
      ingest_to_strategy: hist(184_223, 8, 20, 50),
      strategy: hist(12_400, 4, 12, 25),
      risk: hist(1_284, 0, 1, 2),
      simulation: hist(1_246, 40, 90, 140),
      total: hist(1_246, 55, 120, 180),
    },
  };
}

export function demoCompetition(): import("./types").CompetitionResponse {
  return {
    summary: {
      rows: 42,
      truePositives: 11,
      falsePositives: 6,
      wouldOutbid: 4,
      victimsLanded: 18,
      meanInclusionP: 0.31,
    },
    recent: demoSimulations(8).map((s, i) => ({
      blockNumber: s.targetBlock,
      opportunityId: s.opportunityId,
      strategy: s.strategy,
      simNetWei: s.netWei,
      ourBribeWei: s.bribeWei,
      winningBidWei: String(Math.floor(Number(s.bribeWei) * (0.7 + rand()))),
      victimLanded: i % 3 !== 0,
      wouldOutbid: i % 5 === 0,
      inclusionP: Math.min(0.99, 0.12 + i * 0.07),
      truePositive: s.success && i % 3 !== 0,
      falsePositive: s.success && i % 3 === 0,
      reorged: false,
      createdAtMs: s.createdAtMs,
    })),
    demo: true,
  };
}

export function demoReorgs(): import("./types").ReorgRow[] {
  return [
    {
      fromBlock: START_BLOCK + 390,
      toBlock: START_BLOCK + 391,
      depth: 2,
      oldHash: hash(42),
      newHash: hash(43),
      seenAtMs: Date.now() - 3_600_000,
    },
  ];
}

/**
 * Synthesise plausible per-strategy funnel counters for the demo.
 * The numbers are designed to tell a story: sandwich sees a lot of
 * candidates but most get gated by risk; jit sees nothing because the
 * victim-notional floor is high; atomic_arb has the deepest funnel.
 */
export function demoFunnel(): Record<Strategy, FunnelCounters> {
  const f = (n: Partial<FunnelCounters>): FunnelCounters => ({
    invocationsWithOutput: 0,
    invocationsEmpty: 0,
    candidatesEmitted: 0,
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
      invocationsWithOutput: 184,
      invocationsEmpty: 12_104,
      candidatesEmitted: 221,
      gatedByRisk: 142,
      missingVictimRaw: 31,
      simulationsSucceeded: 4,
      simulationsReverted: 7,
      simulationsFailed: 0,
      submittable: 4,
    }),
    sandwich_v3: f({
      invocationsWithOutput: 0,
      invocationsEmpty: 0,
      candidatesEmitted: 0,
    }),
    jit: f({
      invocationsWithOutput: 0,
      invocationsEmpty: 184_223,
      candidatesEmitted: 0,
      gatedByRisk: 0,
      missingVictimRaw: 0,
      simulationsSucceeded: 0,
      simulationsReverted: 0,
      simulationsFailed: 0,
      submittable: 0,
    }),
    atomic_arb: f({
      invocationsWithOutput: 76,
      invocationsEmpty: 0,
      candidatesEmitted: 412,
      gatedByRisk: 12,
      simulationsSucceeded: 18,
      simulationsReverted: 41,
      simulationsFailed: 5,
      submittable: 14,
    }),
    liquidation: f({
      invocationsWithOutput: 11,
      invocationsEmpty: 0,
      candidatesEmitted: 11,
      gatedByRisk: 0,
      simulationsSucceeded: 6,
      simulationsReverted: 4,
      simulationsFailed: 1,
      submittable: 5,
    }),
    liquidation_compound: f({
      invocationsWithOutput: 0,
      invocationsEmpty: 36_542,
      candidatesEmitted: 2,
      gatedByRisk: 1,
      simulationsSucceeded: 0,
      simulationsReverted: 2,
      simulationsFailed: 0,
      submittable: 0,
    }),
    liquidation_morpho: f({
      invocationsWithOutput: 1,
      invocationsEmpty: 36_541,
      candidatesEmitted: 3,
      gatedByRisk: 1,
      missingVictimRaw: 0,
      simulationsSucceeded: 1,
      simulationsReverted: 1,
      simulationsFailed: 0,
      submittable: 1,
    }),
    liquidation_maker: f({
      invocationsWithOutput: 0,
      invocationsEmpty: 36_542,
      candidatesEmitted: 1,
      gatedByRisk: 0,
      simulationsSucceeded: 0,
      simulationsReverted: 1,
      simulationsFailed: 0,
      submittable: 0,
    }),
    oracle_frontrun: f({
      invocationsWithOutput: 3,
      invocationsEmpty: 41_204,
      candidatesEmitted: 6,
      gatedByRisk: 2,
      missingVictimRaw: 1,
      simulationsSucceeded: 1,
      simulationsReverted: 2,
      simulationsFailed: 0,
      submittable: 1,
    }),
    sniper: f({
      invocationsWithOutput: 0,
      invocationsEmpty: 47,
      candidatesEmitted: 0,
      gatedByRisk: 0,
      simulationsSucceeded: 0,
      simulationsReverted: 0,
      simulationsFailed: 0,
      submittable: 0,
    }),
  };
}

/**
 * The replay lane: bloXroute delivered-block transactions scored after the
 * fact. Volumes are an order of magnitude above the live lane because every
 * delivered block contributes ~150 already-mined transactions — which is
 * exactly why the two lanes are counted separately.
 */
export function demoFunnelReplay(): Record<Strategy, FunnelCounters> {
  const f = (n: Partial<FunnelCounters>): FunnelCounters => ({
    invocationsWithOutput: 0,
    invocationsEmpty: 0,
    candidatesEmitted: 0,
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
      invocationsWithOutput: 2_940,
      invocationsEmpty: 121_060,
      candidatesEmitted: 3_512,
      gatedByRisk: 2_701,
      missingVictimRaw: 402,
      simulationsSucceeded: 118,
      simulationsReverted: 291,
      simulationsFailed: 12,
      submittable: 96,
    }),
    sandwich_v3: f({invocationsEmpty: 124_000}),
    jit: f({invocationsEmpty: 124_000}),
    atomic_arb: f({
      invocationsWithOutput: 1_204,
      invocationsEmpty: 122_796,
      candidatesEmitted: 6_880,
      gatedByRisk: 5_910,
      simulationsSucceeded: 402,
      simulationsReverted: 511,
      simulationsFailed: 57,
      submittable: 288,
    }),
    liquidation: f({
      invocationsWithOutput: 42,
      invocationsEmpty: 123_958,
      candidatesEmitted: 42,
      simulationsSucceeded: 19,
      simulationsReverted: 21,
      simulationsFailed: 2,
      submittable: 17,
    }),
    liquidation_compound: f({invocationsEmpty: 73_608, invocationsWithOutput: 4, candidatesEmitted: 4}),
    liquidation_morpho: f({invocationsEmpty: 73_610, invocationsWithOutput: 6, candidatesEmitted: 9}),
    liquidation_maker: f({invocationsEmpty: 73_611, invocationsWithOutput: 2, candidatesEmitted: 2}),
    oracle_frontrun: f({invocationsEmpty: 82_408, invocationsWithOutput: 12, candidatesEmitted: 24}),
    sniper: f({invocationsEmpty: 123_806, invocationsWithOutput: 194, candidatesEmitted: 194}),
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
      netWei: String(Math.floor(net)),
      revertReason: win ? null : pick(["Unprofitable(0, 1)", "victim replay failed: nonce too low", "tx reverted"]),
      targetBlock: START_BLOCK + 420 - Math.floor(i / 3),
      latencyMs: 40 + Math.floor(rand() * 260),
      createdAtMs: Date.now() - i * 9_000,
      notes: demoNote(strategy),
      victims: hash(5000 + i),
    });
  }
  return out;
}

function demoNote(s: Strategy): string {
  switch (s) {
    case "sandwich":
      return "sandwich WETH/PEPE on univ2 pair 0x9f3…: victim in 4.2 WETH min_out 0 -> front 1.81 WETH";
    case "sandwich_v3":
      return "sandwich_v3 USDC fee 500 pool 0x88e…: victim in 12 WETH min_out 0 -> front 2.4 WETH";
    case "jit":
      return "jit 0x88e…(USDC/WETH 500) ticks [201360, 201480] L 4.2e21 victim_in 42 WETH";
    case "atomic_arb":
      return "arb 0xb4e… -> 0x397… (univ2 -> sushiv2) in 12.4 WETH gross 0.031 WETH";
    case "liquidation":
      return "aave v3 liquidation user 0x51a… hf 0.972 cover 18,400 USDC";
    case "liquidation_compound":
      return "compound v3 absorb+buyCollateral account 0x77c… storefront discount on 3 assets";
    case "liquidation_morpho":
      return "morpho blue liquidate market 0xf8c… borrower 0x2b6… full close repay 41,200 USDC";
    case "liquidation_maker":
      return "maker ETH-A bark urn 0x824… + clip.take id 34,882 slice 6.1 WETH";
    case "oracle_frontrun":
      return "back-run ETH/USD transmit 0x41f… via chainlink: 2 near-miss liquidations rebuilt";
    case "sniper":
      return "new pair 0x71c… token 0xdd2…; atomic round-trip probe (honeypot/tax check)";
  }
}

export function demoPnl(): PnlResponse {
  const sims = demoSimulations(240);
  const byStrategy = STRATEGIES.map((strategy) => {
    const rows = sims.filter((s) => s.strategy === strategy && s.backend === "anvil_fork");
    const net = rows.reduce((a, r) => a + Number(r.netWei), 0);
    return {
      strategy,
      simulations: rows.length,
      wins: rows.filter((r) => BigInt(r.netWei) > 0n).length,
      losses: rows.filter((r) => BigInt(r.netWei) <= 0n).length,
      gross_profit_wei: String(rows.reduce((a, r) => a + Number(r.grossWei), 0)),
      gas_spent_wei: String(rows.reduce((a, r) => a + Number(r.gasCostWei), 0)),
      net_profit_wei: String(Math.floor(net)),
      best_net_wei: String(rows.reduce((a, r) => Math.max(a, Number(r.netWei)), 0)),
      worst_net_wei: String(rows.reduce((a, r) => Math.min(a, Number(r.netWei)), 0)),
      avg_latency_ms: rows.length ? rows.reduce((a, r) => a + r.latencyMs, 0) / rows.length : 0,
    };
  });
  return {
    byStrategy,
    totalNetWei: String(byStrategy.reduce((a, r) => a + Number(r.net_profit_wei), 0)),
    demo: true,
  };
}

export function demoSeries(limit = 120): SeriesPoint[] {
  const out: SeriesPoint[] = [];
  for (let i = 0; i < limit; i++) {
    out.push({
      block: START_BLOCK + 300 + i,
      netWei: String(Math.floor((rand() - 0.42) * 3.2e16)),
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

const BLOXROUTE_RELAY = "https://bloxroute.max-profit.blxrbdn.com";

export function demoRelayBlocks(limit = 25): RelayBlockRow[] {
  const out: RelayBlockRow[] = [];
  for (let i = 0; i < limit; i++) {
    out.push({
      relay: BLOXROUTE_RELAY,
      slot: 9_812_400 - i,
      blockNumber: START_BLOCK + 420 - i,
      blockHash: hash(9000 + i),
      builder: "0x" + (0x94aa + i).toString(16).repeat(12).slice(0, 96),
      valueWei: String(Math.floor(rand() * 3e17 + 8e15)),
      gasUsed: 12_000_000 + Math.floor(rand() * 18_000_000),
      numTx: 200 + Math.floor(rand() * 250),
      seenAtMs: Date.now() - i * 12_000,
    });
  }
  return out;
}

export function demoRelayTxs(blockNumber?: number, limit = 200): RelayBlockTxRow[] {
  const base = blockNumber ?? START_BLOCK + 420;
  const out: RelayBlockTxRow[] = [];
  const selectors = ["0x38ed1739", "0x7ff36ab5", "0x04e45aaf", "0x3593564c", "0xa9059cbb", null];
  for (let i = 0; i < limit; i++) {
    const sel = pick(selectors);
    out.push({
      blockNumber: base,
      txIndex: i,
      hash: hash(7000 + i),
      from: addr(i + 31),
      to: sel
        ? pick([
            "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D",
            "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45",
            "0x3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD",
          ])
        : addr(i + 77),
      valueWei: sel ? "0" : String(Math.floor(rand() * 1e18)),
      nonce: i,
      gas: 120_000 + Math.floor(rand() * 400_000),
      selector: sel,
      input: sel ? "0x" + sel.slice(2) + "0".repeat(500) : "0x",
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
      net_profit_wei: String(win ? Math.floor(gross * 0.08) : -Math.floor(rand() * 4e15)),
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
  if (r < 0.26) {
    const blocks = demoRelayBlocks(1);
    return {
      kind: "relay_block",
      block: {
        relay: blocks[0].relay,
        slot: blocks[0].slot,
        block_number: blocks[0].blockNumber,
        block_hash: blocks[0].blockHash,
        builder: blocks[0].builder,
        value_wei: blocks[0].valueWei,
        gas_used: blocks[0].gasUsed,
        num_tx: blocks[0].numTx,
      },
      tx_count: blocks[0].numTx,
      txs: demoRelayTxs(blocks[0].blockNumber, 3).map((t) => ({
        hash: t.hash,
        from: t.from,
        to: t.to,
        value: t.valueWei,
        selector: t.selector,
      })),
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

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Directional sniper lane
// ---------------------------------------------------------------------------

let demoSniperState = {
  enabled: false,
  simulationBalanceWei: "1000000000000000000",
  buySizeWei: "0",
  minLiquidityWei: "2000000000000000000",
  maxPriceImpactBps: 300,
  takeProfitBps: 10000,
  takeProfitAbsWei: "0",
  sellFractionBps: 10000,
  stopLossBps: 5000,
  trailingStopBps: 0,
  maxHoldSecs: 1800,
  maxConcurrentPositions: 1,
  dailyBudgetWei: "0",
  totalBudgetWei: "0",
  maxDrawdownWei: "0",
  requireHoneypotPass: true,
  maxBuyTaxBps: 500,
  maxSellTaxBps: 500,
  minHoldBlocks: 1,
  requireLpLocked: false,
  halted: false,
  haltReason: null as string | null,
};

export function updateDemoSniperParams(patch: Record<string, unknown>) {
  if (typeof patch.enabled === "boolean") demoSniperState.enabled = patch.enabled;
  if (typeof patch.buySizeWei === "string") demoSniperState.buySizeWei = patch.buySizeWei;
  if (typeof patch.buy_size_wei === "string") demoSniperState.buySizeWei = patch.buy_size_wei;
  if (typeof patch.dailyBudgetWei === "string") demoSniperState.dailyBudgetWei = patch.dailyBudgetWei;
  if (typeof patch.daily_budget_wei === "string") demoSniperState.dailyBudgetWei = patch.daily_budget_wei;
  if (typeof patch.totalBudgetWei === "string") demoSniperState.totalBudgetWei = patch.totalBudgetWei;
  if (typeof patch.total_budget_wei === "string") demoSniperState.totalBudgetWei = patch.total_budget_wei;
  if (typeof patch.takeProfitBps === "number") demoSniperState.takeProfitBps = patch.takeProfitBps;
  if (typeof patch.take_profit_bps === "number") demoSniperState.takeProfitBps = patch.take_profit_bps;
  if (typeof patch.takeProfitAbsWei === "string") demoSniperState.takeProfitAbsWei = patch.takeProfitAbsWei;
  if (typeof patch.take_profit_abs_wei === "string") demoSniperState.takeProfitAbsWei = patch.take_profit_abs_wei;
  if (typeof patch.sellFractionBps === "number") demoSniperState.sellFractionBps = patch.sellFractionBps;
  if (typeof patch.sell_fraction_bps === "number") demoSniperState.sellFractionBps = patch.sell_fraction_bps;
  if (typeof patch.stopLossBps === "number") demoSniperState.stopLossBps = patch.stopLossBps;
  if (typeof patch.stop_loss_bps === "number") demoSniperState.stopLossBps = patch.stop_loss_bps;
  if (typeof patch.trailingStopBps === "number") demoSniperState.trailingStopBps = patch.trailingStopBps;
  if (typeof patch.trailing_stop_bps === "number") demoSniperState.trailingStopBps = patch.trailing_stop_bps;
  if (typeof patch.maxHoldSecs === "number") demoSniperState.maxHoldSecs = patch.maxHoldSecs;
  if (typeof patch.max_hold_secs === "number") demoSniperState.maxHoldSecs = patch.max_hold_secs;
  if (typeof patch.maxConcurrentPositions === "number") demoSniperState.maxConcurrentPositions = patch.maxConcurrentPositions;
  if (typeof patch.max_concurrent_positions === "number") demoSniperState.maxConcurrentPositions = patch.max_concurrent_positions;
  if (typeof patch.minLiquidityWei === "string") demoSniperState.minLiquidityWei = patch.minLiquidityWei;
  if (typeof patch.min_liquidity_wei === "string") demoSniperState.minLiquidityWei = patch.min_liquidity_wei;
  if (typeof patch.maxPriceImpactBps === "number") demoSniperState.maxPriceImpactBps = patch.maxPriceImpactBps;
  if (typeof patch.max_price_impact_bps === "number") demoSniperState.maxPriceImpactBps = patch.max_price_impact_bps;
  if (typeof patch.maxBuyTaxBps === "number") demoSniperState.maxBuyTaxBps = patch.maxBuyTaxBps;
  if (typeof patch.max_buy_tax_bps === "number") demoSniperState.maxBuyTaxBps = patch.max_buy_tax_bps;
  if (typeof patch.maxSellTaxBps === "number") demoSniperState.maxSellTaxBps = patch.maxSellTaxBps;
  if (typeof patch.max_sell_tax_bps === "number") demoSniperState.maxSellTaxBps = patch.max_sell_tax_bps;
  if (typeof patch.minHoldBlocks === "number") demoSniperState.minHoldBlocks = patch.minHoldBlocks;
  if (typeof patch.min_hold_blocks === "number") demoSniperState.minHoldBlocks = patch.min_hold_blocks;
  if (typeof patch.requireHoneypotPass === "boolean") demoSniperState.requireHoneypotPass = patch.requireHoneypotPass;
  if (typeof patch.require_honeypot_pass === "boolean") demoSniperState.requireHoneypotPass = patch.require_honeypot_pass;
  if (typeof patch.requireLpLocked === "boolean") demoSniperState.requireLpLocked = patch.requireLpLocked;
  if (typeof patch.require_lp_locked === "boolean") demoSniperState.requireLpLocked = patch.require_lp_locked;
  return demoSniperParams();
}

export function setDemoSniperHalted(halted: boolean, reason: string | null = null) {
  demoSniperState.halted = halted;
  demoSniperState.haltReason = reason;
  return demoSniperParams();
}

export function resetDemoSniperFunds() {
  demoSniperState.simulationBalanceWei = "1000000000000000000";
  return demoSniperParams();
}

/**
 * Demo sniper mode payload — mirrors `GET /api/sniper/mode`. Demo is always
 * simulation-only with no live ceiling: the demo generator can never grant a
 * path to real funds.
 */
export function demoSniperMode() {
  return {
    atomicMode: "simulation",
    sniperMode: "simulation",
    sniperLiveBootEnabled: false,
    canSwitchLive: false,
    blockers: [
      "SNIPER_LIVE_ENABLED was false at boot — restart with the live sniper ceiling to allow Sniper Live",
      "production SniperVault is not configured (SNIPER_VAULT_ADDRESS)",
      "SNIPER_SEARCHER_PRIVATE_KEY is not configured",
    ],
    simulationVaultAddress: null,
    productionVaultAddress: null,
    simulationBalanceWei: demoSniperState.simulationBalanceWei,
    simulationChainId: 1,
    activeVault: {kind: "none", address: null},
    fixture: {available: false, deployed: false, searcher: null, owner: null},
  };
}

/**
 * Demo sniper params.
 */
export function demoSniperParams() {
  const params = {
    enabled: demoSniperState.enabled,
    paperMode: true,
    simulationBalanceWei: demoSniperState.simulationBalanceWei,
    vaultAddress: null as string | null,
    buySizeWei: demoSniperState.buySizeWei,
    minLiquidityWei: demoSniperState.minLiquidityWei,
    maxPriceImpactBps: demoSniperState.maxPriceImpactBps,
    takeProfitBps: demoSniperState.takeProfitBps,
    takeProfitAbsWei: demoSniperState.takeProfitAbsWei,
    sellFractionBps: demoSniperState.sellFractionBps,
    stopLossBps: demoSniperState.stopLossBps,
    trailingStopBps: demoSniperState.trailingStopBps,
    maxHoldSecs: demoSniperState.maxHoldSecs,
    maxConcurrentPositions: demoSniperState.maxConcurrentPositions,
    dailyBudgetWei: demoSniperState.dailyBudgetWei,
    totalBudgetWei: demoSniperState.totalBudgetWei,
    maxDrawdownWei: demoSniperState.maxDrawdownWei,
    requireHoneypotPass: demoSniperState.requireHoneypotPass,
    maxBuyTaxBps: demoSniperState.maxBuyTaxBps,
    maxSellTaxBps: demoSniperState.maxSellTaxBps,
    minHoldBlocks: demoSniperState.minHoldBlocks,
    requireLpLocked: demoSniperState.requireLpLocked,
  };

  const blockers: string[] = [];
  if (!params.enabled) {
    blockers.push(
      "SNIPER_DIRECTIONAL is off (shadow mode: launches are observed and honeypot-checked, never bought)",
    );
  }
  if (params.buySizeWei === "0" || BigInt(params.buySizeWei || "0") === 0n) {
    blockers.push("buySizeWei is 0");
  }
  if (params.dailyBudgetWei === "0" || BigInt(params.dailyBudgetWei || "0") === 0n) {
    blockers.push("dailyBudgetWei is 0");
  } else if (
    BigInt(params.buySizeWei || "0") > BigInt(params.dailyBudgetWei || "0")
  ) {
    blockers.push("buySizeWei exceeds dailyBudgetWei");
  }
  if (!params.requireHoneypotPass) {
    blockers.push(
      "WARNING: requireHoneypotPass is off — tokens with unknown honeypot status will be bought",
    );
  }

  const isArmed =
    params.enabled &&
    BigInt(params.buySizeWei || "0") > 0n &&
    BigInt(params.dailyBudgetWei || "0") >= BigInt(params.buySizeWei || "0") &&
    !demoSniperState.halted;

  return {
    params,
    paperMode: true,
    simulationBalanceWei: demoSniperState.simulationBalanceWei,
    armed: isArmed,
    bootEnabled: true,
    halted: demoSniperState.halted,
    haltReason: demoSniperState.haltReason,
    armingBlockers: blockers,
    rejections: {honeypot: 47, liquidity_thin: 112, tax_too_high: 9, not_armed: 168},
    envSnippet: [
      `SNIPER_DIRECTIONAL=${params.enabled}`,
      `SNIPER_BUY_SIZE_WEI=${params.buySizeWei}`,
      `SNIPER_DAILY_BUDGET_WEI=${params.dailyBudgetWei}`,
      `SNIPER_TAKE_PROFIT_BPS=${params.takeProfitBps}`,
      `SNIPER_STOP_LOSS_BPS=${params.stopLossBps}`,
      `SNIPER_SELL_FRACTION_BPS=${params.sellFractionBps}`,
    ].join("\n"),
    demo: true,
  };
}

/**
 * Demo portfolio. Deliberately shows a *mixed* book — one winner, one loser,
 * one scaled-out runner and one honeypot escape — because a demo that only
 * shows profit teaches the wrong thing about this lane.
 */
export function demoSniperPortfolio() {
  const now = Date.now();
  const row = (
    id: string,
    symbol: string,
    state: string,
    entryEth: number,
    markEth: number,
    realizedEth: number,
    ageMin: number,
    exitReason: string | null,
  ) => {
    const wei = (n: number) => BigInt(Math.round(n * 1e18)).toString();
    const entry = BigInt(Math.round(entryEth * 1e18));
    const mark = BigInt(Math.round(markEth * 1e18));
    const realized = BigInt(Math.round(realizedEth * 1e18));
    const gas = BigInt(Math.round(0.004 * 1e18));
    const net = realized + mark - entry - gas;
    const closed = state === "closed" || state === "abandoned";
    return {
      id,
      token: `0x${id.padEnd(40, "a")}`.slice(0, 42),
      pair: `0x${id.padEnd(40, "b")}`.slice(0, 42),
      venue: "univ2",
      state,
      symbol,
      entryCostWei: entry.toString(),
      entryQty: "1000000000000000000000000",
      remainingQty: closed ? "0" : "600000000000000000000000",
      realizedWei: realized.toString(),
      gasSpentWei: gas.toString(),
      markValueWei: closed ? "0" : mark.toString(),
      unrealizedPnlWei: closed ? "0" : (mark - entry).toString(),
      netPnlWei: net.toString(),
      netPnlBps: entry === 0n ? 0 : Number((net * 10000n) / entry),
      markStale: false,
      openedBlock: 21_000_000 - ageMin,
      openedAtMs: now - ageMin * 60_000,
      closedAtMs: closed ? now - Math.floor(ageMin / 2) * 60_000 : null,
      ageSecs: ageMin * 60,
      exitReason,
      entryVerdict: "clean",
      notes: `backrun of addLiquidityETH; ${wei(entryEth)} wei committed`,
      executionMode: "simulation",
      settlement: "paper",
      txStatus: closed ? "mined" : "mined",
    };
  };

  const open = [
    row("a1", "PEPE2", "open", 0.1, 0.184, 0, 12, null),
    row("b2", "WOJAK", "scaling", 0.1, 0.062, 0.11, 41, null),
  ];
  const closed = [
    row("c3", "MOON", "closed", 0.1, 0, 0.223, 180, "take_profit_pct"),
    row("d4", "RUGME", "closed", 0.1, 0, 0.041, 260, "stop_loss"),
    row("e5", "TRAP", "closed", 0.1, 0, 0.002, 300, "honeypot_detected"),
  ];

  const sum = (rows: typeof open, f: (r: (typeof open)[number]) => string) =>
    rows.reduce((acc, r) => acc + BigInt(f(r)), 0n).toString();

  return {
    totals: {
      openPositions: open.length,
      closedPositions: closed.length,
      openCostWei: sum(open, (r) => r.entryCostWei),
      openValueWei: sum(open, (r) => r.markValueWei),
      unrealizedPnlWei: sum(open, (r) => r.unrealizedPnlWei),
      realizedPnlWei: sum(closed, (r) => r.netPnlWei),
      totalPnlWei: (
        BigInt(sum(open, (r) => r.unrealizedPnlWei)) + BigInt(sum(closed, (r) => r.netPnlWei))
      ).toString(),
      gasSpentWei: sum([...open, ...closed], (r) => r.gasSpentWei),
      deployedTotalWei: sum([...open, ...closed], (r) => r.entryCostWei),
      deployedTodayWei: sum(open, (r) => r.entryCostWei),
      wins: 1,
      losses: 2,
      winRateBps: 3333,
      anyMarkStale: false,
    },
    totalsByMode: {
      simulation: {
        openPositions: open.length,
        closedPositions: closed.length,
        openCostWei: sum(open, (r) => r.entryCostWei),
        openValueWei: sum(open, (r) => r.markValueWei),
        unrealizedPnlWei: sum(open, (r) => r.unrealizedPnlWei),
        realizedPnlWei: sum(closed, (r) => r.netPnlWei),
        totalPnlWei: (
          BigInt(sum(open, (r) => r.unrealizedPnlWei)) + BigInt(sum(closed, (r) => r.netPnlWei))
        ).toString(),
        gasSpentWei: sum([...open, ...closed], (r) => r.gasSpentWei),
        deployedTotalWei: sum([...open, ...closed], (r) => r.entryCostWei),
        deployedTodayWei: sum(open, (r) => r.entryCostWei),
        wins: 1,
        losses: 2,
        winRateBps: 3333,
        anyMarkStale: false,
      },
      live: {
        openPositions: 0,
        closedPositions: 0,
        openCostWei: "0",
        openValueWei: "0",
        unrealizedPnlWei: "0",
        realizedPnlWei: "0",
        totalPnlWei: "0",
        gasSpentWei: "0",
        deployedTotalWei: "0",
        deployedTodayWei: "0",
        wins: 0,
        losses: 0,
        winRateBps: 0,
        anyMarkStale: false,
      },
    },
    open,
    recentClosed: closed,
    armingBlockers: demoSniperParams().armingBlockers,
    armed: false,
    generatedAtMs: now,
  };
}

export function demoSniperVault() {
  return {
    configured: true,
    address: "0x3333333333333333333333333333333333333333",
    spendableRemainingWei: "250000000000000000",
    dailyBudgetWei: "250000000000000000",
    totalBudgetWei: "1000000000000000000",
    windowResetTimeSecs: Math.floor(Date.now() / 1000) + 43200,
  };
}
