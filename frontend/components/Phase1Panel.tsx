"use client";

import type {ActualMevResponse, CompetitionResponse, LatencySnapshot, ReorgRow} from "@/lib/types";

/**
 * Phase 1 instrumentation: latency histograms, competition ranking, re-orgs.
 */
export default function Phase1Panel({
  latency,
  competition,
  actualMev,
  reorgs,
}: {
  latency: LatencySnapshot | null | undefined;
  competition: CompetitionResponse | null | undefined;
  actualMev: ActualMevResponse | null | undefined;
  reorgs: ReorgRow[] | null | undefined;
}) {
  const total = latency?.stages?.total;
  const within = Boolean(latency?.withinBudget);
  const p95 = total?.p95Ms ?? 0;
  const summary = competition?.summary;
  const tp = summary?.truePositives ?? 0;
  const fp = summary?.falsePositives ?? 0;
  const tpr = tp + fp > 0 ? tp / (tp + fp) : 0;

  return (
    <section style={{display: "grid", gridTemplateColumns: "minmax(0, 1.2fr) minmax(0, 1fr) minmax(0, 0.8fr)", gap: 12}}>
      <div className="panel" style={{padding: 12}}>
        <div className="panel-head">
          <span>latency budget</span>
          <span className={within ? "pos" : "muted"}>{latency ? `${latency.budgetMs} ms p95 target` : "—"}</span>
        </div>
        <div style={{display: "flex", gap: 16, alignItems: "baseline", marginBottom: 8}}>
          <div>
            <div className="muted" style={{fontSize: 10, textTransform: "uppercase"}}>
              total p95
            </div>
            <div className={p95 > 0 && p95 <= (latency?.budgetMs ?? 150) ? "pos" : p95 ? "neg" : ""} style={{fontSize: 22}}>
              {total ? `${p95} ms` : "—"}
            </div>
          </div>
          <div className="muted" style={{fontSize: 11}}>
            p50 {total?.p50Ms ?? "—"} · p99 {total?.p99Ms ?? "—"} · n {total?.count ?? 0}
          </div>
        </div>
        <table className="grid">
          <thead>
            <tr>
              <th>stage</th>
              <th style={{textAlign: "right"}}>p50</th>
              <th style={{textAlign: "right"}}>p95</th>
              <th style={{textAlign: "right"}}>n</th>
            </tr>
          </thead>
          <tbody>
            {["ingest_to_strategy", "strategy", "risk", "simulation", "total"].map((k) => {
              const s = latency?.stages?.[k];
              return (
                <tr key={k}>
                  <td>{k.replaceAll("_", " ")}</td>
                  <td style={{textAlign: "right"}}>{s ? s.p50Ms : "—"}</td>
                  <td style={{textAlign: "right"}}>{s ? s.p95Ms : "—"}</td>
                  <td style={{textAlign: "right"}} className="muted">
                    {s ? s.count : 0}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      <div className="panel" style={{padding: 12}}>
        <div className="panel-head">
          <span>target & on-chain MEV evidence</span>
          <span className="muted">block bid is context, not a forecast</span>
        </div>
        <div style={{display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8, marginBottom: 10}}>
          <Stat label="target-observed rate" value={summary ? `${(100 * tpr).toFixed(0)}%` : "—"} />
          <Stat label="MEV matches" value={String(actualMev?.summary.matches ?? 0)} />
          <Stat label="high confidence" value={String(actualMev?.summary.highConfidence ?? 0)} />
          <Stat label="block-bid rank only" value={String(summary?.wouldOutbid ?? 0)} />
        </div>
        <div style={{maxHeight: 180, overflowY: "auto"}}>
          <table className="grid">
            <thead>
              <tr>
                <th>block</th>
                <th>strategy</th>
                <th style={{textAlign: "right"}}>p</th>
                <th>flag</th>
              </tr>
            </thead>
            <tbody>
              {(competition?.recent ?? []).slice(0, 12).map((r) => (
                <tr key={`${r.opportunityId}-${r.blockNumber}`}>
                  <td className="muted">{r.blockNumber}</td>
                  <td>{r.strategy}</td>
                  <td style={{textAlign: "right"}}>{r.inclusionP.toFixed(2)}</td>
                  <td className={r.truePositive ? "pos" : r.falsePositive ? "neg" : "muted"}>
                    {r.truePositive ? "TARGET" : r.falsePositive ? "MISSING" : r.wouldOutbid ? "BID>" : "—"}
                  </td>
                </tr>
              ))}
              {!(competition?.recent ?? []).length && (
                <tr>
                  <td colSpan={4} className="muted" style={{textAlign: "center", padding: 12}}>
                    no target observations yet — wait for a delivered block
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
        {(actualMev?.matches ?? []).slice(0, 3).map((match) => (
          <div key={match.opportunityId} style={{fontSize: 10, marginTop: 5}}>
            <span className={match.confidence === "high" ? "pos" : "muted"}>{match.confidence}</span>{" "}
            #{match.blockNumber} · {match.mevTxHashes.length} MEV tx · net {match.netWethWei.slice(0, 10)} wei
          </div>
        ))}
      </div>

      <div className="panel" style={{padding: 12}}>
        <div className="panel-head">
          <span>re-orgs</span>
          <span className="muted">{reorgs?.length ?? 0} logged</span>
        </div>
        <table className="grid">
          <thead>
            <tr>
              <th>range</th>
              <th>depth</th>
            </tr>
          </thead>
          <tbody>
            {(reorgs ?? []).map((r, i) => (
              <tr key={`${r.fromBlock}-${i}`}>
                <td>
                  #{r.fromBlock}
                  {r.toBlock !== r.fromBlock ? `–${r.toBlock}` : ""}
                </td>
                <td>{r.depth}</td>
              </tr>
            ))}
            {!(reorgs ?? []).length && (
              <tr>
                <td colSpan={2} className="muted" style={{textAlign: "center", padding: 12}}>
                  none this run
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function Stat({label, value}: {label: string; value: string}) {
  return (
    <div style={{background: "var(--panel-2, #0d131c)", borderRadius: 4, padding: "6px 8px"}}>
      <div className="muted" style={{fontSize: 10, textTransform: "uppercase"}}>
        {label}
      </div>
      <div style={{fontSize: 16}}>{value}</div>
    </div>
  );
}
