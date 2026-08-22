"use client";

/**
 * Risk envelope controls — instant-apply.
 *
 * Every change POSTs to /api/risk and gates the very next opportunity the
 * engine considers (the risk engine, the fork simulator's minProfit/bribe
 * guards and the signed-bundle gas cap all read the same runtime envelope).
 * No restart, no .env round-trip. The .env snippet is still generated — but
 * demoted to what it always really was: persisting the *current* values as
 * boot defaults.
 *
 * Strategy toggles can only narrow (a strategy not constructed at boot
 * cannot be summoned at runtime — the bot refuses with the restart
 * instructions, same shape as the live-mode switch).
 */

import {useCallback, useEffect, useRef, useState} from "react";
import {parseEther} from "viem";
import type {RiskStateResponse, RiskValues, Strategy, StatusResponse} from "@/lib/types";
import {STRATEGY_COLOR, STRATEGY_LABEL} from "@/lib/format";

interface Props {
  status: StatusResponse | null;
}

const ALL_STRATEGIES: Strategy[] = [
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
];

const DEFAULTS: RiskValues = {
  minNetProfitWei: "1",
  maxPositionWei: "100000000000000000000",
  maxBaseFeeWei: "500000000000",
  maxDrawdownWei: "0",
  bribeBps: 9000,
  maxGasPerBundle: 3000000,
  maxInflightPerStrategy: 32,
};

type ApplyState =
  | {kind: "idle"}
  | {kind: "applying"}
  | {kind: "applied"; at: string; demo?: boolean}
  | {kind: "error"; message: string};

