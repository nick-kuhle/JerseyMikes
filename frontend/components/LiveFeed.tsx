"use client";

import {memo, useCallback, useDeferredValue, useEffect, useMemo, useRef} from "react";
import {useVirtualizer} from "@tanstack/react-virtual";
import type {FeedEvent} from "@/lib/types";
import {clock, shortHash, STRATEGY_COLOR, weiToEth} from "@/lib/format";
import {blockUrl, txUrl} from "@/lib/explorer";

/**
 * The live tape: every mempool transaction, MEV-Share hint, block, opportunity
 * and simulation the bot sees, newest first. Every hash that exists on chain
 * links out to the explorer.
 *
 * **Virtualized.** The tape holds up to `FEED_MAX` (400) events and used to
 * mount a `<tr>` for every one of them, each with its own explorer links and
 * formatting work — 400 rows rebuilt on every batch, of which ~15 are on
 * screen. Now only the visible window plus a small overscan is mounted, so
 * render cost is bounded by viewport height rather than buffer depth.
 *
 * Rows are `React.memo`'d on the event they render, and the list reads a
 * deferred copy of the array so a burst of arrivals cannot make the filter
 * dropdown or the rest of the page feel stuck.
 */

/**
 * Estimated row height in px.
 *
 * Measured from the rendered table: 13px monospace line + 5px vertical
 * padding top/bottom + 1px border. This is only the *initial* estimate —
 * rows report their true height back through `measureElement` below, so a
 * font or padding change corrects itself instead of silently desynchronising
 * the scrollbar from the content.
 */
const ROW_HEIGHT = 31;
/** Rows rendered beyond the viewport, so fast scrolling does not show gaps. */
const OVERSCAN = 12;

export default function LiveFeed({
  events,
  filter,
  chainId,
}: {
  events: FeedEvent[];
  filter: string;
  /** Chain the bot follows; drives explorer links. */
  chainId?: number;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);

  // Under a burst the tape is the lowest-priority thing on the page: React may
  // serve a slightly stale list to keep filter changes and the rest of the
  // dashboard responsive.
  const deferred = useDeferredValue(events);
  const shown = useMemo(
    () => (filter === "all" ? deferred : deferred.filter((e) => e.kind === filter)),
    [deferred, filter],
  );

  const virtualizer = useVirtualizer({
    count: shown.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: useCallback(() => ROW_HEIGHT, []),
    overscan: OVERSCAN,
    // Trust the DOM over the estimate: rows are a single unwrapped line, but
    // their exact height depends on the font stack the browser resolves.
    measureElement: (el) => el.getBoundingClientRect().height,
  });

  // Newest events arrive at the top; keep the viewport pinned there so the
  // tape reads as a live feed. Only when the user is already at the top —
  // scrolling back through history should not be yanked away.
  const count = shown.length;
  useEffect(() => {
    const el = scrollRef.current;
    if (el && el.scrollTop < ROW_HEIGHT * 2) el.scrollTop = 0;
  }, [count]);

  const items = virtualizer.getVirtualItems();
  const totalHeight = virtualizer.getTotalSize();
  // Absolute positioning inside a table breaks row layout, so the offset is
  // applied by translating the <tbody> and padding the scroll area to the
  // full virtual height.
  const paddingTop = items.length > 0 ? items[0].start : 0;
  const paddingBottom = items.length > 0 ? totalHeight - items[items.length - 1].end : 0;

  return (
    <div ref={scrollRef} style={{maxHeight: 420, overflowY: "auto"}}>
      <table className="grid" style={{tableLayout: "fixed", width: "100%"}}>
        <thead>
          <tr>
            <th style={{width: 70}}>time</th>
            <th style={{width: 110}}>kind</th>
            <th>detail</th>
          </tr>
        </thead>
        <tbody>
          {count === 0 && (
            <tr>
              <td colSpan={3} className="muted" style={{padding: 16, textAlign: "center"}}>
                waiting for events…
              </td>
            </tr>
          )}
          {paddingTop > 0 && (
            <tr aria-hidden style={{height: paddingTop}}>
              <td colSpan={3} style={{padding: 0, border: "none"}} />
            </tr>
          )}
          {items.map((item) => (
            <FeedRow
              key={item.key}
              event={shown[item.index]}
              chainId={chainId}
              index={item.index}
              measureRef={virtualizer.measureElement}
            />
          ))}
          {paddingBottom > 0 && (
            <tr aria-hidden style={{height: paddingBottom}}>
              <td colSpan={3} style={{padding: 0, border: "none"}} />
            </tr>
          )}
        </tbody>
      </table>
    </div>
  );
}

/**
 * One tape row.
 *
 * Memoized on the event identity: feed events are immutable once parsed, so a
 * row that is still in the window after a flush re-uses its previous render
 * instead of rebuilding its links and formatted numbers.
 */
