"use client";

/**
 * Data-plane diagnostics (work order 0.3).
 *
 * One screen that answers "why am I seeing nothing?" for whichever plane is
 * sick. The acceptance bar: a human can distinguish, at a glance,
 *
 *   1. a missing bot            → `dataMode: demo` (proxy-injected fixtures;
 *                                 the DEMO badge is already in the header),
 *   2. a wrong console registry → the slug↔chain-id banner above this panel,
 *   3. a wrong-chain RPC        → the bot refuses to boot, so that chain's
 *                                 URL reads demo/unreachable here,
 *   4. normal absence of public pending flow → `live_canonical_only` with a
 *                                 healthy upstream: nothing is broken,
 *   5. a broken preconfirmation feed → flashblocks stalled/down while the
 *                                 canonical head stays fresh.
 *
 * Every number on this card comes from `/api/status` (`dataMode`, `head`,
 * `upstream`, `flashblocks`, `chainBlocks`, `stats.sourceFunnels`) — the bot
 * computes the verdicts server-side so the same words mean the same thing in
 * the API, in this panel, and in an operator's runbook.
 */

import {memo} from "react";
import type {StatusResponse} from "@/lib/types";

type DataMode = NonNullable<StatusResponse["dataMode"]>;

const MODE_LABEL: Record<DataMode, string> = {
  live_preconfirmation: "LIVE · PRECONFIRMATION",
  live_canonical_only: "LIVE · CANONICAL ONLY",
  degraded: "DEGRADED",
  demo: "DEMO",
};

const MODE_TONE: Record<DataMode, string> = {
  live_preconfirmation: "#35d07f",
  live_canonical_only: "#22d3ee",
  degraded: "#f5b544",
  demo: "#f5b544",
};

const MODE_EXPLAINER: Record<DataMode, string> = {
  live_preconfirmation:
    "Canonical head is fresh and the preconfirmation feed is producing frames — Base flashblock-pinned candidates are possible.",
  live_canonical_only:
    "Canonical head is fresh; no fresh preconfirmation frames. Normal on Ethereum (no feed configured). On Base: sequencer-chain flow arriving only via canonical blocks — or a quiet/broken Flashblocks feed; see the preconfirmation card.",
  degraded:
    "The canonical head itself is stale. Whatever the rest of the console shows, nothing is live — check the upstream RPC card below.",
  demo: "Bot API unreachable — this screen is generated fixture data, not a live reading. A missing bot can never look like a broken feed.",
};

/** Millisecond duration → short human string ("1.2s", "4m 10s", "—" for none). */
function dur(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return "—";
  if (ms < 1_000) return `${Math.round(ms)}ms`;
  if (ms < 60_000) return `${(ms / 1_000).toFixed(1)}s`;
  if (ms < 3_600_000) return `${Math.floor(ms / 60_000)}m ${Math.floor((ms % 60_000) / 1_000)}s`;
  return `${Math.floor(ms / 3_600_000)}h ${Math.floor((ms % 3_600_000) / 60_000)}m`;
}

function fmt(n: number): string {
  return n < 1000 ? String(n) : n.toLocaleString("en-US");
}

function bps(b: number | null | undefined): string {
  if (b === null || b === undefined) return "—";
  return `${(b / 100).toFixed(b % 100 === 0 ? 0 : 1)}%`;
}

function ageOf(msSinceEpoch: number | undefined, now: number): string {
  if (!msSinceEpoch) return "never";
  return `${dur(Math.max(0, now - msSinceEpoch))} ago`;
}

function Stat({label, value, tone, title}: {label: string; value: string; tone?: string; title?: string}) {
  return (
    <div title={title} style={{display: "grid", gap: 1}}>
      <span className="muted" style={{fontSize: 9, textTransform: "uppercase", letterSpacing: "0.08em"}}>
        {label}
      </span>
      <span style={{fontSize: 13, fontVariantNumeric: "tabular-nums", color: tone ?? "inherit"}}>{value}</span>
    </div>
  );
}

function SubCard({title, tone, children}: {title: string; tone?: string; children: React.ReactNode}) {
  return (
    <div
      style={{
        border: "1px solid var(--line)",
        borderRadius: 4,
        padding: "8px 10px",
        display: "grid",
        gap: 8,
        alignContent: "start",
      }}
    >
      <div style={{display: "flex", alignItems: "center", gap: 6}}>
        <span className="dot" style={{background: tone ?? "var(--muted)"}} />
        <span style={{fontSize: 10, textTransform: "uppercase", letterSpacing: "0.08em", color: "var(--muted)"}}>
          {title}
        </span>
      </div>
      {children}
    </div>
  );
}

