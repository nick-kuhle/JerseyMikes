"use client";

/**
 * Strategy eligibility — which rows may ever broadcast, and why the rest cannot.
 *
 * This answers a different question from the qualification report sitting
 * directly above it, and the distinction is the whole reason this panel
 * exists:
 *
 *   - *Eligibility* is an engineering property of the strategy. It is fixed
 *     at build time by `Strategy::live_candidate()` and never changes at
 *     runtime. A shadow-only row is one whose limits are settlement- or
 *     ordering-shaped — no amount of evidence promotes it.
 *   - *Qualification* is an evidence property of a run. It is earned over a
 *     168-hour window of gap-free observation and can move both ways.
 *
 * Conflating the two costs an operator a week: a shadow-only strategy sits at
 * `PENDING` forever, looking for all the world like a row that just needs more
 * soak time, when in fact no quantity of soak will ever move it. The bot has
 * always known the answer — `GET /api/config` returns a `strategyEligibility`
 * array carrying each row's `shadowOnlyReason` — but until now nothing
 * displayed it, so the reason only existed in the source.
 *
 * Eligibility is necessary, not sufficient. A live candidate still has to
 * clear its own qualification verdict plus every arming, risk and inventory
 * gate before a single bundle leaves the process; this panel says nothing
 * about whether that has happened.
 */

import {memo, useEffect, useState} from "react";
import {readActiveChain, withChain} from "@/lib/chain";
import {STRATEGY_COLOR, STRATEGY_LABEL} from "@/lib/format";
import type {ConfigResponse, StrategyEligibility} from "@/lib/types";

interface Props {
  /**
   * The runtime-enabled strategy set from the polled status, so a row can be
   * shown as eligible-but-switched-off rather than silently implying it is
   * running. Undefined while the first poll is in flight.
   */
  enabled?: string[];
}

function EligibilityPanel({enabled}: Props) {
  const [rows, setRows] = useState<StrategyEligibility[] | null>(null);
  const [demo, setDemo] = useState(false);
  const [failed, setFailed] = useState(false);

  /**
   * Fetched once on mount, not polled: eligibility is boot-fixed, so putting
   * it on the console's 4s tick would re-request an answer that cannot change
   * without a restart. `Console` re-keys its whole panel tree on chain change,
   * so a chain switch remounts this component and re-runs the fetch — no
   * subscription needed, and no window in which one chain's eligibility is
   * rendered under another chain's heading.
   */
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const res = await fetch(withChain("/api/bot/config", readActiveChain()), {cache: "no-store"});
        const data = (await res.json()) as ConfigResponse;
        if (cancelled) return;
        setDemo(Boolean(data.demo));
        // An older bot predates the field entirely. Render the empty state
        // rather than inventing an eligibility claim the bot never made.
        setRows(Array.isArray(data.strategyEligibility) ? data.strategyEligibility : []);
        setFailed(false);
      } catch {
        if (!cancelled) setFailed(true);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const live = (rows ?? []).filter((r) => r.liveCandidate);
  const shadow = (rows ?? []).filter((r) => !r.liveCandidate);

  return (
    <div className="panel" style={{padding: 10}}>
      <div className="panel-head">
        <span>
          strategy eligibility
          <span
            className="muted"
            style={{marginLeft: 8, fontSize: 10, textTransform: "none", letterSpacing: 0}}
            title="Fixed at build time by Strategy::live_candidate() — unlike qualification, soak time cannot change it."
          >
            build-time · not earned by soak
          </span>
        </span>
        <span className="muted">
          {rows === null
            ? failed
              ? "unavailable"
              : "loading"
            : `${live.length} live-eligible · ${shadow.length} shadow-only${demo ? " · demo" : ""}`}
        </span>
      </div>

      <table className="grid">
        <thead>
          <tr>
            <th>strategy</th>
            <th>eligibility</th>
            <th>runtime</th>
            <th style={{whiteSpace: "normal"}}>why</th>
          </tr>
        </thead>
        <tbody>
          {[...live, ...shadow].map((row) => {
            const on = enabled?.includes(row.name);
            return (
              <tr key={row.name}>
                <td>
                  <span
                    className="dot"
                    style={{background: STRATEGY_COLOR[row.name] ?? "#444", marginRight: 6}}
                  />
                  {STRATEGY_LABEL[row.name] ?? row.name}
                </td>
                <td className={row.liveCandidate ? "pos" : "muted"}>
                  {row.liveCandidate ? "LIVE-ELIGIBLE" : "SHADOW-ONLY"}
                </td>
                <td className={on ? "pos" : "muted"} title={
                  enabled === undefined
                    ? "waiting for the first status poll"
                    : on
                      ? "constructed at boot and enabled at runtime"
                      : "not in the runtime-enabled set"
                }>
                  {enabled === undefined ? "—" : on ? "on" : "off"}
                </td>
                <td className="muted" style={{whiteSpace: "normal", maxWidth: 420, lineHeight: 1.45}}>
                  {row.liveCandidate
                    ? "Back-running or settlement-atomic — may broadcast once its qualification verdict passes."
                    : (row.shadowOnlyReason ?? "shadow-only")}
                </td>
              </tr>
            );
          })}
          {rows !== null && rows.length === 0 && (
            <tr>
              <td colSpan={4} className="muted" style={{textAlign: "center", padding: 10}}>
                this bot does not report strategy eligibility
              </td>
            </tr>
          )}
          {rows === null && (
            <tr>
              <td colSpan={4} className="muted" style={{textAlign: "center", padding: 10}}>
                {failed ? "could not reach the bot" : "loading eligibility…"}
              </td>
            </tr>
          )}
        </tbody>
      </table>

      <div className="muted" style={{fontSize: 10, marginTop: 6, lineHeight: 1.5}}>
        • Eligible ≠ approved. A live-eligible row still needs its own <code>PASS</code> verdict above,
        plus boot arming, <code>BROADCAST_ENABLED</code>, an authenticated runtime mode and every risk
        and inventory gate, before it can broadcast.
        <br />• A shadow-only row will never reach <code>PASS</code>. It keeps simulating, and its P/L is
        recorded, but the submission path is closed to it by construction. See <code>docs/STRATEGIES.md</code>.
      </div>
    </div>
  );
}

export default memo(EligibilityPanel);