const FeedRow = memo(function FeedRow({
  event,
  chainId,
  index,
  measureRef,
}: {
  event: FeedEvent;
  chainId?: number;
  index: number;
  /** Reports this row's real height back to the virtualizer. */
  measureRef: (el: HTMLElement | null) => void;
}) {
  return (
    <tr ref={measureRef} data-index={index}>
      <td className="muted">{clock(eventTime(event))}</td>
      <td>
        <span className="badge" style={{color: kindColor(event)}}>
          {label(event)}
        </span>
      </td>
      <td style={{whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis", maxWidth: 620}}>
        {detail(event, chainId)}
      </td>
    </tr>
  );
});

function eventTime(e: FeedEvent): number {
  if ("seen_at_ms" in e && e.seen_at_ms) return e.seen_at_ms;
  if (e.kind === "block") return e.timestamp * 1000;
  return Date.now();
}

function label(e: FeedEvent): string {
  switch (e.kind) {
    case "mev_share_hint":
      return "mev-share";
    case "relay_block":
      return "bloxroute";
    default:
      return e.kind;
  }
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
    case "relay_block":
      return "#4f8bff";
    case "alert":
      return "var(--amber)";
    case "reorg":
      return "#ff5c5c";
  }
}

function detail(e: FeedEvent, chainId?: number) {
  const txLink = (hash: string, label?: string) => {
    const url = txUrl(chainId, hash);
    if (!url)
      return (
        <span title={hash} style={{cursor: "default"}}>
          {label ?? shortHash(hash)}
        </span>
      );
    return (
      <a href={url} target="_blank" rel="noreferrer" title={`${hash} — view on the explorer`} style={{color: "#22d3ee", textDecoration: "none"}}>
        {label ?? shortHash(hash)} ↗
      </a>
    );
  };
  switch (e.kind) {
    case "block":
      return (
        <span>
          {(() => {
            const url = blockUrl(chainId, e.number);
            return url ? (
              <a href={url} target="_blank" rel="noreferrer" style={{color: "var(--cyan)", textDecoration: "none"}}>
                #{e.number} ↗
              </a>
            ) : (
              `#${e.number}`
            );
          })()}{" "}
          · base fee {(Number(e.base_fee_per_gas) / 1e9).toFixed(2)} gwei · gas{" "}
          {(e.gas_used / 1e6).toFixed(2)}M · {shortHash(e.hash)}
        </span>
      );
    case "pending":
      return (
        <span>
          {txLink(e.hash)} → {shortHash(e.to)} · {weiToEth(e.value, 4)} ETH · {e.selector ?? "—"} ·{" "}
          <span className="muted">{e.source}</span>
        </span>
      );
    case "mev_share_hint":
      return (
        <span>
          {txLink(e.hash)} · {e.logs} logs · fns [{e.functions.join(", ") || "redacted"}]
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
          slot {e.slot} · {weiToEth(e.value_wei, 4)} ETH paid to proposer · {safeHost(e.relay)}
        </span>
      );
    case "relay_block":
      return (
        <span>
          {(() => {
            const url = blockUrl(chainId, e.block.block_number);
            return url ? (
              <a href={url} target="_blank" rel="noreferrer" style={{color: "var(--cyan)", textDecoration: "none"}}>
                #{e.block.block_number} ↗
              </a>
            ) : (
              `#${e.block.block_number}`
            );
          })()}{" "}
          · {e.tx_count} txs · {weiToEth(e.block.value_wei, 4)} ETH builder bid ·{" "}
          <span className="muted">
            {e.txs
              .slice(0, 3)
              .map((t) => (
                <span key={t.hash}>{txLink(t.hash, shortHash(t.hash, 4))}:{t.selector ?? "—"} </span>
              ))}
          </span>
        </span>
      );
    case "alert": {
      const ev = e as unknown as {severity?: string; rule?: string; message?: string; active?: boolean};
      return (
        <span style={{color: ev.active ? (ev.severity === "critical" ? "#ff5c5c" : "#f5b544") : undefined}}>
          {ev.active ? "⚠" : "✓"} {ev.severity} · {ev.rule} — {ev.message}
        </span>
      );
    }
    case "reorg":
      return (
        <span>
          depth {e.depth} · discarded #{e.from_block}
          {e.to_block !== e.from_block ? `–${e.to_block}` : ""} · {txLink(e.old_hash)} → {txLink(e.new_hash)}
        </span>
      );
  }
}

/**
 * Hostname of a relay URL, tolerating a non-URL string.
 *
 * `new URL()` throws on malformed input, and this runs inside the render path
 * of a row whose data comes off the wire — a bad relay string used to take the
 * whole tape down with it.
 */
function safeHost(url: string): string {
  try {
    return new URL(url).hostname;
  } catch {
    return url;
  }
}
