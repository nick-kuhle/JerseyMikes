"use client";

import {useCallback, useEffect, useMemo, useState} from "react";
import EquityChart from "./EquityChart";
import LiveFeed from "./LiveFeed";
import ContractPanel from "./ContractPanel";
import GoLivePanel from "./GoLivePanel";
import EligibilityPanel from "./EligibilityPanel";
import RiskPanel from "./RiskPanel";
import SniperPanel from "./SniperPanel";
import FunnelPanel from "./FunnelPanel";
import RelayBlocksPanel from "./RelayBlocksPanel";
import Phase1Panel from "./Phase1Panel";
import ModeSwitch from "./ModeSwitch";
import ChainSwitcher from "./ChainSwitcher";
import Section from "./Section";
import WalletButton from "./WalletButton";
import type {
  ActualMevResponse,
  CompetitionResponse,
  ExecutionResponse,
  OpportunityRow,
  PnlResponse,
  RelayBid,
  ReorgRow,
  SeriesPoint,
  SimulationRow,
  StatusResponse,
} from "@/lib/types";
import {
  ago,
  gwei,
  shortHash,
  signedEth,
  STRATEGY_COLOR,
  STRATEGY_LABEL,
  weiToEth,
} from "@/lib/format";
import {blockUrl, txUrl} from "@/lib/explorer";
import {useFeed} from "@/lib/feed";
import {onChainChange, readActiveChain, withChain} from "@/lib/chain";
import {useWallet} from "@/lib/wallet";

/** Chain-id → operator-friendly label for the mismatch banner. Kept in sync
 *  with the switcher's LABELS map in `frontend/lib/chains.ts`. */
const CHAIN_ID_LABEL: Record<number, string> = {
  1: "Ethereum",
  8453: "Base",
  42161: "Arbitrum",
  10: "Optimism",
  137: "Polygon",
  56: "BNB",
};
const labelFor = (id: number | null | undefined) =>
  id == null ? "an unknown chain" : CHAIN_ID_LABEL[id] ?? `chain ${id}`;

const FEED_MAX = 400;
const POLL_MS = 4000;

