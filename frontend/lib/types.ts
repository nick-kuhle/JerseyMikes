export type Strategy =
  | "sandwich"
  | "sandwich_v3"
  | "jit"
  | "atomic_arb"
  | "liquidation"
  | "liquidation_compound"
  | "liquidation_morpho"
  | "liquidation_maker"
  | "oracle_frontrun"
  | "sniper";

/**
 * One row of `GET /api/config` → `strategyEligibility`.
 *
 * Engineering eligibility, which is a different question from qualification:
 * this says whether a strategy *may ever* broadcast, while
 * `StrategyQualification.verdict` says whether it has earned the right to.
 * A shadow-only row can never reach `PASS`, so showing the reason is what
 * stops an operator waiting on evidence that will never arrive.
 */
export interface StrategyEligibility {
  name: Strategy;
  liveCandidate: boolean;
  /** Null for live candidates; a specific engineering reason otherwise. */
  shadowOnlyReason: string | null;
}

/** GET /api/config — boot-time facts about the running bot. */
export interface ConfigResponse {
  chainId: number;
  weth?: string;
  executor: string;
  searcher?: string;
  liveExecution: boolean;
  liveArmed: boolean;
  broadcastEnabled?: boolean;
  strategyEligibility?: StrategyEligibility[];
  endpoints?: {
    ws: boolean;
    mevShare: boolean;
    relays: number;
    sequencerFeed: boolean;
    externalMempools: number;
  };
  bloxrouteRelay?: {url: string; txIngest: boolean};
  demo?: boolean;
}

/** GET /api/risk — the runtime risk envelope vs its boot values. */
export interface RiskStateResponse {
  effective: RiskValues;
  boot: RiskValues;
  strategies: {name: Strategy; enabled: boolean; bootEnabled: boolean}[];
  killSwitch: {tripped: boolean; cumulativeNetWei: string};
}

export interface RiskValues {
  minNetProfitWei: string;
  maxPositionWei: string;
  maxBaseFeeWei: string;
  maxDrawdownWei: string;
  bribeBps: number;
  maxGasPerBundle: number;
  maxInflightPerStrategy: number;
}

export interface StrategyQualification {
  strategy: Strategy;
  liveCandidate: boolean;
  verdict: "PASS" | "FAIL" | "INSUFFICIENT SAMPLE";
  forkSamples: number;
  relayComparisons: number;
  /** Backend-neutral alias of `relayComparisons` (WS-R). */
  independentComparisons?: number;
  actualComparisons: number;
  relayWithinTolerance: number;
  actualWithinTolerance: number;
  relayAccuracyBps: number;
  actualAccuracyBps: number;
  reasons: string[];
}

