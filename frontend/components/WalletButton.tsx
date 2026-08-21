"use client";

/**
 * Header wallet control.
 *
 * "Fully functional" means: EIP-6963 multi-wallet picker when several wallets
 * are installed, silent eager-reconnect on reload, live account/chain/balance
 * state, a one-click switch to the console's chain when the wallet sits on
 * another network, an explorer link for the connected address, and a real
 * disconnect (permission revoke + local state clear).
 */

import {useEffect, useRef, useState} from "react";
import {useWallet} from "@/lib/wallet";
import {addressUrl, explorerName} from "@/lib/explorer";
import {shortHash} from "@/lib/format";

const CHAIN_NAMES: Record<number, string> = {
  1: "ethereum",
  8453: "base",
  42161: "arbitrum",
  10: "optimism",
  137: "polygon",
  56: "bsc",
  43114: "avalanche",
  11155111: "sepolia",
};

export default function WalletButton({expectedChainId}: {expectedChainId?: number}) {
  const {providers, address, chainId, balanceWei, connecting, error, activeName, connect, disconnect, switchChain} =
    useWallet();
  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", close);
    return () => document.removeEventListener("mousedown", close);
  }, [open]);

  const wrongChain = address !== null && expectedChainId !== undefined && chainId !== null && chainId !== expectedChainId;
  const explorerAddr = addressUrl(expectedChainId ?? chainId ?? undefined, address);

  if (!address) {
    return (
      <div ref={wrapRef} style={{position: "relative"}}>
        <button
          onClick={() => (providers.length > 1 ? setOpen((o) => !o) : connect())}
          disabled={connecting}
          style={btnStyle}
          title={providers.length ? `${providers.length} wallet(s) detected` : "no injected wallet detected"}
        >
          {connecting ? "connecting…" : "connect wallet"}
        </button>
        {error && (
          <div className="muted" style={{fontSize: 10, maxWidth: 220, textAlign: "right"}}>
            {error}
          </div>
        )}
        {open && providers.length > 1 && (
          <div style={menuStyle}>
            <div className="muted" style={{fontSize: 10, padding: "4px 10px", textTransform: "uppercase"}}>
              pick a wallet
            </div>
            {providers.map((p) => (
              <button key={p.uuid} onClick={() => (connect(p), setOpen(false))} style={menuItemStyle}>
                {p.icon ? (
                  // eslint-disable-next-line @next/next/no-img-element
                  <img src={p.icon} alt="" width={16} height={16} style={{borderRadius: 3}} />
                ) : (
                  <span style={{width: 16, display: "inline-block", textAlign: "center"}}>◈</span>
                )}
                <span>{p.name}</span>
              </button>
            ))}
          </div>
        )}
      </div>
    );
  }

  return (
    <div ref={wrapRef} style={{position: "relative", display: "flex", gap: 8, alignItems: "center"}}>
      <span className="badge" style={{color: "#22d3ee"}} title={activeName ?? undefined}>
        <span className="dot" style={{background: "#22d3ee"}} /> {shortHash(address, 6)}
      </span>
      <span className="muted" style={{fontSize: 11}}>
        {balanceWei !== null ? `${(Number(balanceWei) / 1e18).toFixed(4)} ETH` : "—"}
      </span>
      <span className="badge" style={{color: wrongChain ? "#ff5c5c" : "#6b7c93"}}>
        {CHAIN_NAMES[chainId ?? -1] ?? (chainId !== null ? `chain ${chainId}` : "chain ?")}
      </span>
      {wrongChain && (
        <button
          onClick={() => void switchChain(expectedChainId!)}
          style={{...btnStyle, borderColor: "#ff5c5c", color: "#ff5c5c"}}
          title={`the console follows the bot's chain — switch your wallet to ${CHAIN_NAMES[expectedChainId!] ?? expectedChainId}`}
        >
          switch to {CHAIN_NAMES[expectedChainId!] ?? `chain ${expectedChainId}`}
        </button>
      )}
      <button onClick={() => setOpen((o) => !o)} style={btnStyle} title="wallet menu">
        ▾
      </button>
      {open && (
        <div style={menuStyle}>
          <a href={explorerAddr ?? "#"} target="_blank" rel="noreferrer" style={menuItemAnchorStyle}>
            view on {explorerName(expectedChainId ?? chainId ?? undefined)} ↗
          </a>
          <button onClick={() => void disconnect()} style={menuItemStyle}>
            disconnect
          </button>
        </div>
      )}
    </div>
  );
}

const btnStyle: React.CSSProperties = {
  background: "#111a25",
  border: "1px solid #24334a",
  borderRadius: 4,
  color: "#d7e2f0",
  padding: "4px 10px",
  cursor: "pointer",
  fontFamily: "inherit",
  fontSize: 12,
};

const menuStyle: React.CSSProperties = {
  position: "absolute",
  top: "110%",
  right: 0,
  zIndex: 50,
  background: "#0e141d",
  border: "1px solid #1b2532",
  borderRadius: 4,
  minWidth: 190,
  padding: 4,
  boxShadow: "0 8px 24px rgba(0,0,0,0.5)",
};

const menuItemStyle: React.CSSProperties = {
  display: "flex",
  gap: 8,
  alignItems: "center",
  width: "100%",
  background: "transparent",
  border: "none",
  color: "#d7e2f0",
  padding: "6px 10px",
  cursor: "pointer",
  fontFamily: "inherit",
  fontSize: 12,
  textAlign: "left",
};

const menuItemAnchorStyle: React.CSSProperties = {
  ...menuItemStyle,
  textDecoration: "none",
  display: "block",
};
