"use client";

import {useCallback, useEffect, useState} from "react";
import {onChainChange, readActiveChain, setActiveChain} from "@/lib/chain";

interface Chain {
  slug: string;
  label: string;
}

/**
 * The multi-chain switcher (work order WS-F): Ethereum | Base pills in the
 * header. Clicking a chain selects it for every panel (persisted in
 * localStorage) and the Console remounts the panel tree on the new slug so
 * no panel can show another chain's data.
 *
 * Single-chain deployments render one **inert** pill labelled with the only
 * chain, so the console never leaves the operator guessing which chain
 * they're looking at (work order WS-H1). Only the pre-load flash is silent.
 */
export default function ChainSwitcher() {
  const [chains, setChains] = useState<Chain[]>([]);
  const [defaultSlug, setDefaultSlug] = useState<string>("");
  const [active, setActive] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);

  useEffect(() => {
    setActive(readActiveChain());
    const off = onChainChange(setActive);
    fetch("/api/chains", {cache: "no-store"})
      .then((r) => r.json())
      .then((d: {chains: Chain[]; default: string}) => {
        setChains(d.chains);
        setDefaultSlug(d.default);
        setLoaded(true);
      })
      .catch(() => setLoaded(true));
    return off;
  }, []);

  const select = useCallback(
    (slug: string) => {
      setActive(slug);
      setActiveChain(slug);
    },
    []
  );

  // Not loaded yet: stay silent for one frame so the pill doesn't flash a
  // stale label. Once loaded, always render *something* — the operator
  // must never wonder which chain they're staring at.
  if (!loaded) return null;

  // Single-chain deployment: one inert pill (no cursor, no click). The
  // hover title points to the CHAINS env var so operators discover how
  // to enable switching.
  if (chains.length < 2) {
    const only = chains[0];
    return (
      <div
        style={{display: "flex", gap: 2, background: "#070b11", border: "1px solid #1b2532", borderRadius: 6, padding: 2}}
        role="group"
        aria-label="chain"
      >
        <span
          className="badge live"
          style={{
            color: "#35d07f",
            font: "inherit",
            fontSize: 11,
            padding: "2px 10px",
            letterSpacing: "0.04em",
          }}
          title="single-chain console — set CHAINS on the frontend to enable switching"
        >
          {only?.label ?? "Ethereum"}
        </span>
      </div>
    );
  }

  return (
    <div
      style={{display: "flex", gap: 2, background: "#070b11", border: "1px solid #1b2532", borderRadius: 6, padding: 2}}
      role="tablist"
      aria-label="chain"
    >
      {chains.map((c) => {
        const isActive = (active ?? defaultSlug) === c.slug;
        return (
          <button
            key={c.slug}
            role="tab"
            aria-selected={isActive}
            onClick={() => select(c.slug)}
            className={isActive ? "badge live" : "badge"}
            style={{
              color: isActive ? "#35d07f" : "#8ba0bd",
              cursor: "pointer",
              font: "inherit",
              fontSize: 11,
              padding: "2px 10px",
              letterSpacing: "0.04em",
            }}
            title={`show ${c.label} data`}
          >
            {c.label}
          </button>
        );
      })}
    </div>
  );
}
