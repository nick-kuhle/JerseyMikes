"use client";

/**
 * Per-strategy funnel display.
 *
 * Renders a small table per strategy, plus a single-row "where did my
 * opportunities die?" summary so the user can read off the answer at a
 * glance. The funnel is the right answer to the question "why aren't I
 * seeing any opportunities?" — the numbers are pulled straight from
 * `/api/funnel` and `/api/status.stats.funnel`.
 */

import {useEffect, useState} from "react";
import type {FunnelCounters, Strategy} from "@/lib/types";
import {STRATEGY_COLOR, STRATEGY_LABEL} from "@/lib/format";

interface Props {
  /**
   * The funnel as it appears in `StatusResponse.stats.funnel`. Optional
   * because the demo data may not always be loaded, and the live bot
   * populates this lazily (only after the first record_funnel call).
   */
  funnel: Partial<Record<Strategy, FunnelCounters>> | null | undefined;
}

const ALL_STRATEGIES: Strategy[] = [
  "sandwich",
  "jit",
  "atomic_arb",
  "liquidation",
  "sniper",
];

const ZERO: FunnelCounters = {
  candidatesEmitted: 0,
  candidatesSkipped: 0,
  gatedByRisk: 0,
  missingVictimRaw: 0,
  simulationsSucceeded: 0,
  simulationsReverted: 0,
  simulationsFailed: 0,
  submittable: 0,
};

function row(f: FunnelCounters): FunnelCounters {
  return f ?? ZERO;
}

/**
 * Format a number with thousands separators; small numbers stay plain.
 */
function fmt(n: number): string {
  if (n < 1000) return String(n);
  return n.toLocaleString("en-US");
}

