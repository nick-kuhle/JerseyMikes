"use client";

/**
 * Sniper — mini portfolio.
 *
 * The console's other panels all describe *atomic* work: a bundle either
 * cleared its profit guard or it did not, and nothing is held between blocks.
 * This panel describes the one lane that holds inventory, so it is built
 * around a different question: **what are we holding, what is it worth, and
 * how much of that is real?**
 *
 * Three deliberate choices:
 *
 * 1. **Realised and unrealised are never merged.** They sit in separate cells
 *    with separate colours. A "+2.4 ETH" headline that is entirely paper gain
 *    on an illiquid launch is the most dangerous number this console could
 *    render, so the split is structural, not a tooltip.
 * 2. **A stale mark is shown as stale.** If the pool read failed, the row is
 *    dimmed and flagged rather than quietly showing the last good number.
 * 3. **The arming state is always visible.** When the lane cannot buy, the
 *    panel leads with the exact reasons, copied verbatim from the bot.
 */

import {memo, useCallback, useEffect, useState} from "react";
import {readActiveChain, withChain} from "@/lib/chain";
import {ago, shortHash, signedEth, weiToEth} from "@/lib/format";
import type {
  SniperParamsResponse,
  SniperPortfolio,
  SniperPortfolioRow,
} from "@/lib/types";

const EXIT_LABEL: Record<string, string> = {
  take_profit_pct: "take profit %",
  take_profit_abs: "take profit ETH",
  stop_loss: "stop loss",
  trailing_stop: "trailing stop",
  max_hold: "max hold",
  honeypot_detected: "honeypot",
  manual: "manual",
  risk_stop: "risk stop",
};

const STATE_COLOR: Record<string, string> = {
  pending: "#eab308",
  open: "#22c55e",
  scaling: "#38bdf8",
  closed: "var(--muted)",
  abandoned: "var(--muted)",
};

function pnlColor(wei: string): string {
  let v: bigint;
  try {
    v = BigInt(wei);
  } catch {
    return "var(--muted)";
  }
  if (v > 0n) return "#22c55e";
  if (v < 0n) return "#ef4444";
  return "var(--muted)";
}

function bps(n: number): string {
  const pct = n / 100;
  return `${pct >= 0 ? "+" : ""}${pct.toFixed(1)}%`;
}

function Stat({
  label,
  value,
  color,
  title,
}: {
  label: string;
  value: string;
  color?: string;
  title?: string;
}) {
  return (
    <div style={{minWidth: 108}} title={title}>
      <div className="muted" style={{fontSize: 10, textTransform: "uppercase", letterSpacing: 0.4}}>
        {label}
      </div>
      <div style={{fontSize: 15, fontVariantNumeric: "tabular-nums", color: color ?? "inherit"}}>
        {value}
      </div>
    </div>
  );
}

function PositionRow({r}: {r: SniperPortfolioRow}) {
  const closed = r.state === "closed" || r.state === "abandoned";
  return (
    <tr style={{opacity: r.markStale ? 0.55 : 1}}>
      <td>
        <span
          style={{
            display: "inline-block",
            width: 7,
            height: 7,
            borderRadius: "50%",
            background: STATE_COLOR[r.state] ?? "var(--muted)",
            marginRight: 7,
          }}
        />
        <strong>{r.symbol ?? shortHash(r.token, 4)}</strong>
        <span className="muted" style={{marginLeft: 6, fontSize: 11}}>
          {r.state}
        </span>
        {r.markStale && (
          <span
            style={{marginLeft: 6, fontSize: 10, color: "#eab308"}}
            title="the pool could not be re-read; this mark is not current"
          >
            STALE
          </span>
        )}
      </td>
      <td style={{fontVariantNumeric: "tabular-nums"}}>{weiToEth(r.entryCostWei, 4)}</td>
      <td style={{fontVariantNumeric: "tabular-nums"}}>
        {closed ? <span className="muted">—</span> : weiToEth(r.markValueWei, 4)}
      </td>
      <td style={{fontVariantNumeric: "tabular-nums", color: pnlColor(r.realizedWei)}}>
        {BigInt(r.realizedWei || "0") > 0n ? weiToEth(r.realizedWei, 4) : "—"}
      </td>
      <td
        style={{fontVariantNumeric: "tabular-nums", color: pnlColor(r.netPnlWei)}}
        title={`${r.netPnlWei} wei net of gas`}
      >
        {signedEth(r.netPnlWei, 4)}
        <span className="muted" style={{marginLeft: 6, fontSize: 11}}>
          {bps(r.netPnlBps)}
        </span>
      </td>
      <td className="muted" style={{fontSize: 11}}>
        {r.exitReason ? EXIT_LABEL[r.exitReason] ?? r.exitReason : ago(r.openedAtMs)}
      </td>
    </tr>
  );
}

