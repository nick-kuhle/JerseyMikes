export type Strategy = "sandwich" | "jit" | "atomic_arb" | "liquidation" | "sniper";

export interface StatusResponse {
  chain: {id: number; name: string};
  head: {number: number; hash: string; baseFeeWei: string; gasUsed: number; timestamp: number};
  mode: "simulation" | "live";
  strategies: Strategy[];
  risk: {
    minNetProfitWei: string;
    maxPositionWei: string;
    maxBaseFeeWei: string;
    bribeBps: number;
    killSwitchTripped: boolean;
    cumulativeNetWei: string;
  };
  executor: string;
  pools: number;
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
  demo?: boolean;
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
  net_profit_wei: number;
  best_net_wei: number;
  worst_net_wei: number;
  avg_latency_ms: number;
}

export interface PnlResponse {
  byStrategy: PnlRow[];
  totalNetWei: number;
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
  netWei: number;
  revertReason: string | null;
  targetBlock: number;
  latencyMs: number;
  createdAtMs: number;
  notes: string;
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
  netWei: number;
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
      net_profit_wei: number;
      gas_used: number;
      gross_profit_wei: string;
      revert_reason: string | null;
    }
  | {kind: "bundle"; id: string; strategy: Strategy; target_block: number; submitted: boolean}
  | {kind: "relay"; relay: string; slot: number; builder: string; value_wei: string; seen_at_ms: number}
  | {kind: "relay_block"; block: RelayBlock; tx_count: number; txs: RelayTxSummary[]};
