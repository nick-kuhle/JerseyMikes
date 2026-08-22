"use client";

import {useState, useEffect} from "react";
import type {StatusResponse, Strategy} from "@/lib/types";
import {formatEther, parseEther} from "viem";
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

export default function RiskPanel({status}: Props) {
  const [minProfitEth, setMinProfitEth] = useState("0.002");
  const [bribeBps, setBribeBps] = useState(9000);
  const [maxBaseFeeGwei, setMaxBaseFeeGwei] = useState(250);
  const [maxPositionEth, setMaxPositionEth] = useState(100);
  const [maxGas, setMaxGas] = useState(3000000);
  const [enabledStrats, setEnabledStrats] = useState<Record<Strategy, boolean>>({
    sandwich: true,
    sandwich_v3: true,
    jit: true,
    atomic_arb: true,
    liquidation: true,
    liquidation_compound: true,
    liquidation_morpho: true,
    liquidation_maker: true,
    oracle_frontrun: true,
    sniper: true,
  });
  const [copied, setCopied] = useState(false);
  const [activeTab, setActiveTab] = useState<"controls" | "diagnostics" | "sources">("controls");

  // Sync initial state from live status if available
  useEffect(() => {
    if (status?.risk) {
      const rawWei = BigInt(status.risk.minNetProfitWei || "1");
      if (rawWei > 1000000n) {
        setMinProfitEth(formatEther(rawWei));
      }
      setBribeBps(status.risk.bribeBps || 9000);
      setMaxBaseFeeGwei(Math.round(Number(status.risk.maxBaseFeeWei) / 1e9) || 250);
      setMaxPositionEth(Math.round(Number(formatEther(BigInt(status.risk.maxPositionWei || "100000000000000000000")))) || 100);
    }
    if (status?.strategies) {
      const current = status.strategies;
      setEnabledStrats({
        sandwich: current.includes("sandwich"),
        sandwich_v3: current.includes("sandwich_v3"),
        jit: current.includes("jit"),
        atomic_arb: current.includes("atomic_arb"),
        liquidation: current.includes("liquidation"),
        liquidation_compound: current.includes("liquidation_compound"),
        liquidation_morpho: current.includes("liquidation_morpho"),
        liquidation_maker: current.includes("liquidation_maker"),
        oracle_frontrun: current.includes("oracle_frontrun"),
        sniper: current.includes("sniper"),
      });
    }
  }, [status]);

  const toggleStrat = (s: Strategy) => {
    setEnabledStrats((prev) => ({...prev, [s]: !prev[s]}));
  };

  const getMinProfitWei = () => {
    try {
      return parseEther(minProfitEth || "0").toString();
    } catch {
      return "1";
    }
  };

  const generateEnvSnippet = () => {
    return `# ─────────────────────────────────────────────────────────────
# TUNED RISK & STRATEGY SETTINGS
# ─────────────────────────────────────────────────────────────
MIN_NET_PROFIT_WEI=${getMinProfitWei()} # (${minProfitEth} ETH)
MAX_POSITION_WEI=${parseEther(String(maxPositionEth)).toString()} # (${maxPositionEth} ETH)
MAX_BASE_FEE_WEI=${maxBaseFeeGwei}000000000 # (${maxBaseFeeGwei} gwei)
BRIBE_BPS=${bribeBps} # (${(bribeBps / 100).toFixed(1)}% to builder)
MAX_GAS_PER_BUNDLE=${maxGas}

STRATEGY_SANDWICH=${enabledStrats.sandwich}
STRATEGY_SANDWICH_V3=${enabledStrats.sandwich_v3}
STRATEGY_JIT=${enabledStrats.jit}
STRATEGY_ATOMIC_ARB=${enabledStrats.atomic_arb}
STRATEGY_LIQUIDATION=${enabledStrats.liquidation}
STRATEGY_LIQUIDATION_COMPOUND=${enabledStrats.liquidation_compound}
STRATEGY_LIQUIDATION_MORPHO=${enabledStrats.liquidation_morpho}
STRATEGY_LIQUIDATION_MAKER=${enabledStrats.liquidation_maker}
STRATEGY_ORACLE_FRONTRUN=${enabledStrats.oracle_frontrun}
STRATEGY_SNIPER=${enabledStrats.sniper}`;
  };

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

      {/* Tab 1: Controls */}
      {activeTab === "controls" && (
        <div style={{display: "grid", gap: 16}}>
          <div style={{display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(280px, 1fr))", gap: 14}}>
            {/* Min Net Profit Filter */}
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
              <div style={{display: "flex", alignItems: "center", gap: 8}}>
                <input
                  type="text"
                  value={minProfitEth}
                  onChange={(e) => setMinProfitEth(e.target.value)}
                  style={{...inputStyle, width: "100%"}}
                  placeholder="Custom ETH amount..."
                />
                <span className="muted" style={{fontSize: 11}}>ETH</span>
              </div>
            </div>

            {/* Builder Bribe */}
            <div className="panel" style={{padding: 12}}>
              <div style={{display: "flex", justifyContent: "space-between", marginBottom: 6}}>
                <span style={{fontSize: 11, textTransform: "uppercase", color: "var(--muted)"}}>Builder Bribe Auction</span>
                <span style={{color: "var(--cyan)", fontWeight: "bold"}}>{(bribeBps / 100).toFixed(1)}% ({bribeBps} BPS)</span>
              </div>
              <p className="muted" style={{fontSize: 11, margin: "4px 0 10px"}}>
                Percentage of gross profit paid to block.coinbase to win the bundle auction.
              </p>
              <input
                type="range"
                min="5000"
                max="9900"
                step="100"
                value={bribeBps}
                onChange={(e) => setBribeBps(Number(e.target.value))}
                style={{width: "100%", accentColor: "var(--cyan)", cursor: "pointer"}}
              />
              <div style={{display: "flex", justifyContent: "space-between", fontSize: 10, color: "var(--muted)", marginTop: 6}}>
                <span>50% (Low priority)</span>
                <span style={{color: "var(--cyan)"}}>90% (Industry standard)</span>
                <span>99% (Aggressive)</span>
              </div>
            </div>

            {/* Gas & Position Caps */}
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

            {/* Max Notional */}
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
                    onClick={() => setMaxPositionEth(eth)}
                    style={{
                      ...btnStyle,
                      background: maxPositionEth === eth ? "var(--panel-2)" : "var(--panel)",
                      borderColor: maxPositionEth === eth ? "var(--green)" : "var(--line)",
                      fontSize: 10,
                    }}
                  >
                    {eth} ETH
                  </button>
                ))}
              </div>
            </div>
          </div>

          {/* Strategy Toggles */}
          <div className="panel" style={{padding: 12}}>
            <div style={{fontSize: 11, textTransform: "uppercase", color: "var(--muted)", marginBottom: 8}}>
              Strategy Toggles
            </div>
            <div style={{display: "flex", gap: 10, flexWrap: "wrap"}}>
              {ALL_STRATEGIES.map((s) => {
                const active = enabledStrats[s];
                return (
                  <button
                    key={s}
                    onClick={() => toggleStrat(s)}
                    style={{
                      ...btnStyle,
                      display: "flex",
                      alignItems: "center",
                      gap: 8,
                      padding: "6px 12px",
                      background: active ? "#0f1c29" : "#080c12",
                      borderColor: active ? STRATEGY_COLOR[s] : "var(--line)",
                    }}
                  >
                    <span
                      style={{
                        width: 8,
                        height: 8,
                        borderRadius: "50%",
                        background: active ? STRATEGY_COLOR[s] : "#444",
                      }}
                    />
                    <span style={{color: active ? "#fff" : "var(--muted)"}}>
                      {STRATEGY_LABEL[s] || s}
                    </span>
                    <span style={{fontSize: 10, color: active ? "var(--green)" : "var(--red)"}}>
                      {active ? "ON" : "OFF"}
                    </span>
                  </button>
                );
              })}
            </div>
          </div>

          {/* Export / Copy Config */}
          <div className="panel" style={{padding: 12}}>
            <div style={{display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 8}}>
              <span style={{fontSize: 11, textTransform: "uppercase", color: "var(--muted)"}}>
                Generated .env Config Snippet
              </span>
              <button onClick={handleCopy} style={{...btnStyle, borderColor: "var(--cyan)", color: "var(--cyan)"}}>
                {copied ? "✓ Copied to Clipboard!" : "📋 Copy to .env"}
              </button>
            </div>
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
