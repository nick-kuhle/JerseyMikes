"use client";

import {useEffect, useRef} from "react";
import type {FeedEvent} from "@/lib/types";
import {clock, shortHash, STRATEGY_COLOR, weiToEth} from "@/lib/format";

/**
 * The live tape: every mempool transaction, MEV-Share hint, block, opportunity
 * and simulation the bot sees, newest first.
 */
export default function LiveFeed({events, filter}: {events: FeedEvent[]; filter: string}) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (ref.current) ref.current.scrollTop = 0;
  }, [events.length]);

  const shown = events.filter((e) => filter === "all" || e.kind === filter);

  return (
    <div ref={ref} style={{maxHeight: 420, overflowY: "auto"}}>
      <table className="grid">
        <thead>
          <tr>
            <th style={{width: 70}}>time</th>
            <th style={{width: 110}}>kind</th>
            <th>detail</th>
          </tr>
        </thead>
        <tbody>
          {shown.length === 0 && (
            <tr>
              <td colSpan={3} className="muted" style={{padding: 16, textAlign: "center"}}>
                waiting for events…
              </td>
            </tr>
          )}
          {shown.map((e, i) => (
            <tr key={i}>
              <td className="muted">{clock(eventTime(e))}</td>
              <td>
                <span className="badge" style={{color: kindColor(e)}}>
                  {label(e)}
                </span>
              </td>
              <td style={{whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis", maxWidth: 620}}>
                {detail(e)}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function eventTime(e: FeedEvent): number {
  if ("seen_at_ms" in e && e.seen_at_ms) return e.seen_at_ms;
  if (e.kind === "block") return e.timestamp * 1000;
  return Date.now();
}

function label(e: FeedEvent): string {
  return e.kind === "mev_share_hint" ? "mev-share" : e.kind;
}

function kindColor(e: FeedEvent): string {
  switch (e.kind) {
    case "block":
      return "#22d3ee";
    case "pending":
      return "#6b7c93";
    case "mev_share_hint":
      return "#a855f7";
    case "opportunity":
      return STRATEGY_COLOR[e.strategy] ?? "#f5b544";
    case "simulation":
      return e.net_profit_wei > 0 ? "#35d07f" : "#ff5c5c";
    case "bundle":
      return "#f5b544";
    case "relay":
      return "#4f8bff";
  }
}

function detail(e: FeedEvent) {
  switch (e.kind) {
    case "block":
      return (
        <span>
          #{e.number} · base fee {(Number(e.base_fee_per_gas) / 1e9).toFixed(2)} gwei · gas{" "}
          {(e.gas_used / 1e6).toFixed(2)}M · {shortHash(e.hash)}
        </span>
      );
    case "pending":
      return (
        <span>
          {shortHash(e.hash)} → {shortHash(e.to)} · {weiToEth(e.value, 4)} ETH · {e.selector ?? "—"} ·{" "}
          <span className="muted">{e.source}</span>
        </span>
      );
    case "mev_share_hint":
      return (
        <span>
          {shortHash(e.hash)} · {e.logs} logs · fns [{e.functions.join(", ") || "redacted"}]
        </span>
      );
    case "opportunity":
      return (
        <span>
          <b style={{color: STRATEGY_COLOR[e.strategy]}}>{e.strategy}</b> · expect {weiToEth(e.expected_profit_wei, 6)}{" "}
          ETH · block {e.target_block} · <span className="muted">{e.notes}</span>
        </span>
      );
    case "simulation":
      return (
        <span>
          <b style={{color: STRATEGY_COLOR[e.strategy]}}>{e.strategy}</b> · {e.backend} ·{" "}
          <span className={e.net_profit_wei > 0 ? "pos" : "neg"}>
            {e.net_profit_wei > 0 ? "+" : ""}
            {(e.net_profit_wei / 1e18).toFixed(6)} ETH
          </span>{" "}
          · gas {e.gas_used.toLocaleString()} {e.revert_reason ? `· ${e.revert_reason}` : ""}
        </span>
      );
    case "bundle":
      return (
        <span>
          {e.strategy} bundle for block {e.target_block} ·{" "}
          {e.submitted ? <span className="pos">submitted</span> : <span className="muted">not submitted (sim mode)</span>}
        </span>
      );
    case "relay":
      return (
        <span>
          slot {e.slot} · {weiToEth(e.value_wei, 4)} ETH paid to proposer · {new URL(e.relay).hostname}
        </span>
      );
  }
}