export default function FunnelPanel({funnel}: Props) {
  // Auto-refresh every 5s. The funnel counter is monotonically
  // increasing, so the dashboard just shows the latest values; the user
  // can spot deltas visually.
  const [tick, setTick] = useState(0);
  useEffect(() => {
    const t = setInterval(() => setTick((x) => x + 1), 5_000);
    return () => clearInterval(t);
  }, []);
  // Touch `tick` so React registers the dependency.
  void tick;

  if (!funnel) {
    return (
      <div className="panel" style={{padding: 14, color: "var(--muted)", fontSize: 12}}>
        Funnel data not yet populated. The bot emits the first counters on its
        first strategy tick.
      </div>
    );
  }

  // Per-strategy table.
  const rows = ALL_STRATEGIES.map((s) => {
    const f = row(funnel[s] ?? ZERO);
    const seen = f.candidatesEmitted + f.candidatesSkipped;
    const simulated = f.simulationsSucceeded + f.simulationsReverted;
    const revertRate = simulated > 0 ? f.simulationsReverted / simulated : 0;
    return {s, f, seen, simulated, revertRate};
  });

  // Aggregate summary.
  const total = rows.reduce(
    (acc, r) => {
      acc.candidatesSkipped += r.f.candidatesSkipped;
      acc.candidatesEmitted += r.f.candidatesEmitted;
      acc.gatedByRisk += r.f.gatedByRisk;
      acc.missingVictimRaw += r.f.missingVictimRaw;
      acc.simulationsSucceeded += r.f.simulationsSucceeded;
      acc.simulationsReverted += r.f.simulationsReverted;
      acc.submittable += r.f.submittable;
      return acc;
    },
    {
      candidatesSkipped: 0,
      candidatesEmitted: 0,
      gatedByRisk: 0,
      missingVictimRaw: 0,
      simulationsSucceeded: 0,
      simulationsReverted: 0,
      submittable: 0,
    } as Omit<FunnelCounters, "simulationsFailed">,
  );

  return (
    <div className="panel" style={{padding: 14, display: "grid", gap: 14}}>
      <div style={{display: "flex", justifyContent: "space-between", alignItems: "baseline"}}>
        <span style={{fontSize: 13, fontWeight: "bold", color: "var(--amber)"}}>
          🎯 Strategy Funnel — Where Do Opportunities Die?
        </span>
        <span className="muted" style={{fontSize: 10}}>
          Live counters from <code>/api/funnel</code>. Refreshes every 5s.
        </span>
      </div>

      <p className="muted" style={{fontSize: 12, lineHeight: 1.5, margin: 0}}>
        If the simulation tape is empty, this is the first place to look.
        <code> candidatesSkipped </code> means the strategy saw a transaction or block
        but the pre-filter rejected it (often: too small, victim-revert trap, or no
        matching pool cached). <code> gatedByRisk </code> means the opportunity was
        built but the risk engine said no (position too big, base fee too high,
        inflight cap, or kill-switch tripped). <code> simulationsReverted </code>
        means the fork simulation ran and lost money — that is the strategy
        correctly filtering out non-profitable flow, not a bug.
      </p>

      {/* Aggregate summary card */}
      <div
        style={{
          display: "grid",
          gridTemplateColumns: "repeat(auto-fit, minmax(120px, 1fr))",
          gap: 8,
        }}
      >
        <SummaryStat label="Saw but skipped" value={fmt(total.candidatesSkipped)} color="var(--muted)" />
        <SummaryStat label="Built (candidates)" value={fmt(total.candidatesEmitted)} color="var(--cyan)" />
        <SummaryStat label="Gated by risk" value={fmt(total.gatedByRisk)} color="var(--red)" />
        <SummaryStat label="Missing victim raw" value={fmt(total.missingVictimRaw)} color="var(--amber)" />
        <SummaryStat label="Sims succeeded" value={fmt(total.simulationsSucceeded)} color="var(--green)" />
        <SummaryStat label="Sims reverted" value={fmt(total.simulationsReverted)} color="var(--muted)" />
        <SummaryStat label="Submittable" value={fmt(total.submittable)} color="var(--green)" highlight />
      </div>

      {/* Per-strategy breakdown */}
      <div style={{display: "grid", gap: 6}}>
        <div
          style={{
            display: "grid",
            gridTemplateColumns: "1.5fr repeat(8, 1fr)",
            gap: 4,
            fontSize: 10,
            color: "var(--muted)",
            textTransform: "uppercase",
            padding: "0 4px",
          }}
        >
          <span>Strategy</span>
          <span title="Total times the strategy was called (on_pending + on_block)">Saw</span>
          <span title="Strategy emitted at least one Opportunity">Built</span>
          <span title="Strategy saw something but emitted zero">Skipped</span>
          <span title="Opportunity rejected by RiskEngine::check">Risk ✗</span>
          <span title="Raw signed victim tx unavailable">No raw</span>
          <span title="Sims that completed with success=true">Sim ✓</span>
          <span title="Sims that completed with success=false">Sim ✗</span>
          <span title="Cleared the net-profit and gas gates">Sub.</span>
        </div>
        {rows.map((r) => (
          <div
            key={r.s}
            style={{
              display: "grid",
              gridTemplateColumns: "1.5fr repeat(8, 1fr)",
              gap: 4,
              fontSize: 12,
              alignItems: "center",
              padding: "4px 4px",
              background: "var(--panel-2)",
              borderRadius: 3,
              borderLeft: `3px solid ${STRATEGY_COLOR[r.s]}`,
            }}
          >
            <span style={{color: STRATEGY_COLOR[r.s], fontWeight: 600}}>
              {STRATEGY_LABEL[r.s] || r.s}
            </span>
            <span style={{color: "var(--muted)"}}>{fmt(r.seen)}</span>
            <span style={{color: r.f.candidatesEmitted > 0 ? "var(--cyan)" : "var(--muted)"}}>
              {fmt(r.f.candidatesEmitted)}
            </span>
            <span style={{color: r.f.candidatesSkipped > 0 ? "var(--muted)" : "var(--muted)"}}>
              {fmt(r.f.candidatesSkipped)}
            </span>
            <span style={{color: r.f.gatedByRisk > 0 ? "var(--red)" : "var(--muted)"}}>
              {fmt(r.f.gatedByRisk)}
            </span>
            <span style={{color: r.f.missingVictimRaw > 0 ? "var(--amber)" : "var(--muted)"}}>
              {fmt(r.f.missingVictimRaw)}
            </span>
            <span style={{color: r.f.simulationsSucceeded > 0 ? "var(--green)" : "var(--muted)"}}>
              {fmt(r.f.simulationsSucceeded)}
            </span>
            <span style={{color: "var(--muted)"}}>{fmt(r.f.simulationsReverted)}</span>
            <span
              style={{
                color: r.f.submittable > 0 ? "var(--green)" : "var(--muted)",
                fontWeight: r.f.submittable > 0 ? "bold" : "normal",
              }}
            >
              {fmt(r.f.submittable)}
            </span>
          </div>
        ))}
      </div>

      <p className="muted" style={{fontSize: 11, lineHeight: 1.5, margin: 0}}>
        <strong>How to read this.</strong> The end-to-end funnel is
        <code> saw → built → risk → sim → submittable</code>. A healthy bot has a
        non-zero number in <code>built</code> and a reasonable fraction of{" "}
        <code>sim ✓</code> (most sims should either succeed or revert cleanly;
        <code> sim ✗ </code>dominates mean the strategy is producing
        opportunities that lose money in the fork, which means the pre-filter
        sizing is wrong). A bot with all zeros in <code>built</code> is
        pre-filtering too aggressively (raise <code>MIN_NET_PROFIT_WEI</code>{" "}
        check, or check the pool cache for missing entries). A bot with
        non-zero <code>built</code> but all zeros in <code>sim ✓</code> is
        either risk-gating too tightly or hitting simulator failures.
      </p>
    </div>
  );
}

function SummaryStat({
  label,
  value,
  color,
  highlight,
}: {
  label: string;
  value: string;
  color: string;
  highlight?: boolean;
}) {
  return (
    <div
      style={{
        background: highlight ? "rgba(34, 197, 94, 0.08)" : "var(--panel-2)",
        border: `1px solid ${highlight ? "var(--green)" : "var(--line)"}`,
        borderRadius: 4,
        padding: "6px 10px",
      }}
    >
      <div style={{fontSize: 10, color: "var(--muted)", textTransform: "uppercase"}}>
        {label}
      </div>
      <div style={{fontSize: 16, fontWeight: "bold", color}}>{value}</div>
    </div>
  );
}