export interface StatusResponse {
  chain: {id: number; name: string};
  head: {number: number; hash: string; baseFeeWei: string; gasUsed: number; timestamp: number};
  mode: "simulation" | "live";
  /**
   * Boot-time arming: `LIVE_EXECUTION=true` + `I_UNDERSTAND_LIVE_RISK=yes`
   * at process start. When false the runtime mode switch is refused by the
   * bot (409 with instructions); the dashboard shows the arming steps
   * instead of a toggle it cannot honour.
   */
  liveArmed?: boolean;
  broadcastEnabled?: boolean;
  qualification?: {
    pass: boolean;
    startedAtMs: number;
    elapsedHours: number;
    requiredHours: number;
    liveCandidateSimulations: number;
    relayCrossChecks: number;
    highConfidenceActualMatches: number;
    observationCount: number;
    maximumObservationGapSecs: number;
    allowedObservationGapSecs: number;
    minimumSamples: number;
    minimumRelayComparisons: number;
    minimumActualMatches: number;
    maximumErrorBps: number;
    minimumAccuracyBps: number;
    persistenceDropped: number;
    /** `relay` (fork vs `eth_callBundle`, mainnet) or `sequencer` (fork vs
     *  an independent canonical state comparison, Base/L2). Actual route
     *  matches remain a separate evidence population. */
    comparisonBackend?: string;
    reasons: string[];
    strategies: StrategyQualification[];
  };
  strategies: Strategy[];
  /** Boot-time set (env toggles); `strategies` is the runtime-effective set. */
  bootStrategies?: Strategy[];
  risk: {
    minNetProfitWei: string;
    maxPositionWei: string;
    maxBaseFeeWei: string;
    bribeBps: number;
    killSwitchTripped: boolean;
    cumulativeNetWei: string;
    maxGasPerBundle?: number;
    maxInflightPerStrategy?: number;
    maxDrawdownWei?: string;
  };
  executor: string;
  pools: number;
  /** UniswapV3 pools in the disjoint V3 cache. Absent on older bots. */
  poolsV3?: number;
  stats: {
    pendingSeen: number;
    hintsSeen: number;
    blocksSeen: number;
    /** Delivered blocks ingested from the bloXroute Max Profit relay. */
    relayBlocksSeen?: number;
    /** Transactions inside those blocks, scored for extractable value. */
    relayTxsSeen?: number;
    opportunities: number;
    simulations: number;
    submittable: number;
    rejected: number;
    reorgsSeen?: number;
    startedAtMs: number;
    /**
     * Per-strategy funnel counters. The keys match `Strategy`.
     *
     * Two units live in here and must not be divided into each other:
     * `invocationsWithOutput` / `invocationsEmpty` count *strategy calls*,
     * everything else counts *individual opportunities*. See
     * `FunnelCounters`.
     */
    funnel?: Partial<Record<Strategy, FunnelCounters>>;
    /**
     * The same funnel for transactions that were already mined when the bot
     * scored them — the bloXroute delivered-block backfill. Kept apart from
     * `funnel` so a ~150-tx-per-block post-mortem stream cannot drown out the
     * live signal.
     */
    funnelReplay?: Partial<Record<Strategy, FunnelCounters>>;
  };
  simBackends: {anvilFork: boolean; relayCallBundle: boolean};
  inventory?: {
    nonce: number;
    ethWei: string;
    wethWei: string;
    availableWei: string;
    gate: boolean;
  };
  /** Bounded pre-qualification live shots. Absent on older bots. */
  liveSmoke?: {
    max: number;
    used: number;
    remaining: number;
    gasAtRiskWei?: string;
    maxGasCostWei?: string;
  };
  latency?: LatencySnapshot;
  demo?: boolean;
}

export interface HistogramSnapshot {
  count: number;
  meanMs: number;
  minMs: number;
  maxMs: number;
  p50Ms: number;
  p95Ms: number;
  p99Ms: number;
}

export interface LatencySnapshot {
  budgetMs: number;
  withinBudget: boolean;
  stages: Record<string, HistogramSnapshot>;
}

export interface ReconciliationRow {
  blockNumber: number;
  opportunityId: string;
  strategy: string;
  simNetWei: string;
  ourBribeWei: string;
  winningBidWei: string;
  victimLanded: boolean;
  wouldOutbid: boolean;
  inclusionP: number;
  truePositive: boolean;
  falsePositive: boolean;
  reorged: boolean;
  createdAtMs: number;
}

export interface CompetitionSummary {
  rows: number;
  truePositives: number;
  falsePositives: number;
  wouldOutbid: number;
  victimsLanded: number;
  meanInclusionP: number;
}

export interface CompetitionResponse {
  summary: CompetitionSummary;
  recent: ReconciliationRow[];
  demo?: boolean;
}

export interface ActualMevResponse {
  summary: {matches: number; highConfidence: number};
  matches: {
    opportunityId: string;
    blockNumber: number;
    victimHash: string;
    mevTxHashes: string[];
    actor: string | null;
    grossWethWei: string;
    gasCostWei: string;
    netWethWei: string;
    confidence: "high" | "medium" | string;
    confidenceScoreBps: number;
    completeness: Record<string, string>;
    evidence: Record<string, unknown>;
    createdAtMs: number;
  }[];
  demo?: boolean;
}

export interface ExecutionResponse {
  finalityDepth: number;
  executions: {
    bundleId: string;
    opportunityId: string;
    strategy: Strategy;
    targetBlock: number;
    state: string;
    included: boolean | null;
    includedBlock: number | null;
    observedTxHashes: string[];
    submittedAtMs: number;
    txHashes: string[];
    grossProfitWei: string | null;
    builderPaymentWei: string | null;
    retainedProfitWei: string | null;
    gasCostWei: string | null;
    netProfitWei: string | null;
    canonical: boolean | null;
    finalizedBlock: number | null;
    reconciledAtMs: number | null;
  }[];
}

export interface ReorgRow {
  fromBlock: number;
  toBlock: number;
  depth: number;
  oldHash: string;
  newHash: string;
  seenAtMs: number;
}