function SniperPanel() {
  const [pf, setPf] = useState<SniperPortfolio | null>(null);
  const [cfg, setCfg] = useState<SniperParamsResponse | null>(null);
  const [demo, setDemo] = useState(false);
  const [tab, setTab] = useState<"open" | "closed" | "gates">("open");

  const load = useCallback(async () => {
    const chain = readActiveChain();
    try {
      const [pRes, cRes] = await Promise.all([
        fetch(withChain("/api/bot/sniper/portfolio", chain), {cache: "no-store"}),
        fetch(withChain("/api/bot/sniper/params", chain), {cache: "no-store"}),
      ]);
      const p = (await pRes.json()) as SniperPortfolio & {demo?: boolean};
      const c = (await cRes.json()) as SniperParamsResponse & {demo?: boolean};
      setPf(p);
      setCfg(c);
      setDemo(Boolean(p.demo || c.demo));
    } catch {
      /* leave the previous snapshot rather than blanking the panel */
    }
  }, []);

  useEffect(() => {
    load();
    const t = setInterval(load, 5000);
    return () => clearInterval(t);
  }, [load]);

  if (!pf) {
    return <div className="muted">loading sniper portfolio…</div>;
  }

  const t = pf.totals;
  const blockers = pf.armingBlockers ?? [];
  const hardBlockers = blockers.filter((b) => !b.startsWith("WARNING"));
  const warnings = blockers.filter((b) => b.startsWith("WARNING"));

  return (
    <div>
      {/* Arming banner: the first thing an operator must see. */}
      <div
        style={{
          border: `1px solid ${pf.armed ? "#22c55e" : "var(--border)"}`,
          background: pf.armed ? "rgba(34,197,94,0.07)" : "transparent",
          borderRadius: 6,
          padding: "8px 10px",
          marginBottom: 12,
          fontSize: 12,
        }}
      >
        <div style={{display: "flex", alignItems: "center", gap: 8}}>
          <span
            style={{
              fontSize: 10,
              fontWeight: 700,
              letterSpacing: 0.6,
              padding: "2px 7px",
              borderRadius: 4,
              background: pf.armed ? "#22c55e" : "var(--border)",
              color: pf.armed ? "#05240f" : "var(--muted)",
            }}
          >
            {pf.armed ? "ARMED — CAN BUY" : "SHADOW — CANNOT BUY"}
          </span>
          {demo && (
            <span style={{fontSize: 10, color: "#eab308", letterSpacing: 0.5}}>DEMO DATA</span>
          )}
          {cfg?.halted && (
            <span style={{fontSize: 11, color: "#ef4444"}}>halted: {cfg.haltReason}</span>
          )}
        </div>
        {hardBlockers.length > 0 && (
          <ul style={{margin: "7px 0 0", paddingLeft: 18, color: "var(--muted)"}}>
            {hardBlockers.map((b) => (
              <li key={b}>{b}</li>
            ))}
          </ul>
        )}
        {warnings.map((w) => (
          <div key={w} style={{marginTop: 6, color: "#eab308"}}>
            {w}
          </div>
        ))}
        {pf.armed && (
          <div style={{marginTop: 6, color: "var(--muted)"}}>
            This lane is <strong>not</strong> covered by the executor&apos;s profit-or-revert
            guard. Each entry can lose its full size; the budget is the only bound.
          </div>
        )}
      </div>

      {/* Totals. Realised and unrealised deliberately never merged. */}
      <div
        style={{
          display: "flex",
          gap: 20,
          flexWrap: "wrap",
          padding: "10px 0 14px",
          borderBottom: "1px solid var(--border)",
          marginBottom: 12,
        }}
      >
        <Stat label="Open" value={`${t.openPositions}`} />
        <Stat
          label="Held (cost)"
          value={`${weiToEth(t.openCostWei, 4)} Ξ`}
          title="entry cost of everything still held"
        />
        <Stat
          label="Held (mark)"
          value={`${weiToEth(t.openValueWei, 4)} Ξ`}
          color={t.anyMarkStale ? "#eab308" : undefined}
          title={t.anyMarkStale ? "at least one mark is stale" : "current mark-to-market"}
        />
        <Stat
          label="Unrealised"
          value={`${signedEth(t.unrealizedPnlWei, 4)} Ξ`}
          color={pnlColor(t.unrealizedPnlWei)}
          title="paper only — not booked, and not necessarily exitable at this price"
        />
        <Stat
          label="Realised"
          value={`${signedEth(t.realizedPnlWei, 4)} Ξ`}
          color={pnlColor(t.realizedPnlWei)}
          title="actually booked, net of gas"
        />
        <Stat
          label="Win rate"
          value={t.wins + t.losses === 0 ? "—" : `${(t.winRateBps / 100).toFixed(0)}%`}
          title={`${t.wins}W / ${t.losses}L on closed positions`}
        />
        <Stat
          label="Deployed 24h"
          value={`${weiToEth(t.deployedTodayWei, 4)} Ξ`}
          title="entry capital committed in the rolling 24h budget window"
        />
        <Stat
          label="Gas"
          value={`${weiToEth(t.gasSpentWei, 4)} Ξ`}
          title="gas spent on entries and exits"
        />
      </div>

      <div style={{display: "flex", gap: 6, marginBottom: 8}}>
        {(["open", "closed", "gates"] as const).map((k) => (
          <button
            key={k}
            onClick={() => setTab(k)}
            className={tab === k ? "tab active" : "tab"}
            style={{
              fontSize: 11,
              padding: "3px 10px",
              borderRadius: 4,
              border: "1px solid var(--border)",
              background: tab === k ? "var(--border)" : "transparent",
              color: "inherit",
              cursor: "pointer",
            }}
          >
            {k === "open"
              ? `open (${pf.open.length})`
              : k === "closed"
                ? `closed (${pf.recentClosed.length})`
                : "gates"}
          </button>
        ))}
      </div>

      {tab !== "gates" && (
        <table style={{width: "100%", fontSize: 12}}>
          <thead>
            <tr className="muted" style={{textAlign: "left", fontSize: 10}}>
              <th>TOKEN</th>
              <th>ENTRY Ξ</th>
              <th>MARK Ξ</th>
              <th>REALISED Ξ</th>
              <th>NET PNL</th>
              <th>{tab === "open" ? "AGE" : "EXIT"}</th>
            </tr>
          </thead>
          <tbody>
            {(tab === "open" ? pf.open : pf.recentClosed).map((r) => (
              <PositionRow key={r.id} r={r} />
            ))}
          </tbody>
        </table>
      )}

      {tab !== "gates" && (tab === "open" ? pf.open : pf.recentClosed).length === 0 && (
        <div className="muted" style={{padding: "14px 0", fontSize: 12}}>
          {tab === "open"
            ? "no open positions — the lane is either disarmed or has not admitted a launch yet"
            : "no closed positions yet"}
        </div>
      )}

      {tab === "gates" && cfg && (
        <div style={{fontSize: 12}}>
          <div className="muted" style={{marginBottom: 8}}>
            Why launches were turned down. Every rejection is counted by reason, so a lane
            that never buys can be diagnosed instead of guessed at.
          </div>
          <table style={{width: "100%", fontSize: 12}}>
            <tbody>
              {Object.entries(cfg.rejections ?? {})
                .sort((a, b) => b[1] - a[1])
                .map(([code, n]) => (
                  <tr key={code}>
                    <td style={{color: "var(--muted)"}}>{code.replace(/_/g, " ")}</td>
                    <td style={{textAlign: "right", fontVariantNumeric: "tabular-nums"}}>{n}</td>
                  </tr>
                ))}
              {Object.keys(cfg.rejections ?? {}).length === 0 && (
                <tr>
                  <td className="muted">no launches evaluated yet</td>
                </tr>
              )}
            </tbody>
          </table>

          <div className="muted" style={{marginTop: 14, marginBottom: 6}}>
            Active envelope
          </div>
          <table style={{width: "100%", fontSize: 12}}>
            <tbody>
              {[
                ["buy size", `${weiToEth(cfg.params.buySizeWei, 4)} Ξ`],
                ["take profit", `${(cfg.params.takeProfitBps / 100).toFixed(0)}%`],
                [
                  "take profit (abs)",
                  cfg.params.takeProfitAbsWei === "0"
                    ? "off"
                    : `${weiToEth(cfg.params.takeProfitAbsWei, 4)} Ξ`,
                ],
                ["sell fraction", `${(cfg.params.sellFractionBps / 100).toFixed(0)}%`],
                [
                  "stop loss",
                  cfg.params.stopLossBps === 0 ? "off" : `${(cfg.params.stopLossBps / 100).toFixed(0)}%`,
                ],
                [
                  "trailing stop",
                  cfg.params.trailingStopBps === 0
                    ? "off"
                    : `${(cfg.params.trailingStopBps / 100).toFixed(0)}%`,
                ],
                [
                  "max hold",
                  cfg.params.maxHoldSecs === 0 ? "off" : `${Math.round(cfg.params.maxHoldSecs / 60)} min`,
                ],
                ["max positions", `${cfg.params.maxConcurrentPositions}`],
                ["daily budget", `${weiToEth(cfg.params.dailyBudgetWei, 4)} Ξ`],
                [
                  "lifetime budget",
                  cfg.params.totalBudgetWei === "0"
                    ? "unlimited"
                    : `${weiToEth(cfg.params.totalBudgetWei, 4)} Ξ`,
                ],
                ["min liquidity", `${weiToEth(cfg.params.minLiquidityWei, 2)} Ξ`],
                ["max price impact", `${(cfg.params.maxPriceImpactBps / 100).toFixed(2)}%`],
                ["honeypot check", cfg.params.requireHoneypotPass ? "required" : "OFF"],
                [
                  "max tax (buy/sell)",
                  `${(cfg.params.maxBuyTaxBps / 100).toFixed(1)}% / ${(cfg.params.maxSellTaxBps / 100).toFixed(1)}%`,
                ],
                ["min hold", `${cfg.params.minHoldBlocks} blocks`],
                ["LP lock required", cfg.params.requireLpLocked ? "yes" : "no"],
              ].map(([k, v]) => (
                <tr key={k}>
                  <td style={{color: "var(--muted)"}}>{k}</td>
                  <td
                    style={{
                      textAlign: "right",
                      fontVariantNumeric: "tabular-nums",
                      color: v === "OFF" ? "#eab308" : "inherit",
                    }}
                  >
                    {v}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          <div className="muted" style={{marginTop: 10, fontSize: 11}}>
            Edit these with <code>POST /api/sniper/params</code>, or persist them as boot
            defaults with the <code>SNIPER_*</code> block in <code>.env</code>. A lane booted
            with <code>SNIPER_DIRECTIONAL=false</code> cannot be armed at runtime.
          </div>
        </div>
      )}
    </div>
  );
}

export default memo(SniperPanel);