export default function RiskPanel({status}: Props) {
  const [activeTab, setActiveTab] = useState<"controls" | "diagnostics" | "sources">("controls");

  // The live form. Seeded from /api/risk once, then edited locally; every
  // edit schedules a debounced POST of the full numeric patch (idempotent).
  const [values, setValues] = useState<RiskValues>(DEFAULTS);
  const [strategyRows, setStrategyRows] = useState<RiskStateResponse["strategies"]>(
    ALL_STRATEGIES.map((name) => ({name, enabled: true, bootEnabled: true})),
  );
  const [killSwitch, setKillSwitch] = useState<RiskStateResponse["killSwitch"]>({
    tripped: false,
    cumulativeNetWei: "0",
  });
  const [apply, setApply] = useState<ApplyState>({kind: "idle"});
  const [demo, setDemo] = useState(false);
  const [loaded, setLoaded] = useState(false);

  // Derived display state (ETH / gwei units for the inputs).
  const [minProfitEth, setMinProfitEth] = useState("0.000000000000000001");
  const [maxPositionEth, setMaxPositionEth] = useState("100");
  const [maxBaseFeeGwei, setMaxBaseFeeGwei] = useState(500);
  const [maxGas, setMaxGas] = useState(3000000);
  const [maxInflight, setMaxInflight] = useState(32);

  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const skipFirst = useRef(true);

  const seed = useCallback((v: RiskValues) => {
    setValues(v);
    try {
      setMinProfitEth(formatMinimalEther(BigInt(v.minNetProfitWei)));
    } catch {
      /* keep previous */
    }
    setMaxPositionEth(String(Number(BigInt(v.maxPositionWei)) / 1e18));
    setMaxBaseFeeGwei(Math.max(1, Math.round(Number(BigInt(v.maxBaseFeeWei)) / 1e9)));
    setMaxGas(v.maxGasPerBundle);
    setMaxInflight(v.maxInflightPerStrategy);
  }, []);

  const load = useCallback(async () => {
    try {
      const res = await fetch("/api/bot/risk", {cache: "no-store"});
      const data = (await res.json()) as RiskStateResponse & {demo?: boolean};
      setDemo(Boolean(data.demo));
      if (data.effective) seed(data.effective);
      if (data.strategies) setStrategyRows(data.strategies);
      if (data.killSwitch) setKillSwitch(data.killSwitch);
    } catch {
      /* bot unreachable — the demo flag on /api/status data covers this */
    } finally {
      setLoaded(true);
    }
  }, [seed]);

  useEffect(() => {
    void load();
  }, [load]);

  // Keep the kill-switch badge in sync with the polled status.
  useEffect(() => {
    if (status?.risk) {
      setKillSwitch((k) => ({...k, tripped: status.risk.killSwitchTripped}));
    }
  }, [status?.risk?.killSwitchTripped]);

  const pushPatch = useCallback(
    (patch: Record<string, unknown>) => {
      setApply({kind: "applying"});
      fetch("/api/bot/risk", {
        method: "POST",
        headers: {"content-type": "application/json"},
        body: JSON.stringify(patch),
      })
        .then(async (res) => {
          const data = (await res.json().catch(() => ({}))) as Record<string, unknown>;
          if (!res.ok || data.ok === false) {
            const message = typeof data.error === "string" ? data.error : `HTTP ${res.status}`;
            setApply({kind: "error", message});
            void load(); // revert the form to the authoritative state
            return;
          }
          const at = new Date().toLocaleTimeString("en-US", {hour12: false});
          setApply({kind: "applied", at, demo: Boolean(data.demo)});
          if (data.effective) seed(data.effective as RiskValues);
          if (data.strategies) {
            setStrategyRows((rows) =>
              rows.map((r) => ({
                ...r,
                enabled: (data.strategies as string[]).includes(r.name),
              })),
            );
          }
        })
        .catch(() => setApply({kind: "error", message: "network error — is the bot up?"}));
    },
    [load, seed],
  );

  // Debounced full-patch push on any numeric edit.
  useEffect(() => {
    if (skipFirst.current) {
      skipFirst.current = false;
      return;
    }
    if (!loaded) return;
    if (timer.current) clearTimeout(timer.current);
    setApply({kind: "applying"});
    timer.current = setTimeout(() => {
      let minWei = values.minNetProfitWei;
      try {
        minWei = parseEther(minProfitEth || "0").toString();
      } catch {
        /* invalid input: keep the last valid value */
      }
      let posWei = values.maxPositionWei;
      try {
        posWei = parseEther(String(maxPositionEth || "0")).toString();
      } catch {
        /* keep */
      }
      const patch = {
        minNetProfitWei: minWei,
        maxPositionWei: posWei,
        maxBaseFeeWei: String(BigInt(maxBaseFeeGwei) * 1_000_000_000n),
        maxDrawdownWei: values.maxDrawdownWei,
        bribeBps: values.bribeBps,
        maxGasPerBundle: maxGas,
        maxInflightPerStrategy: maxInflight,
      };
      pushPatch(patch);
    }, 500);
    return () => {
      if (timer.current) clearTimeout(timer.current);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [minProfitEth, maxPositionEth, maxBaseFeeGwei, maxGas, maxInflight, values.bribeBps, values.maxDrawdownWei]);

  const toggleStrat = (s: Strategy) => {
    const row = strategyRows.find((r) => r.name === s);
    if (!row) return;
    const next = !row.enabled;
    setStrategyRows((rows) => rows.map((r) => (r.name === s ? {...r, enabled: next} : r)));
    pushPatch({strategies: {[s]: next}});
  };

  const resetKillSwitch = () => {
    fetch("/api/bot/risk/reset", {method: "POST", headers: {"content-type": "application/json"}, body: "{}"})
      .then(() => {
        setKillSwitch({tripped: false, cumulativeNetWei: "0"});
        setApply({kind: "applied", at: new Date().toLocaleTimeString("en-US", {hour12: false})});
      })
      .catch(() => setApply({kind: "error", message: "network error"}));
  };

  const generateEnvSnippet = () => {
    const enabled = (s: Strategy) => strategyRows.find((r) => r.name === s)?.enabled ?? false;
    return `# ─────────────────────────────────────────────────────────────
# TUNED RISK & STRATEGY SETTINGS (persist current values as boot defaults)
# ─────────────────────────────────────────────────────────────
MIN_NET_PROFIT_WEI=${values.minNetProfitWei}
MAX_POSITION_WEI=${values.maxPositionWei}
MAX_BASE_FEE_WEI=${BigInt(maxBaseFeeGwei) * 1_000_000_000n}
BRIBE_BPS=${values.bribeBps}
MAX_GAS_PER_BUNDLE=${maxGas}
MAX_INFLIGHT_PER_STRATEGY=${maxInflight}

STRATEGY_SANDWICH=${enabled("sandwich")}
STRATEGY_SANDWICH_V3=${enabled("sandwich_v3")}
STRATEGY_JIT=${enabled("jit")}
STRATEGY_ATOMIC_ARB=${enabled("atomic_arb")}
STRATEGY_LIQUIDATION=${enabled("liquidation")}
STRATEGY_LIQUIDATION_COMPOUND=${enabled("liquidation_compound")}
STRATEGY_LIQUIDATION_MORPHO=${enabled("liquidation_morpho")}
STRATEGY_LIQUIDATION_MAKER=${enabled("liquidation_maker")}
STRATEGY_ORACLE_FRONTRUN=${enabled("oracle_frontrun")}
STRATEGY_SNIPER=${enabled("sniper")}`;
  };

  const [copied, setCopied] = useState(false);
  const handleCopy = () => {
    navigator.clipboard.writeText(generateEnvSnippet());
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };

  return (
    <div style={{display: "grid", gap: 12}}>
      {/* Tabs */}
      <div style={{display: "flex", gap: 8, borderBottom: "1px solid var(--line)", paddingBottom: 8}}>
        <button
          onClick={() => setActiveTab("controls")}
          style={{...tabBtnStyle, color: activeTab === "controls" ? "var(--cyan)" : "var(--muted)", borderColor: activeTab === "controls" ? "var(--cyan)" : "transparent"}}
        >
          ⚙️ Risk & Profit Controls
        </button>
        <button
          onClick={() => setActiveTab("diagnostics")}
          style={{...tabBtnStyle, color: activeTab === "diagnostics" ? "var(--amber)" : "var(--muted)", borderColor: activeTab === "diagnostics" ? "var(--amber)" : "transparent"}}
        >
          🔍 Simulation Diagnostics (Why 0 Sims?)
        </button>
        <button
          onClick={() => setActiveTab("sources")}
          style={{...tabBtnStyle, color: activeTab === "sources" ? "var(--green)" : "var(--muted)", borderColor: activeTab === "sources" ? "var(--green)" : "transparent"}}
        >
          🌐 Add L2 Chains & Private Feeds
        </button>
      </div>

      {/* Tab 1: Controls — instant apply */}
      {activeTab === "controls" && (
        <div style={{display: "grid", gap: 16}}>
          {/* apply status line */}
          <div style={{display: "flex", justifyContent: "space-between", alignItems: "center", flexWrap: "wrap", gap: 8}}>
            <span className="muted" style={{fontSize: 11}}>
              Changes apply <strong style={{color: "var(--cyan)"}}>instantly</strong> — the next opportunity is gated
              with these values. No restart.
              {demo && <span className="badge" style={{marginLeft: 8, color: "var(--amber)"}}>DEMO DATA</span>}
            </span>
            {apply.kind === "applying" && (
              <span className="muted" style={{fontSize: 11}}>● applying…</span>
            )}
            {apply.kind === "applied" && (
              <span style={{fontSize: 11, color: "var(--green)"}}>✓ applied {apply.at}{apply.demo ? " (demo)" : ""}</span>
            )}
            {apply.kind === "error" && (
              <span style={{fontSize: 11, color: "var(--red)"}}>✗ {apply.message}</span>
            )}
          </div>

          <div style={{display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(280px, 1fr))", gap: 14}}>
            {/* Min Net Profit */}
            <div className="panel" style={{padding: 12}}>
              <div style={{display: "flex", justifyContent: "space-between", marginBottom: 6}}>
                <span style={{fontSize: 11, textTransform: "uppercase", color: "var(--muted)"}}>Minimum Net Profit</span>
                <span className="pos" style={{fontWeight: "bold"}}>{minProfitEth} ETH</span>
              </div>
              <p className="muted" style={{fontSize: 11, margin: "4px 0 10px"}}>
                Rejects trades whose post-gas simulated profit falls below this floor.
              </p>
              <div style={{display: "flex", gap: 6, flexWrap: "wrap", marginBottom: 8}}>
                {[
                  {label: "1 wei (Permissive)", val: "0.000000000000000001"},
                  {label: "0.001 ETH", val: "0.001"},
                  {label: "0.002 ETH (Rec)", val: "0.002"},
                  {label: "0.005 ETH", val: "0.005"},
                  {label: "0.010 ETH", val: "0.01"},
                ].map((p) => (
                  <button
                    key={p.label}
                    onClick={() => setMinProfitEth(p.val)}
                    style={{
                      ...btnStyle,
                      background: minProfitEth === p.val ? "var(--panel-2)" : "var(--panel)",
                      borderColor: minProfitEth === p.val ? "var(--cyan)" : "var(--line)",
                      fontSize: 10,
                    }}
                  >
                    {p.label}
                  </button>
                ))}
              </div>
              <input
                type="text"
                value={minProfitEth}
                onChange={(e) => setMinProfitEth(e.target.value)}
                style={{...inputStyle, width: "100%"}}
                placeholder="Custom ETH amount..."
              />
            </div>

            {/* Builder Bribe */}
            <div className="panel" style={{padding: 12}}>
              <div style={{display: "flex", justifyContent: "space-between", marginBottom: 6}}>
                <span style={{fontSize: 11, textTransform: "uppercase", color: "var(--muted)"}}>Builder Bribe Auction</span>
                <span style={{color: "var(--cyan)", fontWeight: "bold"}}>{(values.bribeBps / 100).toFixed(1)}% ({values.bribeBps} BPS)</span>
              </div>
              <p className="muted" style={{fontSize: 11, margin: "4px 0 10px"}}>
                Percentage of gross profit paid to block.coinbase to win the bundle auction.
              </p>
              <input
                type="range"
                min="5000"
                max="9900"
                step="100"
                value={values.bribeBps}
                onChange={(e) => setValues((v) => ({...v, bribeBps: Number(e.target.value)}))}
                style={{width: "100%", accentColor: "var(--cyan)", cursor: "pointer"}}
              />
              <div style={{display: "flex", justifyContent: "space-between", fontSize: 10, color: "var(--muted)", marginTop: 6}}>
                <span>50% (Low priority)</span>
                <span style={{color: "var(--cyan)"}}>90% (Industry standard)</span>
                <span>99% (Aggressive)</span>
              </div>
            </div>

            {/* Max Base Fee */}
            <div className="panel" style={{padding: 12}}>
              <div style={{display: "flex", justifyContent: "space-between", marginBottom: 6}}>
                <span style={{fontSize: 11, textTransform: "uppercase", color: "var(--muted)"}}>Max Base Fee Ceiling</span>
                <span style={{color: "var(--amber)", fontWeight: "bold"}}>{maxBaseFeeGwei} Gwei</span>
              </div>
              <p className="muted" style={{fontSize: 11, margin: "4px 0 10px"}}>
                Refuses to simulate or build bundles during network gas spikes.
              </p>
              <input
                type="range"
                min="20"
                max="500"
                step="10"
                value={maxBaseFeeGwei}
                onChange={(e) => setMaxBaseFeeGwei(Number(e.target.value))}
                style={{width: "100%", accentColor: "var(--amber)", cursor: "pointer"}}
              />
              <div style={{display: "flex", justifyContent: "space-between", fontSize: 10, color: "var(--muted)", marginTop: 6}}>
                <span>20 Gwei (Cheap)</span>
                <span>100 Gwei</span>
                <span>500 Gwei (Spike limit)</span>
              </div>
            </div>

            {/* Max Position */}
            <div className="panel" style={{padding: 12}}>
              <div style={{display: "flex", justifyContent: "space-between", marginBottom: 6}}>
                <span style={{fontSize: 11, textTransform: "uppercase", color: "var(--muted)"}}>Max Position Size</span>
                <span style={{color: "var(--green)", fontWeight: "bold"}}>{maxPositionEth} ETH</span>
              </div>
              <p className="muted" style={{fontSize: 11, margin: "4px 0 10px"}}>
                Caps the maximum borrowed/swapped capital per bundle.
              </p>
              <div style={{display: "flex", gap: 6, flexWrap: "wrap"}}>
                {[10, 25, 50, 100, 250].map((eth) => (
                  <button
                    key={eth}
                    onClick={() => setMaxPositionEth(String(eth))}
                    style={{
                      ...btnStyle,
                      background: maxPositionEth === String(eth) ? "var(--panel-2)" : "var(--panel)",
                      borderColor: maxPositionEth === String(eth) ? "var(--green)" : "var(--line)",
                      fontSize: 10,
                    }}
                  >
                    {eth} ETH
                  </button>
                ))}
              </div>
            </div>

            {/* Gas cap + inflight */}
            <div className="panel" style={{padding: 12}}>
              <div style={{display: "flex", justifyContent: "space-between", marginBottom: 6}}>
                <span style={{fontSize: 11, textTransform: "uppercase", color: "var(--muted)"}}>Gas Cap / Concurrency</span>
                <span style={{color: "var(--cyan)", fontWeight: "bold"}}>{(maxGas / 1_000_000).toFixed(1)}M · {maxInflight} inf</span>
              </div>
              <p className="muted" style={{fontSize: 11, margin: "4px 0 10px"}}>
                Per-bundle gas ceiling (the simulator clamps it below the block limit) and concurrent simulations per strategy.
              </p>
              <div style={{display: "flex", gap: 6, flexWrap: "wrap", marginBottom: 8}}>
                {[1_000_000, 2_000_000, 3_000_000, 5_000_000].map((g) => (
                  <button
                    key={g}
                    onClick={() => setMaxGas(g)}
                    style={{
                      ...btnStyle,
                      background: maxGas === g ? "var(--panel-2)" : "var(--panel)",
                      borderColor: maxGas === g ? "var(--cyan)" : "var(--line)",
                      fontSize: 10,
                    }}
                  >
                    {g / 1_000_000}M
                  </button>
                ))}
                {[8, 32, 64].map((n) => (
                  <button
                    key={n}
                    onClick={() => setMaxInflight(n)}
                    style={{
                      ...btnStyle,
                      background: maxInflight === n ? "var(--panel-2)" : "var(--panel)",
                      borderColor: maxInflight === n ? "var(--cyan)" : "var(--line)",
                      fontSize: 10,
                    }}
                  >
                    {n} inflight
                  </button>
                ))}
              </div>
              <div style={{display: "flex", gap: 8, alignItems: "center"}}>
                <input
                  type="number"
                  min={1}
                  max={256}
                  value={maxInflight}
                  onChange={(e) => setMaxInflight(Math.max(1, Math.min(256, Number(e.target.value) || 1)))}
                  style={{...inputStyle, width: 90}}
                />
                <span className="muted" style={{fontSize: 10}}>max inflight per strategy</span>
              </div>
            </div>

            {/* Kill switch */}
            <div className="panel" style={{padding: 12, borderColor: killSwitch.tripped ? "var(--red)" : "var(--line)"}}>
              <div style={{display: "flex", justifyContent: "space-between", marginBottom: 6}}>
                <span style={{fontSize: 11, textTransform: "uppercase", color: "var(--muted)"}}>Drawdown Kill Switch</span>
                <span style={{color: killSwitch.tripped ? "var(--red)" : "var(--green)", fontWeight: "bold"}}>
                  {killSwitch.tripped ? "TRIPPED" : "armed"}
                </span>
              </div>
              <p className="muted" style={{fontSize: 11, margin: "4px 0 10px"}}>
                Stops all new positions once cumulative simulated PnL drops below −maxDrawdown (0 disables). Cumulative:{" "}
                <strong>{(Number(BigInt(killSwitch.cumulativeNetWei || "0")) / 1e18).toFixed(4)} ETH</strong>
              </p>
              <button
                onClick={resetKillSwitch}
                disabled={!killSwitch.tripped}
                style={{
                  ...btnStyle,
                  borderColor: killSwitch.tripped ? "var(--red)" : "var(--line)",
                  color: killSwitch.tripped ? "var(--red)" : "var(--muted)",
                  cursor: killSwitch.tripped ? "pointer" : "not-allowed",
                }}
              >
                {killSwitch.tripped ? "Reset kill switch (re-arm)" : "Not tripped"}
              </button>
            </div>
          </div>

          {/* Strategy Toggles */}
          <div className="panel" style={{padding: 12}}>
            <div style={{fontSize: 11, textTransform: "uppercase", color: "var(--muted)", marginBottom: 8}}>
              Strategy Toggles — instant; can only narrow what booted on
            </div>
            <div style={{display: "flex", gap: 10, flexWrap: "wrap"}}>
              {strategyRows.map((row) => {
                const active = row.enabled;
                const bootLocked = !row.bootEnabled;
                return (
                  <button
                    key={row.name}
                    onClick={() => toggleStrat(row.name)}
                    disabled={bootLocked}
                    title={
                      bootLocked
                        ? `${row.name} was off at boot — set STRATEGY_${row.name.toUpperCase()}=true and restart to construct it`
                        : active
                          ? "Click to disable (applies instantly)"
                          : "Click to enable (applies instantly)"
                    }
                    style={{
                      ...btnStyle,
                      display: "flex",
                      alignItems: "center",
                      gap: 8,
                      padding: "6px 12px",
                      background: active ? "#0f1c29" : "#080c12",
                      borderColor: active ? STRATEGY_COLOR[row.name] : "var(--line)",
                      opacity: bootLocked ? 0.4 : 1,
                      cursor: bootLocked ? "not-allowed" : "pointer",
                    }}
                  >
                    <span
                      style={{
                        width: 8,
                        height: 8,
                        borderRadius: "50%",
                        background: active ? STRATEGY_COLOR[row.name] : "#444",
                      }}
                    />
                    <span style={{color: active ? "#fff" : "var(--muted)"}}>
                      {STRATEGY_LABEL[row.name] || row.name}
                    </span>
                    <span style={{fontSize: 10, color: active ? "var(--green)" : "var(--red)"}}>
                      {active ? "ON" : bootLocked ? "BOOT-OFF" : "OFF"}
                    </span>
                  </button>
                );
              })}
            </div>
          </div>

          {/* Persist as defaults */}
          <div className="panel" style={{padding: 12}}>
            <div style={{display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 8}}>
              <span style={{fontSize: 11, textTransform: "uppercase", color: "var(--muted)"}}>
                Persist current values as boot defaults (.env)
              </span>
              <button onClick={handleCopy} style={{...btnStyle, borderColor: "var(--cyan)", color: "var(--cyan)"}}>
                {copied ? "✓ Copied to Clipboard!" : "📋 Copy .env snippet"}
              </button>
            </div>
            <p className="muted" style={{fontSize: 11, margin: "0 0 8px"}}>
              Runtime changes live until the process exits. Paste this into <code>.env</code> to make the current
              values the defaults at the next boot.
            </p>
            <pre
              style={{
                background: "#040608",
                padding: "10px 12px",
                borderRadius: 4,
                border: "1px solid var(--line)",
                color: "#a5b4fc",
                fontSize: 11,
                overflowX: "auto",
                margin: 0,
              }}
            >
              {generateEnvSnippet()}
            </pre>
          </div>
        </div>
      )}

      {/* Tab 2: Diagnostics */}
      {activeTab === "diagnostics" && (
        <div className="panel" style={{padding: 14, display: "grid", gap: 12}}>
          <div style={{fontSize: 13, fontWeight: "bold", color: "var(--amber)"}}>
            Why are there no simulated transactions going through yet?
          </div>
          <p className="muted" style={{fontSize: 12, lineHeight: 1.6, margin: 0}}>
            If your bot is connected to the mempool but you haven't seen a simulated bundle land in the table yet, it is usually due to one of these 4 standard MEV dynamics:
          </p>

          <div style={{display: "grid", gap: 10}}>
            <div style={{background: "var(--panel-2)", border: "1px solid var(--line)", borderRadius: 4, padding: 10}}>
              <div style={{color: "var(--cyan)", fontWeight: "bold", marginBottom: 4}}>
                1. Private Orderflow (Flashbots Protect / MEV Blocker)
              </div>
              <div className="muted" style={{fontSize: 11, lineHeight: 1.5}}>
                Over 70% of large DeFi volume on Ethereum mainnet is routed through private RPCs. These transactions <strong>never enter the public mempool</strong> (`newPendingTransactions`). The bot receives public retail transactions, which are often too small to sandwich profitably after paying Ethereum L1 gas ($5–$15).
              </div>
            </div>

            <div style={{background: "var(--panel-2)", border: "1px solid var(--line)", borderRadius: 4, padding: 10}}>
              <div style={{color: "var(--cyan)", fontWeight: "bold", marginBottom: 4}}>
                2. Sizing Thresholds (e.g. JIT 20 WETH Floor)
              </div>
              <div className="muted" style={{fontSize: 11, lineHeight: 1.5}}>
                The JIT strategy has a safety filter requiring a minimum victim swap size of <strong>20 WETH (~$50,000)</strong>. Swaps of this size occur intermittently. Smaller swaps do not earn enough Uniswap V3 fees to cover the ~500,000 gas required to mint and burn the position.
              </div>
            </div>

            <div style={{background: "var(--panel-2)", border: "1px solid var(--line)", borderRadius: 4, padding: 10}}>
              <div style={{color: "var(--cyan)", fontWeight: "bold", marginBottom: 4}}>
                3. Raw Transaction Byte Hydration (`eth_getRawTransactionByHash`)
              </div>
              <div className="muted" style={{fontSize: 11, lineHeight: 1.5}}>
                To simulate a victim transaction inside the local Anvil fork, the bot must fetch the signed raw transaction bytes. Some free RPC endpoints (or overloaded tiers) drop `eth_getRawTransactionByHash` requests, which causes the bot to skip simulating that victim rather than guessing.
              </div>
            </div>

            <div style={{background: "var(--panel-2)", border: "1px solid var(--line)", borderRadius: 4, padding: 10}}>
              <div style={{color: "var(--cyan)", fontWeight: "bold", marginBottom: 4}}>
                4. V3 sandwich cache is empty
              </div>
              <div className="muted" style={{fontSize: 11, lineHeight: 1.5}}>
                `STRATEGY_SANDWICH_V3` and `POOL_DISCOVERY_V3` default on as a pair. The strategy
                only quotes pools already in the V3 cache, so a cold start (or discovery off)
                emits nothing until `PoolCreated` logs have been scanned. Watch the live
                `sandwich_v3` funnel row and `/api/latency` stage `strategy` p95.
              </div>
            </div>

            <div style={{background: "var(--panel-2)", border: "1px solid var(--line)", borderRadius: 4, padding: 10}}>
              <div style={{color: "var(--cyan)", fontWeight: "bold", marginBottom: 4}}>
                5. Arbitrage Efficiency on L1 Mainnet
              </div>
              <div className="muted" style={{fontSize: 11, lineHeight: 1.5}}>
                Major pools (WETH/USDC, WETH/USDT) on Uniswap and SushiSwap are kept tightly synchronized within 0.1% by institutional searchers. Since Uniswap V2 charges 0.3% fee per swap (0.6% round trip), pure cyclic arbitrage only triggers immediately after a large unbalanced swap lands.
              </div>
            </div>
          </div>
        </div>
      )}

      {/* Tab 3: Sources & L2s */}
      {activeTab === "sources" && (
        <div className="panel" style={{padding: 14, display: "grid", gap: 14}}>
          <div style={{fontSize: 13, fontWeight: "bold", color: "var(--green)"}}>
            How to Expand to Layer 2s (Base, Arbitrum) & Private Feeds
          </div>
          <p className="muted" style={{fontSize: 12, lineHeight: 1.6, margin: 0}}>
            Because L1 Ethereum gas is expensive, moving to Layer 2 chains or adding low-latency mempool feeds yields <strong>10x–50x more executable MEV opportunities</strong> with sub-cent gas fees.
          </p>

          <div style={{display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(320px, 1fr))", gap: 12}}>
            {/* Base Chain */}
            <div style={{background: "var(--panel-2)", border: "1px solid var(--line)", borderRadius: 4, padding: 12}}>
              <div style={{display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 6}}>
                <span style={{fontWeight: "bold", color: "#3b82f6"}}>🔵 Base (Chain ID 8453)</span>
                <span className="badge" style={{color: "var(--green)"}}>Highest Volume</span>
              </div>
              <p className="muted" style={{fontSize: 11, lineHeight: 1.5, margin: "0 0 8px"}}>
                Aerodrome & Uniswap V3 on Base have enormous retail swap flow with &lt;$0.01 gas costs.
              </p>
              <pre style={codeBoxStyle}>
{`# .env for Base
CHAIN_ID=8453
CHAIN_NAME=base
BLOCK_TIME_MS=2000
ETH_HTTP_URL=https://base-mainnet.g.alchemy.com/v2/KEY
ETH_WS_URL=wss://base-mainnet.g.alchemy.com/v2/KEY
WETH_ADDRESS=0x4200000000000000000000000000000000000006
USD_STABLE_ADDRESS=0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913`}
              </pre>
            </div>

            {/* Arbitrum */}
            <div style={{background: "var(--panel-2)", border: "1px solid var(--line)", borderRadius: 4, padding: 12}}>
              <div style={{display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 6}}>
                <span style={{fontWeight: "bold", color: "#0ea5e9"}}>🔷 Arbitrum One (Chain ID 42161)</span>
                <span className="badge" style={{color: "var(--cyan)"}}>Sequencer Feed</span>
              </div>
              <p className="muted" style={{fontSize: 11, lineHeight: 1.5, margin: "0 0 8px"}}>
                Connect to Arbitrum's Nitro sequencer websocket feed for sub-250ms pre-confirmations.
              </p>
              <pre style={codeBoxStyle}>
{`# .env for Arbitrum
CHAIN_ID=42161
CHAIN_NAME=arbitrum
BLOCK_TIME_MS=250
ETH_HTTP_URL=https://arb-mainnet.g.alchemy.com/v2/KEY
ETH_WS_URL=wss://arb-mainnet.g.alchemy.com/v2/KEY
SEQUENCER_FEED_URL=wss://arb1.arbitrum.io/feed`}
              </pre>
            </div>

            {/* Third-party Streams */}
            <div style={{background: "var(--panel-2)", border: "1px solid var(--line)", borderRadius: 4, padding: 12}}>
              <div style={{display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 6}}>
                <span style={{fontWeight: "bold", color: "var(--amber)"}}>⚡ Extra Mempool Streams</span>
                <span className="badge" style={{color: "var(--amber)"}}>Zero Latency</span>
              </div>
              <p className="muted" style={{fontSize: 11, lineHeight: 1.5, margin: "0 0 8px"}}>
                Add bloXroute BDN or Chainbound Fiber gateway websockets in `EXTRA_MEMPOOL_WS`.
              </p>
              <pre style={codeBoxStyle}>
{`# Add comma-separated gateway endpoints
EXTRA_MEMPOOL_WS=wss://api.blxrbdn.com/ws,...`}
              </pre>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

/** "0.000000000000000001" for 1 wei instead of "0.000000000000000001000000000000". */
function formatMinimalEther(wei: bigint): string {
  const whole = wei / 10n ** 18n;
  const frac = (wei % 10n ** 18n).toString().padStart(18, "0").replace(/0+$/, "");
  return frac ? `${whole}.${frac}` : `${whole}`;
}

const tabBtnStyle: React.CSSProperties = {
  background: "transparent",
  border: "none",
  borderBottom: "2px solid transparent",
  padding: "6px 12px",
  cursor: "pointer",
  fontFamily: "inherit",
  fontSize: 12,
  fontWeight: 600,
};

const inputStyle: React.CSSProperties = {
  background: "#070b11",
  border: "1px solid #1b2532",
  borderRadius: 4,
  color: "#d7e2f0",
  padding: "5px 8px",
  fontFamily: "inherit",
  fontSize: 12,
};

const btnStyle: React.CSSProperties = {
  background: "#111a25",
  border: "1px solid #24334a",
  borderRadius: 4,
  color: "#d7e2f0",
  padding: "4px 8px",
  cursor: "pointer",
  fontFamily: "inherit",
  fontSize: 11,
};

const codeBoxStyle: React.CSSProperties = {
  background: "#040608",
  padding: 8,
  borderRadius: 4,
  border: "1px solid #1a2330",
  fontSize: 10,
  color: "#94a3b8",
  margin: 0,
  overflowX: "auto",
};