export interface FunnelCounters {
  /** Unit: strategy calls that produced at least one opportunity. */
  invocationsWithOutput: number;
  /** Unit: strategy calls that produced none. */
  invocationsEmpty: number;
  /** Unit: opportunities. Sum of opps.len() over every call. */
  candidatesEmitted: number;
  gatedByRisk: number;
  missingVictimRaw: number;
  simulationsSucceeded: number;
  simulationsReverted: number;
  simulationsFailed: number;
  submittable: number;
}

export interface PnlRow {
  strategy: Strategy;
  simulations: number;
  wins: number;
  losses: number;
  gross_profit_wei: string;
  gas_spent_wei: string;
  net_profit_wei: string;
  best_net_wei: string;
  worst_net_wei: string;
  avg_latency_ms: number;
}

export interface PnlResponse {
  byStrategy: PnlRow[];
  totalNetWei: string;
  demo?: boolean;
}

export interface SimulationRow {
  opportunityId: string;
  strategy: Strategy;
  backend: "anvil_fork" | "relay_call_bundle" | "eth_call";
  success: boolean;
  grossWei: string;
  gasUsed: number;
  gasCostWei: string;
  bribeWei: string;
  netWei: string;
  revertReason: string | null;
  targetBlock: number;
  latencyMs: number;
  createdAtMs: number;
  notes: string;
  /**
   * Comma-separated victim transaction hashes from the parent opportunity.
   * Each simulation links to the transaction it reacted to on the explorer.
   * Empty string when the opportunity row is gone or had no victims.
   */
  victims?: string;
}

/** `/api/mode` — effective + boot-time-armed execution mode. */
export interface ModeResponse {
  mode: "simulation" | "live";
  liveArmed: boolean;
  ok?: boolean;
  error?: string;
  hint?: string;
  demo?: boolean;
}

export interface OpportunityRow {
  id: string;
  strategy: Strategy;
  targetBlock: number;
  profitToken: string;
  expectedWei: string;
  notionalWei: string;
  victims: string;
  notes: string;
  createdAtMs: number;
}

export interface SeriesPoint {
  block: number;
  netWei: string;
  count: number;
}

export interface RelayBid {
  relay: string;
  slot: number;
  builder: string;
  valueWei: string;
  seenAtMs: number;
}

/** A block delivered through a MEV-Boost relay (feed shape, snake_case). */
export interface RelayBlock {
  relay: string;
  slot: number;
  block_number: number;
  block_hash: string;
  builder: string;
  value_wei: string;
  gas_used: number;
  num_tx: number;
}

/** Trimmed transaction summary inside a delivered block (feed shape). */
export interface RelayTxSummary {
  hash: string;
  from: string | null;
  to: string | null;
  value: string;
  selector: string | null;
}

/** A delivered block row from `/api/relay-blocks` (camelCase). */
export interface RelayBlockRow {
  relay: string;
  slot: number;
  blockNumber: number;
  blockHash: string;
  builder: string;
  valueWei: string;
  gasUsed: number;
  numTx: number;
  seenAtMs: number;
}

/** A delivered-block transaction from `/api/relay-txs` (calldata included). */
export interface RelayBlockTxRow {
  blockNumber: number;
  txIndex: number;
  hash: string;
  from: string | null;
  to: string | null;
  valueWei: string;
  nonce: number;
  gas: number;
  selector: string | null;
  input: string;
}

export type FeedEvent =
  | {kind: "block"; number: number; hash: string; base_fee_per_gas: string; gas_used: number; timestamp: number}
  | {
      kind: "pending";
      hash: string;
      from: string | null;
      to: string | null;
      value: string;
      gas: number;
      source: string;
      selector: string | null;
      seen_at_ms: number;
    }
  | {kind: "mev_share_hint"; hash: string; logs: number; functions: string[]; seen_at_ms: number}
  | {kind: "opportunity"; id: string; strategy: Strategy; notes: string; expected_profit_wei: string; target_block: number}
  | {
      kind: "simulation";
      opportunity_id: string;
      strategy: Strategy;
      backend: string;
      success: boolean;
      net_profit_wei: string;
      gas_used: number;
      gross_profit_wei: string;
      revert_reason: string | null;
    }
  | {kind: "bundle"; id: string; strategy: Strategy; target_block: number; submitted: boolean}
  | {kind: "relay"; relay: string; slot: number; builder: string; value_wei: string; seen_at_ms: number}
  | {kind: "relay_block"; block: RelayBlock; tx_count: number; txs: RelayTxSummary[]}
  | {kind: "alert"; rule: string; severity: string; message: string; active: boolean; seen_at_ms: number}
  | {
      kind: "reorg";
      from_block: number;
      to_block: number;
      depth: number;
      old_hash: string;
      new_hash: string;
      seen_at_ms: number;
    };

