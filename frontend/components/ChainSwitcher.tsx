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
 * Single-chain deployments see one pill and the control stays inert.
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

  // Nothing to switch over (single chain or not loaded yet): stay invisible
  // rather than render one dead pill.
  if (!loaded || chains.length < 2) return null;

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
