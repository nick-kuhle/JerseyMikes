"use client";
import {readActiveChain, withChain} from "@/lib/chain";

import {useCallback, useEffect, useState} from "react";
import type {RelayBlockRow, RelayBlockTxRow} from "@/lib/types";
import {shortHash, weiToEth} from "@/lib/format";
import {addressUrl, blockUrl, txUrl} from "@/lib/explorer";

/**
 * bloXroute Max Profit relay — delivered blocks and their transactions.
 *
 * The relay's `proposer_payload_delivered` data API tells us exactly which
 * blocks the winning builder delivered and how much they paid (`valueWei`).
 * The bot fetches each delivered block's transactions, stores them, and scores
 * them for extractable value; this panel is the read side of that data.
 * Every block number, transaction hash and recipient links to the explorer.
 */
export default function RelayBlocksPanel({chainId}: {chainId?: number}) {
  const [blocks, setBlocks] = useState<RelayBlockRow[]>([]);
  const [expanded, setExpanded] = useState<number | null>(null);
  const [txs, setTxs] = useState<RelayBlockTxRow[]>([]);
  const [loadingTxs, setLoadingTxs] = useState(false);

  const loadBlocks = useCallback(async () => {
    try {
      const r = await fetch(withChain("/api/bot/relay-blocks?limit=25", readActiveChain()), {cache: "no-store"});
      const rows = (await r.json()) as RelayBlockRow[];
      setBlocks(Array.isArray(rows) ? rows : []);
    } catch {
      /* bot unreachable; keep last data */
    }
  }, []);

  useEffect(() => {
    loadBlocks();
    const t = setInterval(loadBlocks, 5_000);
    return () => clearInterval(t);
  }, [loadBlocks]);

  useEffect(() => {
    if (expanded === null) {
      setTxs([]);
      return;
    }
    let cancelled = false;
    setLoadingTxs(true);
    (async () => {
      try {
        const r = await fetch(withChain(`/api/bot/relay-txs?blockNumber=${expanded}&limit=300`, readActiveChain()), {cache: "no-store"});
        const rows = (await r.json()) as RelayBlockTxRow[];
        if (!cancelled) setTxs(Array.isArray(rows) ? rows : []);
      } catch {
        if (!cancelled) setTxs([]);
      } finally {
        if (!cancelled) setLoadingTxs(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [expanded]);

  return (
    <div className="panel">
      <div className="panel-head">
        <span>bloXroute Max Profit relay — delivered blocks</span>
        <span className="muted">winning builder payment + landed transactions</span>
      </div>

      <div style={{maxHeight: 320, overflowY: "auto"}}>
        <table className="grid">
          <thead>
            <tr>
              <th>block</th>
              <th>slot</th>
              <th style={{textAlign: "right"}}>builder bid ETH</th>
              <th style={{textAlign: "right"}}>txs</th>
              <th style={{textAlign: "right"}}>gas used</th>
              <th>builder</th>
            </tr>
          </thead>
          <tbody>
            {!blocks.length && (
              <tr>
                <td colSpan={6} className="muted" style={{textAlign: "center", padding: 16}}>
                  no delivered blocks yet — the relay is polled once per block
                </td>
              </tr>
            )}
            {blocks.map((b) => {
              const open = expanded === b.blockNumber;
              return (
                <BlockRow
                  key={`${b.relay}-${b.slot}`}
                  block={b}
                  open={open}
                  chainId={chainId}
                  onToggle={() => setExpanded(open ? null : b.blockNumber)}
                />
              );
            })}
          </tbody>
        </table>
      </div>

      {expanded !== null && (
        <div style={{borderTop: "1px solid var(--line)", marginTop: 8, paddingTop: 8}}>
          <div className="muted" style={{fontSize: 10, marginBottom: 4}}>
            transactions in block #{expanded}
            {loadingTxs && " — loading…"}
          </div>
          <div style={{maxHeight: 260, overflowY: "auto"}}>
            <table className="grid">
              <thead>
                <tr>
                  <th>#</th>
                  <th>hash</th>
                  <th>to</th>
                  <th>selector</th>
                  <th style={{textAlign: "right"}}>value ETH</th>
                  <th style={{textAlign: "right"}}>gas</th>
                </tr>
              </thead>
              <tbody>
                {!txs.length && !loadingTxs && (
                  <tr>
                    <td colSpan={6} className="muted" style={{textAlign: "center", padding: 12}}>
                      no transactions stored for this block
                    </td>
                  </tr>
                )}
                {txs.map((t) => {
                  const hashLink = txUrl(chainId, t.hash);
                  const toLink = addressUrl(chainId, t.to);
                  return (
                    <tr key={`${t.hash}-${t.txIndex}`}>
                      <td className="muted">{t.txIndex}</td>
                      <td className="muted" title={t.hash}>
                        {hashLink ? (
                          <a
                            href={hashLink}
                            target="_blank"
                            rel="noreferrer"
                            title={`${t.hash} — view on the explorer`}
                            style={{color: "#22d3ee", textDecoration: "none"}}
                          >
                            {shortHash(t.hash, 10)} ↗
                          </a>
                        ) : (
                          shortHash(t.hash, 10)
                        )}
                      </td>
                      <td className="muted" title={t.to ?? undefined}>
                        {toLink ? (
                          <a
                            href={toLink}
                            target="_blank"
                            rel="noreferrer"
                            style={{color: undefined, textDecoration: "none"}}
                          >
                            {shortHash(t.to, 8)}
                          </a>
                        ) : (
                          shortHash(t.to, 8)
                        )}
                      </td>
                      <td className="muted">{t.selector ?? "—"}</td>
                      <td style={{textAlign: "right"}}>{weiToEth(t.valueWei, 4)}</td>
                      <td style={{textAlign: "right"}} className="muted">
                        {t.gas.toLocaleString()}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  );
}

function BlockRow({
  block,
  open,
  chainId,
  onToggle,
}: {
  block: RelayBlockRow;
  open: boolean;
  chainId?: number;
  onToggle: () => void;
}) {
  const url = blockUrl(chainId, block.blockNumber);
  return (
    <tr
      onClick={onToggle}
      style={{cursor: "pointer", background: open ? "var(--panel-2)" : undefined}}
      title="click to list the transactions that landed in this block"
    >
      <td>
        {url ? (
          <a
            href={url}
            target="_blank"
            rel="noreferrer"
            onClick={(e) => e.stopPropagation()}
            style={{color: "var(--cyan)"}}
            title="view this block on the explorer"
          >
            #{block.blockNumber} ↗
          </a>
        ) : (
          <span style={{color: "var(--cyan)"}}>#{block.blockNumber}</span>
        )}
      </td>
      <td className="muted">{block.slot}</td>
      <td style={{textAlign: "right"}} className="pos">
        {weiToEth(block.valueWei, 4)}
      </td>
      <td style={{textAlign: "right"}}>{block.numTx}</td>
      <td style={{textAlign: "right"}} className="muted">
        {(block.gasUsed / 1e6).toFixed(1)}M
      </td>
      <td className="muted">{shortHash(block.builder, 8)}</td>
    </tr>
  );
}
