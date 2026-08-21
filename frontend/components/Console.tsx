"use client";

import {useCallback, useEffect, useMemo, useRef, useState} from "react";
import EquityChart from "./EquityChart";
import LiveFeed from "./LiveFeed";
import ContractPanel from "./ContractPanel";
import RiskPanel from "./RiskPanel";
import FunnelPanel from "./FunnelPanel";
import RelayBlocksPanel from "./RelayBlocksPanel";
import Phase1Panel from "./Phase1Panel";
import ModeSwitch from "./ModeSwitch";
import WalletButton from "./WalletButton";
import DeployPanel from "./DeployPanel";
import type {
  CompetitionResponse,
  FeedEvent,
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

const FEED_MAX = 400;
const POLL_MS = 4000;

export default function Console() {
  const [status, setStatus] = useState<StatusResponse | null>(null);
  const [pnl, setPnl] = useState<PnlResponse | null>(null);
  const [series, setSeries] = useState<SeriesPoint[]>([]);
  const [sims, setSims] = useState<SimulationRow[]>([]);
  const [opps, setOpps] = useState<OpportunityRow[]>([]);
  const [bids, setBids] = useState<RelayBid[]>([]);
  const [competition, setCompetition] = useState<CompetitionResponse | null>(null);
  const [reorgs, setReorgs] = useState<ReorgRow[]>([]);
  const [events, setEvents] = useState<FeedEvent[]>([]);
  const [feedFilter, setFeedFilter] = useState("all");
  const [strategyFilter, setStrategyFilter] = useState("all");
  const [connected, setConnected] = useState(false);
  const esRef = useRef<EventSource | null>(null);

  const load = useCallback(async () => {
    const get = async <T,>(p: string, fallback: T): Promise<T> => {
      try {
        const r = await fetch(`/api/bot/${p}`, {cache: "no-store"});
        return (await r.json()) as T;
      } catch {
        return fallback;
      }
    };
    const [s, p, se, si, op, rb, comp, rg] = await Promise.all([
      get<StatusResponse | null>("status", null),
      get<PnlResponse | null>("pnl", null),
      get<SeriesPoint[]>("pnl/series?limit=250", []),
      get<SimulationRow[]>("simulations?limit=120", []),
      get<OpportunityRow[]>("opportunities?limit=60", []),
      get<RelayBid[]>("relay-bids?limit=25", []),
      get<CompetitionResponse | null>("competition?limit=25", null),
      get<ReorgRow[]>("reorgs?limit=15", []),
    ]);
    if (s) setStatus(s);
    if (p) setPnl(p);
    setSeries(Array.isArray(se) ? se : []);
    setSims(Array.isArray(si) ? si : []);
    setOpps(Array.isArray(op) ? op : []);
    setBids(Array.isArray(rb) ? rb : []);
    if (comp) setCompetition(comp);
    setReorgs(Array.isArray(rg) ? rg : []);
  }, []);

  useEffect(() => {
    load();
    const t = setInterval(load, POLL_MS);
    return () => clearInterval(t);
  }, [load]);

  useEffect(() => {
    const es = new EventSource("/api/bot/stream");
    esRef.current = es;
    es.onopen = () => setConnected(true);
    es.onerror = () => setConnected(false);
    es.onmessage = (m) => {
      try {
        const ev = JSON.parse(m.data) as FeedEvent;
        setEvents((prev) => [ev, ...prev].slice(0, FEED_MAX));
      } catch {
        /* ignore malformed frames */
      }
    };
    return () => es.close();
  }, []);

  const demo = Boolean(status?.demo);
  const chainId = status?.chain.id;
  const totalNet = pnl?.totalNetWei ?? 0;
  const filteredSims = useMemo(
    () => (strategyFilter === "all" ? sims : sims.filter((s) => s.strategy === strategyFilter)),
    [sims, strategyFilter]
  );

  const winRate = useMemo(() => {
    const rows = pnl?.byStrategy ?? [];
    const w = rows.reduce((a, r) => a + r.wins, 0);
    const n = rows.reduce((a, r) => a + r.simulations, 0);
    return n ? (100 * w) / n : 0;
  }, [pnl]);

  return (
    <main style={{padding: 12, display: "grid", gap: 12, maxWidth: 1800, margin: "0 auto"}}>
      {/* header */}
      <header className="panel" style={{display: "flex", alignItems: "center", gap: 16, padding: "10px 14px", flexWrap: "wrap"}}>
        <div style={{fontSize: 15, letterSpacing: "0.06em"}}>
          JERSEY<span style={{color: "#22d3ee"}}>MIKES</span>
          <span className="muted" style={{marginLeft: 8, fontSize: 11}}>MEV simulation console</span>
        </div>

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

      {/* stat cards */}
      <section style={{display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(180px, 1fr))", gap: 12}}>
        <Card
          title="simulated net P/L"
          value={`${signedEth(totalNet)} ETH`}
          tone={totalNet >= 0 ? "pos" : "neg"}
          sub="fork simulations only"
        />
        <Card title="win rate" value={`${winRate.toFixed(1)}%`} sub={`${pnl?.byStrategy.reduce((a, r) => a + r.simulations, 0) ?? 0} sims`} />
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
      <section style={{display: "grid", gridTemplateColumns: "minmax(0, 2fr) minmax(0, 1fr)", gap: 12}}>
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
                  <td style={{textAlign: "right"}} className={r.net_profit_wei >= 0 ? "pos" : "neg"}>
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
              {["all", "sandwich", "sandwich_v3", "jit", "atomic_arb", "liquidation", "sniper"].map((k) => (
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
                      <td style={{textAlign: "right"}} className={s.netWei >= 0 ? "pos" : "neg"}>
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
                        {s.success ? "profitable" : s.revertReason ?? "no edge"}
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

      {/* opportunities + relay */}
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

      {/* bloXroute Max Profit relay — delivered blocks + their transactions */}
      <RelayBlocksPanel chainId={chainId} />

      {/* strategy funnel — answers "why no opportunities?" with data */}
      <FunnelPanel
        funnel={status?.stats.funnel ?? null}
        funnelReplay={status?.stats.funnelReplay ?? null}
        pendingSeen={status?.stats.pendingSeen ?? 0}
        hintsSeen={status?.stats.hintsSeen ?? 0}
        startedAtMs={status?.stats.startedAtMs}
      />

      {/* risk & strategy controls */}
      <section className="panel" style={{padding: 12}}>
        <div className="panel-head" style={{marginBottom: 12}}>
          <span>Risk Management & Searcher Tuning</span>
          <span className="muted">Live Parameters & Diagnostics</span>
        </div>
        <RiskPanel status={status} />
      </section>

      {/* go-live: executor deployment checklist */}
      <section className="panel">
        <div className="panel-head">
          <span>Go live — deploy MevExecutor to mainnet</span>
          <span className="muted">6-step checklist · deploy first, arm much later · docs/GO_LIVE.md</span>
        </div>
        <DeployPanel chainId={chainId} />
      </section>

      {/* contract */}
      <section className="panel">
        <div className="panel-head">
          <span>MevExecutor — on-chain control</span>
          <span className="muted">{status ? shortHash(status.executor, 8) : "—"}</span>
        </div>
        <ContractPanel executor={status?.executor ?? ""} chainId={chainId} />
      </section>

      <footer className="muted" style={{padding: "4px 2px 20px", fontSize: 11}}>
        Simulation-only build. The bot reads live mainnet data and scores bundles against a forked EVM; nothing is
        broadcast. Risk parameters start deliberately liberal — see <code>docs/RISK.md</code>.
      </footer>
    </main>
  );
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