const SOURCE_LABEL: Record<string, string> = {
  chainBlock: "chain blocks",
  flashblocks: "flashblocks",
  sequencerFeed: "sequencer feed",
  publicMempool: "public mempool",
};

function DataPlanePanel({status, now}: {status: StatusResponse | null; now: number}) {
  const mode: DataMode = status?.dataMode ?? (status?.demo ? "demo" : "degraded");
  const up = status?.upstream;
  const fb = status?.flashblocks;
  const cb = status?.chainBlocks;
  const sources = status?.stats.sourceFunnels ?? {};
  const sourceKeys = Object.keys(sources).sort();

  const fbStateTone =
    fb?.connectionState === "connected"
      ? "#35d07f"
      : fb?.connectionState === "stalled"
        ? "#f5b544"
        : "#ef5350";

  return (
    <div className="panel" style={{padding: 14, display: "grid", gap: 12}}>
      {/* verdict row */}
      <div style={{display: "flex", alignItems: "center", gap: 12, flexWrap: "wrap"}}>
        <span
          className="badge"
          style={{color: MODE_TONE[mode], borderColor: MODE_TONE[mode], fontSize: 11, padding: "3px 8px"}}
        >
          <span className="dot" style={{background: MODE_TONE[mode]}} />
          {MODE_LABEL[mode]}
        </span>
        <span className="muted" style={{fontSize: 11, maxWidth: 900}}>{MODE_EXPLAINER[mode]}</span>
      </div>

      <div
        style={{
          display: "grid",
          gap: 10,
          gridTemplateColumns: "repeat(auto-fit, minmax(230px, 1fr))",
        }}
      >
        {/* upstream RPC */}
        <SubCard
          title="upstream rpc"
          tone={
            !up
              ? "var(--muted)"
              : up.errors === 0
                ? "#35d07f"
                : up.errorRateBps > 1_000
                  ? "#ef5350"
                  : "#f5b544"
          }
        >
          {up ? (
            <>
              <div style={{display: "grid", gridTemplateColumns: "1fr 1fr 1fr", gap: 8}}>
                <Stat label="requests" value={fmt(up.requests)} />
                <Stat label="errors" value={fmt(up.errors)} tone={up.errors ? "#f5b544" : undefined} />
                <Stat label="error rate" value={bps(up.errorRateBps)} />
                <Stat
                  label="rate limited"
                  value={fmt(up.rateLimited)}
                  tone={up.rateLimited ? "#f5b544" : undefined}
                  title="HTTP 429 + provider rate-limit JSON-RPC errors — on a public sequencer endpoint, sustained load-shedding here means the data plane needs a paid RPC"
                />
                <Stat label="avg latency" value={dur(up.avgLatencyMs)} />
                <Stat label="last ok" value={ageOf(up.lastOkMs, now)} />
              </div>
            </>
          ) : (
            <span className="muted" style={{fontSize: 11}}>bot predates the upstream counters — upgrade it.</span>
          )}
        </SubCard>

        {/* canonical head */}
        <SubCard
          title="canonical head"
          tone={status && (status.head.ageMs ?? 0) < 8_000 ? "#35d07f" : "#f5b544"}
        >
          <div style={{display: "grid", gridTemplateColumns: "1fr 1fr 1fr", gap: 8}}>
            <Stat label="block" value={status ? `#${status.head.number}` : "—"} />
            <Stat
              label="age"
              value={dur(status?.head.ageMs)}
              tone={status && (status.head.ageMs ?? 0) >= 8_000 ? "#f5b544" : undefined}
            />
            <Stat
              label="gas used"
              value={status ? fmt(status.head.gasUsed) : "—"}
              title="gas consumed by the newest known canonical block"
            />
          </div>
          {cb?.configured ? (
            <div style={{display: "grid", gridTemplateColumns: "1fr 1fr 1fr", gap: 8}}>
              <Stat
                label="blocks fetched"
                value={fmt(cb.blocksFetched ?? 0)}
                title="chain-native full-block fetches (replay + sequencer qualification evidence)"
              />
              <Stat
                label="fetches failed"
                value={fmt(cb.fetchesFailed ?? 0)}
                tone={(cb.fetchesFailed ?? 0) > 0 ? "#f5b544" : undefined}
                title="full-block fetch failures — public RPCs rate-limit these; coverage below 100% means delivered-block evidence has gaps"
              />
              <Stat label="fetch coverage" value={bps(cb.fetchSuccessRateBps)} />
            </div>
          ) : (
            <span className="muted" style={{fontSize: 11}}>
              chain-block ingest off — canonical flow arrives via the head feed only.
            </span>
          )}
        </SubCard>

        {/* preconfirmation feed */}
        <SubCard
          title="preconfirmation feed"
          tone={!fb?.configured ? "var(--muted)" : fbStateTone}
        >
          {fb?.configured ? (
            <>
              <div style={{display: "grid", gridTemplateColumns: "1fr 1fr 1fr", gap: 8}}>
                <Stat
                  label="state"
                  value={fb.connectionState ?? "—"}
                  tone={fbStateTone}
                  title="<2s since last frame = connected, <10s = stalled, otherwise down (frames seal at ~200ms)"
                />
                <Stat label="last frame" value={fb.lastFrameAgeMs != null ? `${dur(fb.lastFrameAgeMs)} ago` : "never"} />
                <Stat
                  label="reconnects"
                  value={fmt(fb.reconnects ?? 0)}
                  tone={(fb.reconnects ?? 0) > 0 ? "#f5b544" : undefined}
                />
                <Stat label="frames" value={fmt(fb.framesTotal ?? 0)} />
                <Stat
                  label="malformed"
                  value={fmt(fb.framesMalformed ?? 0)}
                  tone={(fb.framesMalformed ?? 0) > 0 ? "#f5b544" : undefined}
                />
                <Stat
                  label="state gaps"
                  value={fmt(fb.stateGaps ?? 0)}
                  tone={(fb.stateGaps ?? 0) > 0 ? "#f5b544" : undefined}
                />
                <Stat
                  label="sealed match"
                  value={bps(fb.sealedMatchRateBps)}
                  tone={fb.sealedMatchRateBps != null && fb.sealedMatchRateBps < 9_500 ? "#ef5350" : undefined}
                  title="share of sealed blocks whose transaction sequence exactly matched the preconfirmed stream — a feed that cannot match what seals cannot be trusted to trigger sends"
                />
                <Stat
                  label="sealed lead"
                  value={dur(fb.lastSealedLeadMs)}
                  title="how far ahead of the canonical seal the last matched frame arrived — the real speed advantage of the feed"
                />
                <Stat
                  label="dup txs"
                  value={fmt(fb.txsDuplicate ?? 0)}
                  title="duplicate txs absorbed by the dedupe window (normal after a reconnect)"
                />
              </div>
            </>
          ) : (
            <span className="muted" style={{fontSize: 11}}>
              no preconfirmation feed configured on this chain — flashblock-pinned candidates cannot exist here.
            </span>
          )}
        </SubCard>
      </div>

      {/* per-source funnels */}
      <div>
        <div className="muted" style={{fontSize: 10, textTransform: "uppercase", letterSpacing: "0.08em", marginBottom: 6}}>
          live candidates by data source
          <span style={{textTransform: "none", letterSpacing: 0, marginLeft: 8}}>
            attribution per candidate, never combined with the per-strategy relay/mempool funnels
          </span>
        </div>
        {sourceKeys.length === 0 ? (
          <span className="muted" style={{fontSize: 11}}>
            no live candidates attributed yet — on a healthy chain this is where chain-block, flashblocks,
            sequencer-feed or public-mempool flow shows up.
          </span>
        ) : (
          <table style={{width: "100%", borderCollapse: "collapse", fontSize: 11}}>
            <thead>
              <tr className="muted" style={{textAlign: "left", fontSize: 9, textTransform: "uppercase"}}>
                <th style={{padding: "2px 6px"}}>source</th>
                <th style={{padding: "2px 6px", textAlign: "right"}}>candidates</th>
                <th style={{padding: "2px 6px", textAlign: "right"}}>gated by risk</th>
                <th style={{padding: "2px 6px", textAlign: "right"}}>simulated</th>
                <th style={{padding: "2px 6px", textAlign: "right"}}>sim rate</th>
              </tr>
            </thead>
            <tbody>
              {sourceKeys.map((k) => {
                const f = sources[k];
                const simRate = f.gatedByRisk < f.candidates
                  ? (100 * f.simulated) / (f.candidates - f.gatedByRisk)
                  : null;
                return (
                  <tr key={k} style={{borderTop: "1px solid var(--line)"}}>
                    <td style={{padding: "3px 6px"}}>{SOURCE_LABEL[k] ?? k}</td>
                    <td style={{padding: "3px 6px", textAlign: "right", fontVariantNumeric: "tabular-nums"}}>
                      {fmt(f.candidates)}
                    </td>
                    <td style={{padding: "3px 6px", textAlign: "right", fontVariantNumeric: "tabular-nums"}}>
                      {fmt(f.gatedByRisk)}
                    </td>
                    <td style={{padding: "3px 6px", textAlign: "right", fontVariantNumeric: "tabular-nums"}}>
                      {fmt(f.simulated)}
                    </td>
                    <td style={{padding: "3px 6px", textAlign: "right", fontVariantNumeric: "tabular-nums"}}>
                      {simRate === null ? "—" : `${simRate.toFixed(0)}%`}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}

export default memo(DataPlanePanel);