// ---------------------------------------------------------------------------
// Directional sniper lane
//
// A separate surface from the risk/strategy types above, mirroring the
// separation on the bot side: these describe a lane that holds positions and
// can lose money, not an atomic profit-or-revert bundle.
// ---------------------------------------------------------------------------

export type SniperPositionState = "pending" | "open" | "scaling" | "closed" | "abandoned";

export type SniperExitReason =
  | "take_profit_pct"
  | "take_profit_abs"
  | "stop_loss"
  | "trailing_stop"
  | "max_hold"
  | "honeypot_detected"
  | "manual"
  | "risk_stop";

export interface SniperPortfolioRow {
  id: string;
  token: string;
  pair: string;
  venue: string;
  state: SniperPositionState;
  symbol: string | null;
  /** Wei values are decimal strings: they exceed JS safe integers. */
  entryCostWei: string;
  entryQty: string;
  remainingQty: string;
  realizedWei: string;
  gasSpentWei: string;
  markValueWei: string;
  unrealizedPnlWei: string;
  netPnlWei: string;
  netPnlBps: number;
  markStale: boolean;
  openedBlock: number;
  openedAtMs: number;
  closedAtMs: number | null;
  ageSecs: number;
  exitReason: SniperExitReason | null;
  entryVerdict: string;
  notes: string;
}

export interface SniperPortfolioTotals {
  openPositions: number;
  closedPositions: number;
  openCostWei: string;
  openValueWei: string;
  unrealizedPnlWei: string;
  realizedPnlWei: string;
  totalPnlWei: string;
  gasSpentWei: string;
  deployedTotalWei: string;
  deployedTodayWei: string;
  wins: number;
  losses: number;
  winRateBps: number;
  anyMarkStale: boolean;
}

export interface SniperPortfolio {
  totals: SniperPortfolioTotals;
  open: SniperPortfolioRow[];
  recentClosed: SniperPortfolioRow[];
  armingBlockers: string[];
  armed: boolean;
  generatedAtMs: number;
  demo?: boolean;
}

export interface SniperParams {
  enabled: boolean;
  vaultAddress?: string | null;
  buySizeWei: string;
  minLiquidityWei: string;
  maxPriceImpactBps: number;
  takeProfitBps: number;
  takeProfitAbsWei: string;
  sellFractionBps: number;
  stopLossBps: number;
  trailingStopBps: number;
  maxHoldSecs: number;
  maxConcurrentPositions: number;
  dailyBudgetWei: string;
  totalBudgetWei: string;
  maxDrawdownWei: string;
  requireHoneypotPass: boolean;
  maxBuyTaxBps: number;
  maxSellTaxBps: number;
  minHoldBlocks: number;
  requireLpLocked: boolean;
}

export interface SniperParamsPatch {
  enabled?: boolean;
  vaultAddress?: string;
  buySizeWei?: string;
  minLiquidityWei?: string;
  maxPriceImpactBps?: number;
  takeProfitBps?: number;
  takeProfitAbsWei?: string;
  sellFractionBps?: number;
  stopLossBps?: number;
  trailingStopBps?: number;
  maxHoldSecs?: number;
  maxConcurrentPositions?: number;
  dailyBudgetWei?: string;
  totalBudgetWei?: string;
  maxDrawdownWei?: string;
  requireHoneypotPass?: boolean;
  maxBuyTaxBps?: number;
  maxSellTaxBps?: number;
  minHoldBlocks?: number;
  requireLpLocked?: boolean;
}

export interface SniperParamsResponse {
  params: SniperParams;
  armed: boolean;
  bootEnabled: boolean;
  halted: boolean;
  haltReason: string | null;
  armingBlockers: string[];
  rejections: Record<string, number>;
  envSnippet: string;
  demo?: boolean;
}

export interface SniperVaultStatus {
  configured: boolean;
  address: string | null;
  spendableRemainingWei: string;
  dailyBudgetWei: string;
  totalBudgetWei: string;
  windowResetTimeSecs: number;
  demo?: boolean;
}