export default function Console() {
  // Multi-chain: the active chain slug drives every API path (the server
  // falls back to the default chain when it is null or unknown) and the
  // keyed remount below so no panel can show another chain's data.
  const [chainSlug, setChainSlug] = useState<string | null>(null);
  const [status, setStatus] = useState<StatusResponse | null>(null);
  const [pnl, setPnl] = useState<PnlResponse | null>(null);
  const [series, setSeries] = useState<SeriesPoint[]>([]);
  const [sims, setSims] = useState<SimulationRow[]>([]);
  const [opps, setOpps] = useState<OpportunityRow[]>([]);
  const [bids, setBids] = useState<RelayBid[]>([]);
  const [competition, setCompetition] = useState<CompetitionResponse | null>(null);
  const [actualMev, setActualMev] = useState<ActualMevResponse | null>(null);
  const [executions, setExecutions] = useState<ExecutionResponse | null>(null);
  const [reorgs, setReorgs] = useState<ReorgRow[]>([]);
  const [feedFilter, setFeedFilter] = useState("all");
  const [strategyFilter, setStrategyFilter] = useState("all");
  // Batched + typed SSE consumption: frames accumulate off-render and flush
  // together, so a 200-event burst costs one render instead of 200.
  const {events, connected} = useFeed(withChain("/api/stream", chainSlug), FEED_MAX);

  const load = useCallback(async () => {
    const get = async <T,>(p: string, fallback: T): Promise<T> => {
      try {
        const r = await fetch(withChain(`/api/bot/${p}`, chainSlug), {cache: "no-store"});
        return (await r.json()) as T;
      } catch {
        return fallback;
      }
    };
    const [s, p, se, si, op, rb, comp, actual, executionRows, rg] = await Promise.all([
      get<StatusResponse | null>("status", null),
      get<PnlResponse | null>("pnl", null),
      get<SeriesPoint[]>("pnl/series?limit=250", []),
      get<SimulationRow[]>("simulations?limit=120", []),
      get<OpportunityRow[]>("opportunities?limit=60", []),
      get<RelayBid[]>("relay-bids?limit=25", []),
      get<CompetitionResponse | null>("competition?limit=25", null),
      get<ActualMevResponse | null>("actual-mev?limit=25", null),
      get<ExecutionResponse | null>("executions?limit=25", null),
      get<ReorgRow[]>("reorgs?limit=15", []),
    ]);
    // Identity-preserving updates.
    //
    // Every poll used to hand each `useState` a brand new array or object,
    // even on the ~95% of ticks where the bot returned exactly the same rows.
    // A new identity re-renders every consumer and defeats `memo` /
    // `useMemo` downstream — the equity chart re-rendered its SVG four times
    // a second against unchanged data. `keepIfSame` swaps in the new value
    // only when the serialised payload actually differs.
    if (s) setStatus((prev) => keepIfSame(prev, s));
    if (p) setPnl((prev) => keepIfSame(prev, p));
    setSeries((prev) => keepIfSame(prev, Array.isArray(se) ? se : []));
    setSims((prev) => keepIfSame(prev, Array.isArray(si) ? si : []));
    setOpps((prev) => keepIfSame(prev, Array.isArray(op) ? op : []));
    setBids((prev) => keepIfSame(prev, Array.isArray(rb) ? rb : []));
    if (comp) setCompetition((prev) => keepIfSame(prev, comp));
    if (actual) setActualMev((prev) => keepIfSame(prev, actual));
    if (executionRows) setExecutions((prev) => keepIfSame(prev, executionRows));
    setReorgs((prev) => keepIfSame(prev, Array.isArray(rg) ? rg : []));
  }, [chainSlug]);

  // Active chain: initialise from localStorage, follow the switcher.
  useEffect(() => {
    setChainSlug(readActiveChain());
    return onChainChange(setChainSlug);
  }, []);

  // Wallet state for the mismatch banner (WS-H3). `useWallet` is a shared
  // context so this doesn't spin up another provider subscription.
  const wallet = useWallet();

  useEffect(() => {
    load();
    const t = setInterval(load, POLL_MS);
    return () => clearInterval(t);
  }, [load]);

  const demo = Boolean(status?.demo);
  const chainId = status?.chain.id;
  // Amber banner when the wallet and the console are pointed at different
  // chains — a real-world source of confusion (WS-H3). Suppressed when the
  // wallet isn't connected or the bot's chain hasn't come back yet.
  const walletMismatch =
    wallet.address !== null &&
    wallet.chainId !== null &&
    chainId !== undefined &&
    wallet.chainId !== chainId;

  // Tab title: prefix the active chain so a screenshot or a browser tab
  // strip reads "Base · JerseyMikes …" (WS-H4). Runs client-side only.
  useEffect(() => {
    if (typeof document === "undefined") return;
    const name = status?.chain.name ?? (chainSlug ? chainSlug[0].toUpperCase() + chainSlug.slice(1) : "");
    document.title = name ? `${name} · JerseyMikes console` : "JerseyMikes — MEV simulation console";
  }, [status?.chain.name, chainSlug]);
  const totalNet = pnl?.totalNetWei ?? "0";
  const filteredSims = useMemo(
    () => (strategyFilter === "all" ? sims : sims.filter((s) => s.strategy === strategyFilter)),
    [sims, strategyFilter]
  );

  // One pass over the strategy rows for both the win rate and the sim total,
  // instead of two `reduce`s plus a third inline in the card below.
  const {winRate, totalSims} = useMemo(() => {
    const rows = pnl?.byStrategy ?? [];
    let w = 0;
    let n = 0;
    for (const r of rows) {
      w += r.wins;
      n += r.simulations;
    }
    return {winRate: n ? (100 * w) / n : 0, totalSims: n};
  }, [pnl]);

  return (
    <main style={{padding: 12, display: "grid", gap: 12, maxWidth: 1800, margin: "0 auto"}}>
      {/* header */}
      <header className="panel" style={{display: "flex", alignItems: "center", gap: 16, padding: "10px 14px", flexWrap: "wrap"}}>
        <div style={{fontSize: 15, letterSpacing: "0.06em"}}>
          JERSEY<span style={{color: "#22d3ee"}}>MIKES</span>
          <span className="muted" style={{marginLeft: 8, fontSize: 11}}>MEV simulation console</span>
        </div>

        <ChainSwitcher />

        <ModeSwitch mode={status?.mode} armed={status?.liveArmed} demo={demo} onChanged={load} />

        {demo && (
          <span className="badge" style={{color: "#f5b544"}} title="bot API unreachable — showing generated data">
            DEMO DATA
          </span>
        )}

        <span className={connected ? "badge live" : "badge"} style={{color: connected ? "#35d07f" : "#6b7c93"}}>
          <span className="dot" style={{background: connected ? "#35d07f" : "#6b7c93"}} /> feed
        </span>

        <WalletButton expectedChainId={chainId} />

        <div style={{marginLeft: "auto", display: "flex", gap: 18, alignItems: "center", flexWrap: "wrap"}}>
          <HeadStat label="chain" value={status ? `${status.chain.name} (${status.chain.id})` : "—"} />
          <HeadStat label="block" value={status ? `#${status.head.number}` : "—"} />
          <HeadStat label="base fee" value={status ? `${gwei(status.head.baseFeeWei)} gwei` : "—"} />
          <HeadStat label="pools" value={status ? String(status.pools) : "—"} />
          <HeadStat label="nonce" value={status?.inventory ? String(status.inventory.nonce) : "—"} />
          <HeadStat
            label="kill switch"
            value={status?.risk.killSwitchTripped ? "TRIPPED" : "armed"}
            tone={status?.risk.killSwitchTripped ? "neg" : undefined}
          />
        </div>
      </header>

      <div key={chainSlug ?? "default"}>
      {/* wallet ↔ console chain mismatch (WS-H3): a wallet on Ethereum while
          the console shows Base is a real bug source — spell it out.
          Suppressed when either side is unknown. */}
      {walletMismatch && (
        <div
          role="status"
          style={{
            border: "1px solid var(--amber)",
            background: "rgba(245, 181, 68, 0.08)",
            color: "var(--amber)",
            padding: "6px 10px",
            borderRadius: 4,
            fontSize: 11,
            marginBottom: 8,
          }}
        >
          wallet is on <strong>{labelFor(wallet.chainId)}</strong> — console is showing{" "}
          <strong>{status?.chain.name ?? labelFor(chainId)}</strong>. Chain-scoped actions (deploy,
          allowlist, fund) will refuse until the wallet switches.
        </div>
      )}

      {/* jump nav — the page is long; this keeps every section one click away */}
      <nav
        style={{
          position: "sticky",
          top: 0,
          zIndex: 30,
          display: "flex",
          gap: 4,
          flexWrap: "wrap",
          alignItems: "center",
          background: "rgba(4, 6, 8, 0.92)",
          border: "1px solid var(--line)",
          borderRadius: 4,
          padding: "5px 8px",
        }}
      >
        <span className="muted" style={{fontSize: 9, textTransform: "uppercase", letterSpacing: "0.08em"}}>
          jump
        </span>
        {[
          ["pnl", "P/L"],
          ["activity", "Activity"],
          ["history", "Transactions"],
          ["relay", "Relay blocks"],
          ["funnel", "Funnel"],
          ["sniper", "Sniper"],
          ["risk", "Controls"],
          ["golive", "Go live"],
          ["executor", "Executor"],
        ].map(([id, label]) => (
          <a
            key={id}
            href={`#${id}`}
            style={{
              fontSize: 11,
              color: "var(--muted)",
              textDecoration: "none",
              padding: "2px 8px",
              borderRadius: 4,
              border: "1px solid transparent",
            }}
            onMouseEnter={(e) => {
              e.currentTarget.style.color = "var(--cyan)";
              e.currentTarget.style.borderColor = "var(--line)";
            }}
            onMouseLeave={(e) => {
              e.currentTarget.style.color = "var(--muted)";
              e.currentTarget.style.borderColor = "transparent";
            }}
          >
            {label}
          </a>
        ))}
      </nav>

      {/* stat cards */}
      <section style={{display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(180px, 1fr))", gap: 12}}>
        <Card
          title="simulated net P/L"
          value={`${signedEth(totalNet)} ETH`}
          tone={BigInt(totalNet) >= 0n ? "pos" : "neg"}
          sub="fork simulations only"
        />
        <Card title="win rate" value={`${winRate.toFixed(1)}%`} sub={`${totalSims} sims`} />
        <Card
          title="opportunities"
          value={String(status?.stats.opportunities ?? 0)}
          sub={`${status?.stats.rejected ?? 0} risk-rejected`}
        />
        <Card
          title="would-submit"
          value={String(status?.stats.submittable ?? 0)}
          tone="pos"
          sub="net-positive bundles"
        />
        <Card
          title="mempool seen"
          value={(status?.stats.pendingSeen ?? 0).toLocaleString()}
          sub={`${status?.stats.hintsSeen ?? 0} mev-share hints`}
        />
        <Card
          title="sim backends"
          value={`${status?.simBackends.anvilFork ? "fork" : "—"} / ${status?.simBackends.relayCallBundle ? "relay" : "—"}`}
          sub="anvil / eth_callBundle"
        />
        <Card
          title="bloxroute blocks"
          value={(status?.stats.relayBlocksSeen ?? 0).toLocaleString()}
          sub={`${(status?.stats.relayTxsSeen ?? 0).toLocaleString()} delivered txs`}
        />
      </section>

      {/* equity + strategies */}
      <section
        id="pnl"
        style={{display: "grid", gridTemplateColumns: "minmax(0, 2fr) minmax(0, 1fr)", gap: 12, scrollMarginTop: 56}}
      >
        <div className="panel">
          <div className="panel-head">
            <span>cumulative simulated P/L (ETH)</span>
            <span className="muted">{series.length} blocks</span>
          </div>
          <EquityChart series={series} />
        </div>

        <div className="panel">
          <div className="panel-head">
            <span>per-strategy</span>
          </div>
          <table className="grid">
            <thead>
              <tr>
                <th>strategy</th>
                <th style={{textAlign: "right"}}>sims</th>
                <th style={{textAlign: "right"}}>win</th>
                <th style={{textAlign: "right"}}>net ETH</th>
              </tr>
            </thead>
            <tbody>
              {(pnl?.byStrategy ?? []).map((r) => (
                <tr key={r.strategy}>
                  <td>
                    <span className="dot" style={{background: STRATEGY_COLOR[r.strategy], marginRight: 6}} />
                    {STRATEGY_LABEL[r.strategy] ?? r.strategy}
                  </td>
                  <td style={{textAlign: "right"}}>{r.simulations}</td>
                  <td style={{textAlign: "right"}}>
                    {r.simulations ? `${((100 * r.wins) / r.simulations).toFixed(0)}%` : "—"}
                  </td>
                  <td style={{textAlign: "right"}} className={BigInt(r.net_profit_wei) >= 0n ? "pos" : "neg"}>
                    {signedEth(r.net_profit_wei)}
                  </td>
                </tr>
              ))}
              {!pnl?.byStrategy.length && (
                <tr>
                  <td colSpan={4} className="muted" style={{textAlign: "center", padding: 16}}>
                    no data yet
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </section>

      {/* feed + simulations */}
      <Section id="activity" title="Activity" subtitle="live tape · events">
      <section style={{display: "grid", gridTemplateColumns: "minmax(0, 1fr) minmax(0, 1fr)", gap: 12}}>
        <div className="panel">
          <div className="panel-head">
            <span>live data feed</span>
            <select value={feedFilter} onChange={(e) => setFeedFilter(e.target.value)} style={selectStyle}>
              {[
                "all",
                "pending",
                "block",
                "mev_share_hint",
                "opportunity",
                "simulation",
                "bundle",
                "relay",
                "relay_block",
                "reorg",
                "alert",
              ].map((k) => (
                <option key={k} value={k}>
                  {k}
                </option>
              ))}
            </select>
          </div>
          <LiveFeed events={events} filter={feedFilter} chainId={chainId} />
        </div>

        <div className="panel">
          <div className="panel-head">
            <span>simulated transaction history</span>
            <select value={strategyFilter} onChange={(e) => setStrategyFilter(e.target.value)} style={selectStyle}>
              {[
                "all",
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
              ].map((k) => (
                <option key={k} value={k}>
                  {k}
                </option>
              ))}
            </select>
          </div>
          <div style={{maxHeight: 420, overflowY: "auto"}}>
            <table className="grid">
              <thead>
                <tr>
                  <th>age</th>
                  <th>strategy</th>
                  <th>backend</th>
                  <th style={{textAlign: "right"}}>gas</th>
                  <th style={{textAlign: "right"}}>gross</th>
                  <th style={{textAlign: "right"}}>net ETH</th>
                  <th>victim tx</th>
                  <th>result</th>
                </tr>
              </thead>
              <tbody>
                {filteredSims.map((s, i) => {
                  const victim = s.victims ? s.victims.split(",")[0] : null;
                  const link = txUrl(chainId, victim);
                  return (
                    <tr key={`${s.opportunityId}-${i}`} title={s.notes}>
                      <td className="muted">{ago(s.createdAtMs)}</td>
                      <td style={{color: STRATEGY_COLOR[s.strategy]}}>{s.strategy}</td>
                      <td className="muted">{s.backend}</td>
                      <td style={{textAlign: "right"}}>{s.gasUsed.toLocaleString()}</td>
                      <td style={{textAlign: "right"}}>{weiToEth(s.grossWei, 5)}</td>
                      <td style={{textAlign: "right"}} className={BigInt(s.netWei) >= 0n ? "pos" : "neg"}>
                        {signedEth(s.netWei)}
                      </td>
                      <td>
                        {link && victim ? (
                          <a
                            href={link}
                            target="_blank"
                            rel="noreferrer"
                            title={`victim tx ${victim} — view on the block explorer`}
                            style={{color: "#22d3ee", textDecoration: "none"}}
                          >
                            {shortHash(victim, 4)} ↗
                          </a>
                        ) : (
                          <span className="muted">—</span>
                        )}
                      </td>
                      <td className={s.success ? "pos" : "muted"}>
                        <SimVerdict success={s.success} revertReason={s.revertReason} />
                      </td>
                    </tr>
                  );
                })}
                {!filteredSims.length && (
                  <tr>
                    <td colSpan={8} className="muted" style={{textAlign: "center", padding: 16}}>
                      no simulations yet
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>
        </div>
      </section>

      </Section>

      {/* transactions + opportunities */}
      <Section id="history" title="Simulated transactions & opportunities" subtitle="newest first">
      <section style={{display: "grid", gridTemplateColumns: "minmax(0, 2fr) minmax(0, 1fr)", gap: 12}}>
        <div className="panel">
          <div className="panel-head">
            <span>opportunities found</span>
            <span className="muted">newest first</span>
          </div>
          <div style={{maxHeight: 300, overflowY: "auto"}}>
            <table className="grid">
              <thead>
                <tr>
                  <th>age</th>
                  <th>strategy</th>
                  <th style={{textAlign: "right"}}>expected</th>
                  <th style={{textAlign: "right"}}>notional</th>
                  <th>block</th>
                  <th>victim</th>
                  <th>notes</th>
                </tr>
              </thead>
              <tbody>
                {opps.map((o) => {
                  const victim = o.victims ? o.victims.split(",")[0] : null;
                  const victimLink = txUrl(chainId, victim);
                  const blockLink = blockUrl(chainId, o.targetBlock);
                  return (
                    <tr key={o.id}>
                      <td className="muted">{ago(o.createdAtMs)}</td>
                      <td style={{color: STRATEGY_COLOR[o.strategy]}}>{o.strategy}</td>
                      <td style={{textAlign: "right"}}>{weiToEth(o.expectedWei, 5)}</td>
                      <td style={{textAlign: "right"}}>{weiToEth(o.notionalWei, 3)}</td>
                      <td className="muted">
                        {blockLink ? (
                          <a
                            href={blockLink}
                            target="_blank"
                            rel="noreferrer"
                            title="view this block on the explorer"
                            style={{color: undefined}}
                          >
                            {o.targetBlock}
                          </a>
                        ) : (
                          o.targetBlock
                        )}
                      </td>
                      <td className="muted">
                        {victimLink && victim ? (
                          <a
                            href={victimLink}
                            target="_blank"
                            rel="noreferrer"
                            title={`victim tx ${victim}`}
                            style={{color: "#22d3ee", textDecoration: "none"}}
                          >
                            {shortHash(victim)} ↗
                          </a>
                        ) : o.victims ? (
                          shortHash(victim)
                        ) : (
                          "—"
                        )}
                      </td>
                      <td
                        className="muted"
                        style={{maxWidth: 420, overflow: "hidden", textOverflow: "ellipsis"}}
                        title={o.notes}
                      >
                        {o.notes}
                      </td>
                    </tr>
                  );
                })}
                {!opps.length && (
                  <tr>
                    <td colSpan={7} className="muted" style={{textAlign: "center", padding: 16}}>
                      nothing yet
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>
        </div>

        <div className="panel">
          <div className="panel-head">
            <span>relay payloads delivered</span>
            <span className="muted">market price of MEV</span>
          </div>
          <div style={{maxHeight: 300, overflowY: "auto"}}>
            <table className="grid">
              <thead>
                <tr>
                  <th>slot</th>
                  <th>relay</th>
                  <th style={{textAlign: "right"}}>value ETH</th>
                </tr>
              </thead>
              <tbody>
                {bids.map((b) => (
                  <tr key={`${b.relay}-${b.slot}`}>
                    <td className="muted">{b.slot}</td>
                    <td>{safeHost(b.relay)}</td>
                    <td style={{textAlign: "right"}}>{weiToEth(b.valueWei, 4)}</td>
                  </tr>
                ))}
                {!bids.length && (
                  <tr>
                    <td colSpan={3} className="muted" style={{textAlign: "center", padding: 16}}>
                      no relay data
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>
        </div>
      </section>

      </Section>

      <Section id="validation" title="Validation — latency & on-chain evidence" subtitle="decision-time simulations vs canonical blocks">
        <Phase1Panel
          latency={status?.latency}
          competition={competition}
          actualMev={actualMev}
          executions={executions}
          reorgs={reorgs}
        />
      </Section>

      {/* bloXroute Max Profit relay — delivered blocks + their transactions */}
      <Section id="relay" title="Relay — delivered blocks" subtitle="what MEV sold for, block by block" defaultOpen={false}>
        <RelayBlocksPanel chainId={chainId} />
      </Section>

      {/* strategy funnel — answers "why no opportunities?" with data */}
      <Section id="funnel" title="Strategy funnel" subtitle="why no opportunities? — with data">
      <FunnelPanel
        funnel={status?.stats.funnel ?? null}
        funnelReplay={status?.stats.funnelReplay ?? null}
        pendingSeen={status?.stats.pendingSeen ?? 0}
        hintsSeen={status?.stats.hintsSeen ?? 0}
        startedAtMs={status?.stats.startedAtMs}
        chainId={status?.chain.id}
      />

      </Section>

      {/* Directional sniper — its own lane, its own contract, its own risk
          envelope. Kept as a distinct section rather than a row inside the
          risk panel because nothing about it shares the atomic path's
          guarantees. See docs/SNIPER.md. */}
      <Section
        id="sniper"
        title="Sniper — new-token portfolio"
        subtitle="directional lane · holds positions · docs/SNIPER.md"
        defaultOpen={false}
      >
        <SniperPanel />
      </Section>

      {/* risk & strategy controls */}
      <Section id="risk" title="Risk & strategy controls" subtitle="applies instantly — no restart">
        <RiskPanel killSwitchTripped={status?.risk.killSwitchTripped} />
      </Section>

      {/* go-live checklist — deploying MevExecutor (Phase 3 readiness) */}
      <Section
        id="golive"
        title="Production go-live wizard · deploy & arm independently"
        subtitle="five-card wallet, vault, funding, pre-flight & live controls · docs/GO_LIVE.md"
        defaultOpen={false}
      >
        <div style={{padding: 4, display: "grid", gap: 12}}>
          <QualificationReport qualification={status?.qualification} />
          <EligibilityPanel enabled={status?.strategies} />
          <GoLivePanel executor={status?.executor ?? ""} armed={status?.liveArmed} chainId={chainId} />
        </div>
      </Section>

      {/* contract */}
      <Section id="executor" title="MevExecutor — on-chain control" subtitle={status ? shortHash(status.executor, 8) : "—"}>
        <ContractPanel executor={status?.executor ?? ""} chainId={chainId} />
      </Section>

      <footer className="muted" style={{padding: "4px 2px 20px", fontSize: 11}}>
        Broadcasting is disabled by default and remains fail-closed unless every arming, risk, inventory, and
        strategy-specific qualification gate passes. See <code>docs/GO_LIVE.md</code> and <code>docs/RISK.md</code>.
      </footer>
      </div>
    </main>
  );
}

/**
 * The `result` cell of the simulations table.
 *
 * Three outcomes, not two. A plain revert and an *uncertified* result both
 * used to render as grey prose in a `nowrap` cell, which conflated the two
 * cases that most need telling apart:
 *
 *   - "no edge" / a revert reason: the simulator looked and there was nothing
 *     there, or the bundle would genuinely have failed.
 *   - "uncertified": the bundle may well have been profitable, but the profit
 *     landed in a token the bot could not price against ETH gas at the pinned
 *     fork block, so it refuses to *claim* a number. This is fail-closed
 *     accounting, and it is usually fixed by configuration
 *     (`TOKEN_VALUATION=true`) rather than by strategy work.
 *
 * Reading the second as the first understates the strategy. The full reason
 * stays available on hover; only the label is short, because these strings run
 * to a full sentence and the cell is `nowrap`.
 */
function SimVerdict({success, revertReason}: {success: boolean; revertReason: string | null}) {
  if (success) return <>profitable</>;
  const reason = revertReason ?? "no edge";
  if (reason.startsWith("uncertified accounting")) {
    return (
      <span style={{color: "var(--amber)"}} title={reason}>
        uncertified
      </span>
    );
  }
  return (
    <span title={reason.length > 40 ? reason : undefined}>
      {reason.length > 40 ? `${reason.slice(0, 39)}…` : reason}
    </span>
  );
}

function QualificationReport({qualification}: {qualification: StatusResponse["qualification"]}) {
  const rows = qualification?.strategies ?? [];
  const comparisonLabel = qualification?.comparisonBackend === "sequencer" ? "independent state" : "relay";
  return (
    <div className="panel" style={{padding: 10}}>
      <div className="panel-head">
        <span>
          strategy qualification
          {qualification?.comparisonBackend && (
            <span
              className="muted"
              style={{marginLeft: 8, fontSize: 10, textTransform: "uppercase", letterSpacing: "0.08em"}}
              title="the independent second opinion the accuracy numbers below are graded against"
            >
              backend: {qualification.comparisonBackend}
            </span>
          )}
        </span>
        <span className={qualification?.pass ? "pos" : "muted"}>
          {qualification
            ? `${qualification.elapsedHours}/${qualification.requiredHours}h · max gap ${qualification.maximumObservationGapSecs}s`
            : "waiting for bot"}
        </span>
      </div>
      <table className="grid">
        <thead>
          <tr>
            <th>strategy</th>
            <th>verdict</th>
            <th style={{textAlign: "right"}}>fork</th>
            <th style={{textAlign: "right"}}>{comparisonLabel}</th>
            <th style={{textAlign: "right"}}>actual</th>
            <th style={{textAlign: "right"}}>{comparisonLabel} accuracy</th>
            <th style={{textAlign: "right"}}>actual accuracy</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.strategy} title={row.reasons.join("; ")}>
              <td>{STRATEGY_LABEL[row.strategy] ?? row.strategy}</td>
              <td className={row.verdict === "PASS" ? "pos" : row.verdict === "FAIL" ? "neg" : "muted"}>
                {row.verdict}
              </td>
              <td style={{textAlign: "right"}}>{row.forkSamples}</td>
              <td style={{textAlign: "right"}}>{row.independentComparisons ?? row.relayComparisons}</td>
              <td style={{textAlign: "right"}}>{row.actualComparisons}</td>
              <td style={{textAlign: "right"}}>{(row.relayAccuracyBps / 100).toFixed(1)}%</td>
              <td style={{textAlign: "right"}}>{(row.actualAccuracyBps / 100).toFixed(1)}%</td>
            </tr>
          ))}
          {!rows.length && (
            <tr>
              <td colSpan={7} className="muted" style={{textAlign: "center", padding: 10}}>
                no qualification report yet
              </td>
            </tr>
          )}
        </tbody>
      </table>
      {(qualification?.reasons ?? []).map((reason) => (
        <div key={reason} className="muted" style={{fontSize: 10, marginTop: 4}}>
          • {reason}
        </div>
      ))}
    </div>
  );
}

/**
 * Return `prev` when it is structurally equal to `next`, so React can bail out
 * of the update and memoized children keep their previous render.
 *
 * The payloads here are small (≤250 rows) and already came off the wire as
 * JSON, so re-serialising is far cheaper than the cascade of re-renders it
 * prevents. This is deliberately a value comparison rather than a shallow one:
 * the arrays are rebuilt by `JSON.parse` every poll, so every element is a new
 * object and a shallow check would never match.
 */
function keepIfSame<T>(prev: T, next: T): T {
  try {
    return JSON.stringify(prev) === JSON.stringify(next) ? prev : next;
  } catch {
    return next;
  }
}

function safeHost(url: string): string {
  try {
    return new URL(url).hostname.replace("www.", "");
  } catch {
    return url;
  }
}

function HeadStat({label, value, tone}: {label: string; value: string; tone?: string}) {
  return (
    <div style={{display: "flex", flexDirection: "column"}}>
      <span className="muted" style={{fontSize: 9, textTransform: "uppercase", letterSpacing: "0.08em"}}>
        {label}
      </span>
      <span className={tone}>{value}</span>
    </div>
  );
}

function Card({title, value, sub, tone}: {title: string; value: string; sub?: string; tone?: string}) {
  return (
    <div className="panel" style={{padding: "10px 12px"}}>
      <div className="muted" style={{fontSize: 10, textTransform: "uppercase", letterSpacing: "0.08em"}}>
        {title}
      </div>
      <div className={tone} style={{fontSize: 20, marginTop: 4}}>
        {value}
      </div>
      {sub && (
        <div className="muted" style={{fontSize: 10, marginTop: 2}}>
          {sub}
        </div>
      )}
    </div>
  );
}

const selectStyle: React.CSSProperties = {
  background: "#070b11",
  border: "1px solid #1b2532",
  borderRadius: 4,
  color: "#d7e2f0",
  fontFamily: "inherit",
  fontSize: 11,
  padding: "2px 6px",
};
