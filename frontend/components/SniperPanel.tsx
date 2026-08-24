"use client";

/**
 * Sniper — New Token Portfolio & Directional Execution Console.
 *
 * Cleaned up and unified for maximum performance and functionality:
 * 1. Master On/Off Switch & Sim/Live Mode Switch with emergency halt/resume.
 * 2. Strategy & Investment Parameters: initial investment (ETH), auto-sell take profit (% and absolute ETH),
 *    sell fraction %, stop loss %, trailing stop %, max hold duration, budgets, and safety filters.
 *    (Fixed: camelCase payload synchronization + unsaved edit protection).
 * 3. Unified Holdings & Positions: Combines active bot positions and wallet token holdings into one seamless portfolio.
 * 4. History: Clean log of closed positions, categorized by Simulation vs. Live Execution.
 * 5. Swapping & Selling: 1-click quick-sells (25%, 50%, 100%) and DEX Aggregators (1inch, Uniswap, KyberSwap, DexScreener).
 */

import {memo, useCallback, useEffect, useMemo, useRef, useState} from "react";
import {readActiveChain, withChain} from "@/lib/chain";
import {ago, shortHash, signedEth, weiToEth} from "@/lib/format";
import {addressUrl} from "@/lib/explorer";
import {useWallet} from "@/lib/wallet";
import {getAggregatorLinks, ERC20_ABI} from "@/lib/swap";
import {
  createPublicClient,
  formatEther,
  formatUnits,
  http,
  isAddress,
  parseEther,
  type Address,
} from "viem";
import {base, mainnet} from "viem/chains";
import type {
  SniperParams,
  SniperParamsPatch,
  SniperParamsResponse,
  SniperPortfolio,
} from "@/lib/types";

const EXIT_LABEL: Record<string, string> = {
  take_profit_pct: "Take Profit %",
  take_profit_abs: "Take Profit ETH",
  stop_loss: "Stop Loss",
  trailing_stop: "Trailing Stop",
  max_hold: "Max Hold Timeout",
  honeypot_detected: "Honeypot Detected",
  manual: "Manual Exit",
  risk_stop: "Risk Stop",
};

const STATE_COLOR: Record<string, string> = {
  pending: "var(--amber)",
  open: "var(--green)",
  scaling: "var(--cyan)",
  closed: "var(--muted)",
  abandoned: "var(--muted)",
};

function pnlColor(wei: string): string {
  let v: bigint;
  try {
    v = BigInt(wei || "0");
  } catch {
    return "var(--muted)";
  }
  if (v > 0n) return "var(--green)";
  if (v < 0n) return "var(--red)";
  return "var(--muted)";
}

function bpsFormatted(n: number): string {
  const pct = n / 100;
  return `${pct >= 0 ? "+" : ""}${pct.toFixed(1)}%`;
}

function ethFromWei(weiStr: string, decimals = 4): string {
  try {
    const val = formatEther(BigInt(weiStr || "0"));
    const num = parseFloat(val);
    return isNaN(num) ? "0.0000" : num.toFixed(decimals);
  } catch {
    return "0.0000";
  }
}

function weiFromEth(ethStr: string): string {
  try {
    const clean = ethStr.trim();
    if (!clean || isNaN(Number(clean))) return "0";
    return parseEther(clean).toString();
  } catch {
    return "0";
  }
}

// Preset configurations
const PRESETS = {
  conservative: {
    buySizeEth: "0.025",
    dailyBudgetEth: "0.1",
    takeProfitPct: 50,
    takeProfitAbsEth: "0",
    sellFractionPct: 100,
    stopLossPct: 25,
    trailingStopPct: 15,
    maxHoldMins: 30,
    minLiquidityEth: "3.0",
    maxPriceImpactPct: 2.0,
    maxTaxPct: 3.0,
    requireHoneypot: true,
  },
  balanced: {
    buySizeEth: "0.05",
    dailyBudgetEth: "0.25",
    takeProfitPct: 100,
    takeProfitAbsEth: "0",
    sellFractionPct: 100,
    stopLossPct: 50,
    trailingStopPct: 0,
    maxHoldMins: 45,
    minLiquidityEth: "2.0",
    maxPriceImpactPct: 3.0,
    maxTaxPct: 5.0,
    requireHoneypot: true,
  },
  moonshot: {
    buySizeEth: "0.1",
    dailyBudgetEth: "0.5",
    takeProfitPct: 300,
    takeProfitAbsEth: "0",
    sellFractionPct: 50,
    stopLossPct: 60,
    trailingStopPct: 20,
    maxHoldMins: 120,
    minLiquidityEth: "1.5",
    maxPriceImpactPct: 5.0,
    maxTaxPct: 8.0,
    requireHoneypot: true,
  },
};

function SniperPanel() {
  const [pf, setPf] = useState<SniperPortfolio | null>(null);
  const [cfg, setCfg] = useState<SniperParamsResponse | null>(null);
  const [demo, setDemo] = useState(false);
  const [tab, setTab] = useState<"portfolio" | "parameters" | "swap" | "history" | "gates">("portfolio");

  // Wallet and chain state
  const wallet = useWallet();
  const rawChainSlug = readActiveChain();
  const activeChainSlug = rawChainSlug || "ethereum";
  const currentChainId = activeChainSlug === "base" ? 8453 : 1;

  // Notification / Feedback state
  const [feedback, setFeedback] = useState<{type: "success" | "error" | "info"; msg: string} | null>(null);
  const [isSaving, setIsSaving] = useState(false);
  const [isHalting, setIsHalting] = useState(false);

  // Form dirty flag: prevents background polling from overwriting user edits while typing
  const isFormDirty = useRef(false);
  const formInitialized = useRef(false);

  // Editable parameter form state
  const [formBuySizeEth, setFormBuySizeEth] = useState("0.05");
  const [formDailyBudgetEth, setFormDailyBudgetEth] = useState("0.25");
  const [formTotalBudgetEth, setFormTotalBudgetEth] = useState("0");
  const [formTakeProfitPct, setFormTakeProfitPct] = useState("100");
  const [formTakeProfitAbsEth, setFormTakeProfitAbsEth] = useState("0");
  const [formSellFractionPct, setFormSellFractionPct] = useState("100");
  const [formStopLossPct, setFormStopLossPct] = useState("50");
  const [formTrailingStopPct, setFormTrailingStopPct] = useState("0");
  const [formMaxHoldMins, setFormMaxHoldMins] = useState("30");
  const [formMaxPositions, setFormMaxPositions] = useState("1");
  const [formMinLiquidityEth, setFormMinLiquidityEth] = useState("2.0");
  const [formMaxPriceImpactPct, setFormMaxPriceImpactPct] = useState("3.0");
  const [formMaxBuyTaxPct, setFormMaxBuyTaxPct] = useState("5.0");
  const [formMaxSellTaxPct, setFormMaxSellTaxPct] = useState("5.0");
  const [formMinHoldBlocks, setFormMinHoldBlocks] = useState("1");
  const [formRequireHoneypot, setFormRequireHoneypot] = useState(true);
  const [formRequireLpLocked, setFormRequireLpLocked] = useState(false);

  // Swap / Aggregator state
  const [swapTarget, setSwapTarget] = useState<{
    token: string;
    symbol: string;
    pair?: string;
    qty?: string;
    markEth?: string;
  } | null>(null);
  const [swapFraction, setSwapFraction] = useState<number>(100);
  const [customTokenInput, setCustomTokenInput] = useState("");
  const [walletTokens, setWalletTokens] = useState<Array<{address: string; symbol: string; balance: string; decimals: number}>>([]);
  const [isScanningWallet, setIsScanningWallet] = useState(false);

  // Sync form from server configuration
  const populateFormFromConfig = useCallback((params: SniperParams) => {
    setFormBuySizeEth(ethFromWei(params.buySizeWei, 4));
    setFormDailyBudgetEth(ethFromWei(params.dailyBudgetWei, 4));
    setFormTotalBudgetEth(params.totalBudgetWei === "0" ? "0" : ethFromWei(params.totalBudgetWei, 4));
    setFormTakeProfitPct(String(Math.round(params.takeProfitBps / 100)));
    setFormTakeProfitAbsEth(params.takeProfitAbsWei === "0" ? "0" : ethFromWei(params.takeProfitAbsWei, 4));
    setFormSellFractionPct(String(Math.round(params.sellFractionBps / 100)));
    setFormStopLossPct(String(Math.round(params.stopLossBps / 100)));
    setFormTrailingStopPct(String(Math.round(params.trailingStopBps / 100)));
    setFormMaxHoldMins(String(Math.round(params.maxHoldSecs / 60)));
    setFormMaxPositions(String(params.maxConcurrentPositions));
    setFormMinLiquidityEth(ethFromWei(params.minLiquidityWei, 2));
    setFormMaxPriceImpactPct((params.maxPriceImpactBps / 100).toFixed(1));
    setFormMaxBuyTaxPct((params.maxBuyTaxBps / 100).toFixed(1));
    setFormMaxSellTaxPct((params.maxSellTaxBps / 100).toFixed(1));
    setFormMinHoldBlocks(String(params.minHoldBlocks));
    setFormRequireHoneypot(params.requireHoneypotPass);
    setFormRequireLpLocked(params.requireLpLocked);
  }, []);

  const load = useCallback(async () => {
    const chain = readActiveChain();
    try {
      const [pRes, cRes] = await Promise.all([
        fetch(withChain("/api/bot/sniper/portfolio", chain), {cache: "no-store"}),
        fetch(withChain("/api/bot/sniper/params", chain), {cache: "no-store"}),
      ]);
      if (pRes.ok && cRes.ok) {
        const p = (await pRes.json()) as SniperPortfolio & {demo?: boolean};
        const c = (await cRes.json()) as SniperParamsResponse & {demo?: boolean};
        setPf(p);
        setCfg(c);
        setDemo(Boolean(p.demo || c.demo));

        // Populate form on initial load or if user is not actively editing
        if (c?.params && (!formInitialized.current || !isFormDirty.current)) {
          populateFormFromConfig(c.params);
          formInitialized.current = true;
        }
      }
    } catch {
      /* retain cached state on network blip */
    }
  }, [populateFormFromConfig]);

  useEffect(() => {
    load();
    const t = setInterval(load, 4000);
    return () => clearInterval(t);
  }, [load]);

  // Viem client for contract reads
  const publicClient = useMemo(() => {
    return createPublicClient({
      chain: activeChainSlug === "base" ? base : mainnet,
      transport: http(withChain("/api/eth", activeChainSlug)),
    });
  }, [activeChainSlug]);

  // Scan user wallet holdings for sniped tokens & custom search
  const scanWalletBalances = useCallback(async () => {
    if (!wallet.address || !publicClient) return;
    setIsScanningWallet(true);
    try {
      const tokensToScan = new Set<string>();
      if (pf?.open) {
        pf.open.forEach((pos) => tokensToScan.add(pos.token.toLowerCase()));
      }
      if (pf?.recentClosed) {
        pf.recentClosed.forEach((pos) => tokensToScan.add(pos.token.toLowerCase()));
      }
      if (customTokenInput && isAddress(customTokenInput)) {
        tokensToScan.add(customTokenInput.toLowerCase());
      }

      const results: Array<{address: string; symbol: string; balance: string; decimals: number}> = [];

      for (const tokenAddr of Array.from(tokensToScan)) {
        try {
          const [bal, dec, sym] = await Promise.all([
            publicClient.readContract({
              address: tokenAddr as Address,
              abi: ERC20_ABI,
              functionName: "balanceOf",
              args: [wallet.address as Address],
            }),
            publicClient.readContract({
              address: tokenAddr as Address,
              abi: ERC20_ABI,
              functionName: "decimals",
            }).catch(() => 18),
            publicClient.readContract({
              address: tokenAddr as Address,
              abi: ERC20_ABI,
              functionName: "symbol",
            }).catch(() => shortHash(tokenAddr, 4)),
          ]);

          if (bal > 0n) {
            results.push({
              address: tokenAddr,
              symbol: sym as string,
              balance: formatUnits(bal, dec as number),
              decimals: dec as number,
            });
          }
        } catch {
          // ignore read failure on unverified token
        }
      }
      setWalletTokens(results);
    } catch (e) {
      console.error("Wallet balance scan error:", e);
    } finally {
      setIsScanningWallet(false);
    }
  }, [wallet.address, publicClient, pf, customTokenInput]);

  useEffect(() => {
    if (wallet.address && tab === "portfolio") {
      scanWalletBalances();
    }
  }, [wallet.address, tab, scanWalletBalances]);

  // Patch parameters API call (Sends exact camelCase payload)
  const handleSaveParams = async (overridePatch?: Partial<SniperParamsPatch>) => {
    setIsSaving(true);
    setFeedback(null);
    const chain = readActiveChain();

    const patch: SniperParamsPatch = {
      enabled: overridePatch?.enabled !== undefined ? overridePatch.enabled : cfg?.params.enabled,
      buySizeWei: overridePatch?.buySizeWei ?? weiFromEth(formBuySizeEth),
      dailyBudgetWei: overridePatch?.dailyBudgetWei ?? weiFromEth(formDailyBudgetEth),
      totalBudgetWei: overridePatch?.totalBudgetWei ?? (formTotalBudgetEth === "0" ? "0" : weiFromEth(formTotalBudgetEth)),
      takeProfitBps: overridePatch?.takeProfitBps ?? Math.round(parseFloat(formTakeProfitPct || "100") * 100),
      takeProfitAbsWei: overridePatch?.takeProfitAbsWei ?? (formTakeProfitAbsEth === "0" ? "0" : weiFromEth(formTakeProfitAbsEth)),
      sellFractionBps: overridePatch?.sellFractionBps ?? Math.round(parseFloat(formSellFractionPct || "100") * 100),
      stopLossBps: overridePatch?.stopLossBps ?? Math.round(parseFloat(formStopLossPct || "0") * 100),
      trailingStopBps: overridePatch?.trailingStopBps ?? Math.round(parseFloat(formTrailingStopPct || "0") * 100),
      maxHoldSecs: overridePatch?.maxHoldSecs ?? Math.round(parseFloat(formMaxHoldMins || "0") * 60),
      maxConcurrentPositions: overridePatch?.maxConcurrentPositions ?? parseInt(formMaxPositions || "1", 10),
      minLiquidityWei: overridePatch?.minLiquidityWei ?? weiFromEth(formMinLiquidityEth),
      maxPriceImpactBps: overridePatch?.maxPriceImpactBps ?? Math.round(parseFloat(formMaxPriceImpactPct || "3") * 100),
      maxBuyTaxBps: overridePatch?.maxBuyTaxBps ?? Math.round(parseFloat(formMaxBuyTaxPct || "5") * 100),
      maxSellTaxBps: overridePatch?.maxSellTaxBps ?? Math.round(parseFloat(formMaxSellTaxPct || "5") * 100),
      minHoldBlocks: overridePatch?.minHoldBlocks ?? parseInt(formMinHoldBlocks || "1", 10),
      requireHoneypotPass: overridePatch?.requireHoneypotPass !== undefined ? overridePatch.requireHoneypotPass : formRequireHoneypot,
      requireLpLocked: overridePatch?.requireLpLocked !== undefined ? overridePatch.requireLpLocked : formRequireLpLocked,
    };

    try {
      const res = await fetch(withChain("/api/bot/sniper/params", chain), {
        method: "POST",
        headers: {"content-type": "application/json"},
        body: JSON.stringify(patch),
      });
      const data = await res.json();
      if (res.ok && data.ok) {
        setFeedback({type: "success", msg: "✓ Parameters saved and applied successfully!"});
        isFormDirty.current = false;
        if (data.params) {
          populateFormFromConfig(data.params);
        }
        await load();
      } else {
        const errorMsg = data.errors?.join("; ") || data.error || "Failed to update parameters";
        setFeedback({type: "error", msg: `Save failed: ${errorMsg}`});
      }
    } catch (err) {
      setFeedback({type: "error", msg: `Network error: ${(err as Error).message}`});
    } finally {
      setIsSaving(false);
    }
  };

  // Master On/Off Toggle
  const handleToggleMasterEnable = async () => {
    if (!cfg) return;
    const nextState = !cfg.params.enabled;
    await handleSaveParams({enabled: nextState});
  };

  // Emergency Halt / Resume Toggle
  const handleToggleHalt = async () => {
    if (!cfg) return;
    setIsHalting(true);
    setFeedback(null);
    const chain = readActiveChain();
    const endpoint = cfg.halted ? "/api/bot/sniper/resume" : "/api/bot/sniper/halt";
    try {
      const res = await fetch(withChain(endpoint, chain), {
        method: "POST",
        headers: {"content-type": "application/json"},
        body: JSON.stringify({reason: "operator action from console"}),
      });
      const data = await res.json();
      if (res.ok && data.ok) {
        setFeedback({
          type: "info",
          msg: cfg.halted ? "Sniper lane resumed!" : "Sniper lane halted (exits remain active).",
        });
        await load();
      } else {
        setFeedback({type: "error", msg: data.error || "Failed to toggle halt state"});
      }
    } catch (err) {
      setFeedback({type: "error", msg: `Error: ${(err as Error).message}`});
    } finally {
      setIsHalting(false);
    }
  };

  // Apply Preset Configuration
  const applyPreset = (key: keyof typeof PRESETS) => {
    const p = PRESETS[key];
    setFormBuySizeEth(p.buySizeEth);
    setFormDailyBudgetEth(p.dailyBudgetEth);
    setFormTakeProfitPct(String(p.takeProfitPct));
    setFormTakeProfitAbsEth(p.takeProfitAbsEth);
    setFormSellFractionPct(String(p.sellFractionPct));
    setFormStopLossPct(String(p.stopLossPct));
    setFormTrailingStopPct(String(p.trailingStopPct));
    setFormMaxHoldMins(String(p.maxHoldMins));
    setFormMinLiquidityEth(p.minLiquidityEth);
    setFormMaxPriceImpactPct(String(p.maxPriceImpactPct));
    setFormMaxBuyTaxPct(String(p.maxTaxPct));
    setFormMaxSellTaxPct(String(p.maxTaxPct));
    setFormRequireHoneypot(p.requireHoneypot);
    isFormDirty.current = true;
    setFeedback({type: "info", msg: `Loaded ${key.toUpperCase()} preset. Click 'Save & Apply Parameters' to save.`});
  };

  if (!pf || !cfg) {
    return (
      <div className="panel" style={{padding: 16}}>
        <div className="muted">Loading new token sniper console…</div>
      </div>
    );
  }

  const totals = pf.totals;
  const isEnabled = cfg.params.enabled;
  const isArmed = pf.armed;
  const isHalted = cfg.halted;
  const blockers = pf.armingBlockers ?? [];
  const hardBlockers = blockers.filter((b) => !b.startsWith("WARNING"));
  const warnings = blockers.filter((b) => b.startsWith("WARNING"));

  // Merge bot positions and wallet token balances into a unified list
  const unifiedHoldings = useMemo(() => {
    const map = new Map<
      string,
      {
        token: string;
        symbol: string;
        pair?: string;
        venue?: string;
        state?: string;
        entryCostWei: string;
        markValueWei: string;
        unrealizedPnlWei: string;
        netPnlBps: number;
        ageSecs: number;
        openedAtMs: number;
        markStale: boolean;
        inBot: boolean;
        inWallet: boolean;
        walletBalance?: string;
      }
    >();

    // Add bot open positions
    pf.open.forEach((pos) => {
      map.set(pos.token.toLowerCase(), {
        token: pos.token,
        symbol: pos.symbol || shortHash(pos.token, 4),
        pair: pos.pair,
        venue: pos.venue,
        state: pos.state,
        entryCostWei: pos.entryCostWei,
        markValueWei: pos.markValueWei,
        unrealizedPnlWei: pos.unrealizedPnlWei,
        netPnlBps: pos.netPnlBps,
        ageSecs: pos.ageSecs,
        openedAtMs: pos.openedAtMs,
        markStale: pos.markStale,
        inBot: true,
        inWallet: false,
      });
    });

    // Add/merge wallet holdings
    walletTokens.forEach((wt) => {
      const key = wt.address.toLowerCase();
      const existing = map.get(key);
      if (existing) {
        existing.inWallet = true;
        existing.walletBalance = wt.balance;
      } else {
        map.set(key, {
          token: wt.address,
          symbol: wt.symbol,
          entryCostWei: "0",
          markValueWei: "0",
          unrealizedPnlWei: "0",
          netPnlBps: 0,
          ageSecs: 0,
          openedAtMs: Date.now(),
          markStale: false,
          inBot: false,
          inWallet: true,
          walletBalance: wt.balance,
        });
      }
    });

    return Array.from(map.values());
  }, [pf.open, walletTokens]);

  return (
    <div style={{display: "grid", gap: 12}}>
      {/* ── Control Header: Master Switch, Mode Indicator, Emergency Halt & Status ── */}
      <div
        className="panel"
        style={{
          padding: "12px 16px",
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          flexWrap: "wrap",
          gap: 12,
          background: isArmed ? "rgba(34, 197, 94, 0.05)" : "var(--panel)",
          borderColor: isArmed ? "rgba(34, 197, 94, 0.4)" : "var(--line)",
        }}
      >
        <div style={{display: "flex", alignItems: "center", gap: 12, flexWrap: "wrap"}}>
          {/* Master ON / OFF Toggle */}
          <div style={{display: "flex", alignItems: "center", gap: 6}}>
            <span className="muted" style={{fontSize: 11, fontWeight: 700, textTransform: "uppercase"}}>
              SNIPER:
            </span>
            <button
              onClick={handleToggleMasterEnable}
              disabled={isSaving}
              style={{
                background: isEnabled ? "var(--green)" : "#1e293b",
                color: isEnabled ? "#05240f" : "var(--muted)",
                fontWeight: 700,
                fontSize: 11,
                padding: "5px 12px",
                borderRadius: 4,
                border: "1px solid",
                borderColor: isEnabled ? "var(--green)" : "var(--line)",
                cursor: "pointer",
                transition: "all 0.15s ease",
              }}
            >
              {isEnabled ? "● ENABLED (ON)" : "○ DISABLED (OFF)"}
            </button>
          </div>

          {/* SIM / LIVE Mode Badge */}
          <div style={{display: "flex", alignItems: "center", gap: 6}}>
            <span
              className="badge"
              style={{
                background: isArmed ? "rgba(34,197,94,0.15)" : "rgba(107,124,147,0.15)",
                color: isArmed ? "var(--green)" : "var(--muted)",
                borderColor: isArmed ? "var(--green)" : "var(--line)",
                padding: "4px 8px",
                fontSize: 11,
                fontWeight: 600,
              }}
              title={
                isArmed
                  ? "Armed for real trades through SniperVault"
                  : "Shadow / Simulation mode: launches monitored without broadcast"
              }
            >
              {isArmed ? "LIVE TRADING ARMED" : "SIMULATION (SHADOW MODE)"}
            </span>
            {demo && (
              <span className="badge" style={{color: "var(--amber)"}}>
                DEMO DATA
              </span>
            )}
          </div>

          {/* Emergency Halt / Resume Button */}
          <button
            onClick={handleToggleHalt}
            disabled={isHalting}
            style={{
              background: isHalted ? "rgba(245, 181, 68, 0.15)" : "rgba(255, 92, 92, 0.12)",
              color: isHalted ? "var(--amber)" : "var(--red)",
              border: `1px solid ${isHalted ? "var(--amber)" : "rgba(255, 92, 92, 0.4)"}`,
              borderRadius: 4,
              padding: "4px 10px",
              fontSize: 11,
              fontWeight: 600,
              cursor: "pointer",
            }}
          >
            {isHalted ? "▶ RESUME SNIPER" : "⏸ EMERGENCY HALT"}
          </button>
        </div>

        {/* Quick Active Parameters in Header */}
        <div style={{display: "flex", alignItems: "center", gap: 14, fontSize: 11}}>
          <div>
            <span className="muted">BUY SIZE: </span>
            <strong>{ethFromWei(cfg.params.buySizeWei, 3)} Ξ</strong>
          </div>
          <div>
            <span className="muted">TP / SL: </span>
            <strong style={{color: "var(--green)"}}>+{(cfg.params.takeProfitBps / 100).toFixed(0)}%</strong>
            {" / "}
            <strong style={{color: "var(--red)"}}>-{(cfg.params.stopLossBps / 100).toFixed(0)}%</strong>
          </div>
          <div>
            <span className="muted">DAILY BUDGET: </span>
            <strong>{ethFromWei(cfg.params.dailyBudgetWei, 2)} Ξ</strong>
          </div>
        </div>
      </div>

      {/* ── Status Feedback Banner ── */}
      {feedback && (
        <div
          style={{
            padding: "8px 12px",
            borderRadius: 4,
            fontSize: 11,
            display: "flex",
            justifyContent: "space-between",
            alignItems: "center",
            background:
              feedback.type === "success"
                ? "rgba(34, 197, 94, 0.15)"
                : feedback.type === "error"
                  ? "rgba(255, 92, 92, 0.15)"
                  : "rgba(34, 211, 238, 0.15)",
            border: `1px solid ${
              feedback.type === "success"
                ? "var(--green)"
                : feedback.type === "error"
                  ? "var(--red)"
                  : "var(--cyan)"
            }`,
            color:
              feedback.type === "success"
                ? "var(--green)"
                : feedback.type === "error"
                  ? "var(--red)"
                  : "var(--cyan)",
          }}
        >
          <span>{feedback.msg}</span>
          <button
            onClick={() => setFeedback(null)}
            style={{background: "none", border: "none", color: "inherit", cursor: "pointer", fontSize: 13}}
          >
            ×
          </button>
        </div>
      )}

      {/* ── Hard Blockers or Warnings Banner ── */}
      {(hardBlockers.length > 0 || isHalted || warnings.length > 0) && (
        <div
          style={{
            border: `1px solid ${isHalted ? "var(--red)" : "var(--line)"}`,
            background: isHalted ? "rgba(255, 92, 92, 0.08)" : "var(--panel-2)",
            borderRadius: 4,
            padding: "8px 12px",
            fontSize: 11,
          }}
        >
          {isHalted && (
            <div style={{color: "var(--red)", fontWeight: 700, marginBottom: 4}}>
              ⚠ SNIPER LANE HALTED: {cfg.haltReason || "Stopped by operator"}
            </div>
          )}
          {hardBlockers.length > 0 && (
            <div>
              <span className="muted" style={{fontWeight: 600}}>
                Arming Blockers (Set in Parameters tab to enable live buys):
              </span>
              <ul style={{margin: "4px 0 0", paddingLeft: 18, color: "var(--muted)"}}>
                {hardBlockers.map((b) => (
                  <li key={b}>{b}</li>
                ))}
              </ul>
            </div>
          )}
          {warnings.map((w) => (
            <div key={w} style={{marginTop: 4, color: "var(--amber)"}}>
              {w}
            </div>
          ))}
        </div>
      )}

      {/* ── Mini Portfolio Summary Metrics Strip ── */}
      <div
        className="panel"
        style={{
          display: "grid",
          gridTemplateColumns: "repeat(auto-fit, minmax(130px, 1fr))",
          gap: 10,
          padding: "12px 16px",
        }}
      >
        <MetricCard label="Active Holdings" value={`${unifiedHoldings.length}`} />
        <MetricCard
          label="Active Held Cost"
          value={`${ethFromWei(totals.openCostWei, 4)} Ξ`}
          title="Initial capital currently committed to active positions"
        />
        <MetricCard
          label="Active Mark Value"
          value={`${ethFromWei(totals.openValueWei, 4)} Ξ`}
          tone={totals.anyMarkStale ? "amber" : undefined}
          title="Current mark-to-market valuation"
        />
        <MetricCard
          label="Unrealized PnL"
          value={`${signedEth(totals.unrealizedPnlWei, 4)} Ξ`}
          tone={BigInt(totals.unrealizedPnlWei || "0") > 0n ? "pos" : BigInt(totals.unrealizedPnlWei || "0") < 0n ? "neg" : "muted"}
          title="Paper gain/loss on currently held tokens"
        />
        <MetricCard
          label="Realized PnL"
          value={`${signedEth(totals.realizedPnlWei, 4)} Ξ`}
          tone={BigInt(totals.realizedPnlWei || "0") > 0n ? "pos" : BigInt(totals.realizedPnlWei || "0") < 0n ? "neg" : "muted"}
          title="Net realized profit after gas and fees"
        />
        <MetricCard
          label="Win Rate"
          value={totals.wins + totals.losses === 0 ? "—" : `${(totals.winRateBps / 100).toFixed(0)}%`}
          sub={`${totals.wins}W / ${totals.losses}L`}
        />
        <MetricCard
          label="24h Deployed"
          value={`${ethFromWei(totals.deployedTodayWei, 3)} / ${ethFromWei(cfg.params.dailyBudgetWei, 3)} Ξ`}
          title="Capital deployed in rolling 24h window vs daily limit"
        />
      </div>

      {/* ── Sub-Navigation Tabs ── */}
      <div style={{display: "flex", gap: 8, borderBottom: "1px solid var(--line)", paddingBottom: 6, flexWrap: "wrap"}}>
        {[
          {id: "portfolio", label: `📊 Holdings & Positions (${unifiedHoldings.length})`},
          {id: "parameters", label: "⚙️ Strategy & Investment Parameters"},
          {id: "swap", label: "⚡ Instant Sell & DEX Aggregators"},
          {id: "history", label: `📜 Trade History (${pf.recentClosed.length})`},
          {id: "gates", label: "🛡️ Gate Logs & Honeypots"},
        ].map((t) => (
          <button
            key={t.id}
            onClick={() => setTab(t.id as typeof tab)}
            style={{
              padding: "6px 14px",
              borderRadius: 4,
              fontSize: 12,
              fontWeight: tab === t.id ? 700 : 500,
              background: tab === t.id ? "var(--panel-2)" : "transparent",
              border: "1px solid",
              borderColor: tab === t.id ? "var(--cyan)" : "transparent",
              color: tab === t.id ? "var(--cyan)" : "var(--text)",
              cursor: "pointer",
            }}
          >
            {t.label}
          </button>
        ))}
      </div>

      {/* ────────────────────────────────────────────────────────────────────────
          TAB 1: UNIFIED HOLDINGS & ACTIVE POSITIONS
          ──────────────────────────────────────────────────────────────────────── */}
      {tab === "portfolio" && (
        <div className="panel" style={{padding: 14, display: "grid", gap: 12}}>
          {/* Top Search & Wallet Status Bar */}
          <div style={{display: "flex", justifyContent: "space-between", alignItems: "center", flexWrap: "wrap", gap: 8}}>
            <div style={{display: "flex", gap: 8, alignItems: "center", flex: 1, maxWidth: 500}}>
              <input
                value={customTokenInput}
                onChange={(e) => setCustomTokenInput(e.target.value)}
                placeholder="Scan / Import any token address (0x...)..."
                style={{
                  flex: 1,
                  background: "#070b11",
                  border: "1px solid var(--line)",
                  color: "var(--text)",
                  padding: "5px 8px",
                  fontSize: 11,
                  borderRadius: 3,
                }}
              />
              <button
                onClick={() => scanWalletBalances()}
                disabled={isScanningWallet || !wallet.address}
                style={{
                  padding: "5px 10px",
                  fontSize: 11,
                  background: "var(--panel-2)",
                  border: "1px solid var(--line)",
                  color: "var(--cyan)",
                  borderRadius: 3,
                  cursor: "pointer",
                }}
              >
                {isScanningWallet ? "Scanning..." : "Scan Token"}
              </button>
            </div>

            <div style={{fontSize: 11, color: "var(--muted)"}}>
              {wallet.address ? (
                <span>
                  Connected Wallet: <strong style={{color: "var(--cyan)"}}>{shortHash(wallet.address, 6)}</strong>
                </span>
              ) : (
                <button
                  onClick={() => wallet.connect()}
                  style={{
                    padding: "3px 8px",
                    background: "var(--panel-2)",
                    border: "1px solid var(--cyan)",
                    color: "var(--cyan)",
                    borderRadius: 3,
                    fontSize: 10,
                    cursor: "pointer",
                  }}
                >
                  Connect Wallet to View Balances
                </button>
              )}
            </div>
          </div>

          {/* Unified Holdings Table */}
          {unifiedHoldings.length === 0 ? (
            <div className="muted" style={{padding: "28px 0", textAlign: "center", fontSize: 12}}>
              No active holdings found.
              <br />
              <span style={{fontSize: 11, color: "var(--muted)"}}>
                When the sniper enters a launch or your wallet holds sniped tokens, they will appear here with live PnL and 1-click aggregator sell buttons.
              </span>
            </div>
          ) : (
            <div style={{overflowX: "auto"}}>
              <table className="grid" style={{width: "100%", fontSize: 12}}>
                <thead>
                  <tr>
                    <th>TOKEN</th>
                    <th>SOURCE / TYPE</th>
                    <th style={{textAlign: "right"}}>ENTRY (ETH)</th>
                    <th style={{textAlign: "right"}}>CURRENT MARK</th>
                    <th style={{textAlign: "right"}}>UNREALIZED PNL</th>
                    <th style={{textAlign: "right"}}>BALANCE</th>
                    <th style={{textAlign: "center"}}>QUICK ACTIONS (SELL / SWAP)</th>
                  </tr>
                </thead>
                <tbody>
                  {unifiedHoldings.map((h) => {
                    const aggLinks = getAggregatorLinks(h.token, currentChainId, activeChainSlug);
                    const isPos = BigInt(h.unrealizedPnlWei || "0") > 0n;
                    const isNeg = BigInt(h.unrealizedPnlWei || "0") < 0n;
                    return (
                      <tr key={h.token} style={{opacity: h.markStale ? 0.7 : 1}}>
                        <td>
                          <div style={{display: "flex", alignItems: "center", gap: 6}}>
                            <span
                              style={{
                                width: 7,
                                height: 7,
                                borderRadius: "50%",
                                background: h.state ? STATE_COLOR[h.state] : "var(--cyan)",
                              }}
                            />
                            <strong>{h.symbol}</strong>
                            <a
                              href={addressUrl(currentChainId, h.token) || undefined}
                              target="_blank"
                              rel="noreferrer"
                              className="muted"
                              style={{fontSize: 10, textDecoration: "none"}}
                              title="View on Explorer"
                            >
                              ↗
                            </a>
                          </div>
                          <div className="muted" style={{fontSize: 10}}>
                            {shortHash(h.token, 6)}
                          </div>
                        </td>

                        <td>
                          <div style={{display: "flex", gap: 4}}>
                            {h.inBot && (
                              <span className="badge" style={{fontSize: 9, color: "var(--green)"}}>
                                🎯 Sniper Position
                              </span>
                            )}
                            {h.inWallet && (
                              <span className="badge" style={{fontSize: 9, color: "var(--cyan)"}}>
                                💼 In Wallet
                              </span>
                            )}
                          </div>
                        </td>

                        <td style={{textAlign: "right", fontVariantNumeric: "tabular-nums"}}>
                          {h.inBot ? `${weiToEth(h.entryCostWei, 4)} Ξ` : "—"}
                        </td>

                        <td style={{textAlign: "right", fontVariantNumeric: "tabular-nums"}}>
                          {h.inBot ? (
                            <>
                              {weiToEth(h.markValueWei, 4)} Ξ
                              {h.markStale && (
                                <span style={{color: "var(--amber)", fontSize: 9, marginLeft: 4}}>STALE</span>
                              )}
                            </>
                          ) : (
                            "—"
                          )}
                        </td>

                        <td
                          style={{
                            textAlign: "right",
                            fontVariantNumeric: "tabular-nums",
                            color: isPos ? "var(--green)" : isNeg ? "var(--red)" : "var(--muted)",
                          }}
                        >
                          {h.inBot ? (
                            <>
                              <div>{signedEth(h.unrealizedPnlWei, 4)} Ξ</div>
                              <div style={{fontSize: 10}}>{bpsFormatted(h.netPnlBps)}</div>
                            </>
                          ) : (
                            "—"
                          )}
                        </td>

                        <td style={{textAlign: "right", fontVariantNumeric: "tabular-nums", color: "var(--text)"}}>
                          {h.walletBalance ? (
                            <strong>{Number(h.walletBalance).toLocaleString(undefined, {maximumFractionDigits: 4})}</strong>
                          ) : (
                            <span className="muted">Active</span>
                          )}
                        </td>

                        <td style={{textAlign: "center"}}>
                          <div style={{display: "inline-flex", gap: 4}}>
                            <button
                              onClick={() => {
                                setSwapTarget({
                                  token: h.token,
                                  symbol: h.symbol,
                                  pair: h.pair,
                                  markEth: h.markValueWei ? weiToEth(h.markValueWei, 4) : undefined,
                                });
                                setTab("swap");
                              }}
                              style={{
                                padding: "3px 8px",
                                fontSize: 10,
                                fontWeight: 700,
                                background: "rgba(34, 211, 238, 0.15)",
                                border: "1px solid var(--cyan)",
                                color: "var(--cyan)",
                                borderRadius: 3,
                                cursor: "pointer",
                              }}
                            >
                              ⚡ SELL / SWAP
                            </button>
                            <a
                              href={aggLinks.oneInch}
                              target="_blank"
                              rel="noreferrer"
                              style={{
                                padding: "3px 6px",
                                fontSize: 10,
                                background: "var(--panel-2)",
                                border: "1px solid var(--line)",
                                color: "var(--text)",
                                borderRadius: 3,
                                textDecoration: "none",
                              }}
                              title="Sell on 1inch Aggregator"
                            >
                              1inch ↗
                            </a>
                            <a
                              href={aggLinks.dexscreener}
                              target="_blank"
                              rel="noreferrer"
                              style={{
                                padding: "3px 6px",
                                fontSize: 10,
                                background: "var(--panel-2)",
                                border: "1px solid var(--line)",
                                color: "var(--text)",
                                borderRadius: 3,
                                textDecoration: "none",
                              }}
                              title="View on DexScreener"
                            >
                              Chart ↗
                            </a>
                          </div>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </div>
      )}

      {/* ────────────────────────────────────────────────────────────────────────
          TAB 2: STRATEGY & INVESTMENT PARAMETERS (USER INPUTS)
          ──────────────────────────────────────────────────────────────────────── */}
      {tab === "parameters" && (
        <div className="panel" style={{padding: 16, display: "grid", gap: 16}}>
          {/* Preset Buttons Bar */}
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              flexWrap: "wrap",
              gap: 8,
              paddingBottom: 10,
              borderBottom: "1px solid var(--line)",
            }}
          >
            <span className="muted" style={{fontSize: 11}}>
              Quick Presets:
            </span>
            <div style={{display: "flex", gap: 8}}>
              <button
                onClick={() => applyPreset("conservative")}
                style={{
                  padding: "4px 10px",
                  fontSize: 11,
                  background: "var(--panel-2)",
                  border: "1px solid var(--line)",
                  color: "var(--green)",
                  borderRadius: 4,
                  cursor: "pointer",
                }}
              >
                🛡️ Conservative
              </button>
              <button
                onClick={() => applyPreset("balanced")}
                style={{
                  padding: "4px 10px",
                  fontSize: 11,
                  background: "var(--panel-2)",
                  border: "1px solid var(--line)",
                  color: "var(--cyan)",
                  borderRadius: 4,
                  cursor: "pointer",
                }}
              >
                ⚖️ Balanced
              </button>
              <button
                onClick={() => applyPreset("moonshot")}
                style={{
                  padding: "4px 10px",
                  fontSize: 11,
                  background: "var(--panel-2)",
                  border: "1px solid var(--line)",
                  color: "var(--amber)",
                  borderRadius: 4,
                  cursor: "pointer",
                }}
              >
                🚀 Degen Moonshot
              </button>
            </div>
          </div>

          <div style={{display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(280px, 1fr))", gap: 16}}>
            {/* Section A: Entry & Investment Sizing */}
            <div style={{display: "grid", gap: 10}}>
              <div className="muted" style={{fontSize: 11, fontWeight: 700, textTransform: "uppercase"}}>
                1. Initial Investment & Budgets
              </div>

              {/* Initial Investment / Buy Size */}
              <div>
                <label style={{display: "block", fontSize: 11, marginBottom: 3}}>
                  <strong>Initial Investment per Token (ETH):</strong>
                </label>
                <input
                  type="number"
                  step="0.005"
                  min="0"
                  value={formBuySizeEth}
                  onChange={(e) => {
                    setFormBuySizeEth(e.target.value);
                    isFormDirty.current = true;
                  }}
                  style={inputStyle}
                  placeholder="e.g. 0.05"
                />
                <div style={{display: "flex", gap: 4, marginTop: 4}}>
                  {["0.01", "0.025", "0.05", "0.1", "0.25"].map((val) => (
                    <button
                      key={val}
                      onClick={() => {
                        setFormBuySizeEth(val);
                        isFormDirty.current = true;
                      }}
                      style={chipButtonStyle}
                    >
                      {val} Ξ
                    </button>
                  ))}
                </div>
              </div>

              {/* Rolling 24h Daily Budget */}
              <div>
                <label style={{display: "block", fontSize: 11, marginBottom: 3}}>
                  <strong>Daily Spend Budget (ETH):</strong>
                </label>
                <input
                  type="number"
                  step="0.05"
                  min="0"
                  value={formDailyBudgetEth}
                  onChange={(e) => {
                    setFormDailyBudgetEth(e.target.value);
                    isFormDirty.current = true;
                  }}
                  style={inputStyle}
                  placeholder="e.g. 0.25"
                />
                <span className="muted" style={{fontSize: 10}}>
                  Ceiling on entry capital deployed within rolling 24 hours.
                </span>
              </div>

              {/* Lifetime Total Budget */}
              <div>
                <label style={{display: "block", fontSize: 11, marginBottom: 3}}>
                  <strong>Lifetime Budget (ETH, 0 = Unlimited):</strong>
                </label>
                <input
                  type="number"
                  step="0.1"
                  min="0"
                  value={formTotalBudgetEth}
                  onChange={(e) => {
                    setFormTotalBudgetEth(e.target.value);
                    isFormDirty.current = true;
                  }}
                  style={inputStyle}
                />
              </div>

              {/* Max Concurrent Positions */}
              <div>
                <label style={{display: "block", fontSize: 11, marginBottom: 3}}>
                  <strong>Max Concurrent Positions:</strong>
                </label>
                <input
                  type="number"
                  min="1"
                  max="32"
                  value={formMaxPositions}
                  onChange={(e) => {
                    setFormMaxPositions(e.target.value);
                    isFormDirty.current = true;
                  }}
                  style={inputStyle}
                />
              </div>
            </div>

            {/* Section B: Auto-Sell, Take Profit & Stop Loss */}
            <div style={{display: "grid", gap: 10}}>
              <div className="muted" style={{fontSize: 11, fontWeight: 700, textTransform: "uppercase"}}>
                2. Auto-Sell & Exit Triggers
              </div>

              {/* Take Profit (%) */}
              <div>
                <label style={{display: "block", fontSize: 11, marginBottom: 3}}>
                  <strong>Take Profit Gain (%):</strong>
                </label>
                <input
                  type="number"
                  min="5"
                  step="10"
                  value={formTakeProfitPct}
                  onChange={(e) => {
                    setFormTakeProfitPct(e.target.value);
                    isFormDirty.current = true;
                  }}
                  style={inputStyle}
                  placeholder="e.g. 100"
                />
                <div style={{display: "flex", gap: 4, marginTop: 4}}>
                  {["25", "50", "100", "200", "500"].map((val) => (
                    <button
                      key={val}
                      onClick={() => {
                        setFormTakeProfitPct(val);
                        isFormDirty.current = true;
                      }}
                      style={chipButtonStyle}
                    >
                      +{val}%
                    </button>
                  ))}
                </div>
              </div>

              {/* Take Profit (Absolute ETH Profit) */}
              <div>
                <label style={{display: "block", fontSize: 11, marginBottom: 3}}>
                  <strong>Take Profit in Absolute Profit (ETH, 0 = Off):</strong>
                </label>
                <input
                  type="number"
                  step="0.01"
                  min="0"
                  value={formTakeProfitAbsEth}
                  onChange={(e) => {
                    setFormTakeProfitAbsEth(e.target.value);
                    isFormDirty.current = true;
                  }}
                  style={inputStyle}
                  placeholder="0 (off)"
                />
                <span className="muted" style={{fontSize: 10}}>
                  Triggers if position reaches either +% gain OR this absolute ETH profit.
                </span>
              </div>

              {/* Sell Fraction (%) */}
              <div>
                <label style={{display: "block", fontSize: 11, marginBottom: 3}}>
                  <strong>Sell Fraction on Take Profit (%):</strong>
                </label>
                <input
                  type="number"
                  min="10"
                  max="100"
                  step="10"
                  value={formSellFractionPct}
                  onChange={(e) => {
                    setFormSellFractionPct(e.target.value);
                    isFormDirty.current = true;
                  }}
                  style={inputStyle}
                />
                <div style={{display: "flex", gap: 4, marginTop: 4}}>
                  {[
                    {label: "25% Scale", val: "25"},
                    {label: "50% Half", val: "50"},
                    {label: "75%", val: "75"},
                    {label: "100% Full Close", val: "100"},
                  ].map((item) => (
                    <button
                      key={item.val}
                      onClick={() => {
                        setFormSellFractionPct(item.val);
                        isFormDirty.current = true;
                      }}
                      style={chipButtonStyle}
                    >
                      {item.label}
                    </button>
                  ))}
                </div>
              </div>

              {/* Stop Loss (%) */}
              <div>
                <label style={{display: "block", fontSize: 11, marginBottom: 3}}>
                  <strong>Stop Loss (% Loss, 0 = Off):</strong>
                </label>
                <input
                  type="number"
                  min="0"
                  max="100"
                  step="5"
                  value={formStopLossPct}
                  onChange={(e) => {
                    setFormStopLossPct(e.target.value);
                    isFormDirty.current = true;
                  }}
                  style={inputStyle}
                  placeholder="e.g. 50"
                />
              </div>

              {/* Trailing Stop (%) */}
              <div>
                <label style={{display: "block", fontSize: 11, marginBottom: 3}}>
                  <strong>Trailing Stop (% from Peak, 0 = Off):</strong>
                </label>
                <input
                  type="number"
                  min="0"
                  max="90"
                  step="5"
                  value={formTrailingStopPct}
                  onChange={(e) => {
                    setFormTrailingStopPct(e.target.value);
                    isFormDirty.current = true;
                  }}
                  style={inputStyle}
                  placeholder="0 (off)"
                />
              </div>

              {/* Max Hold Duration (Minutes) */}
              <div>
                <label style={{display: "block", fontSize: 11, marginBottom: 3}}>
                  <strong>Max Hold Duration (Minutes, 0 = Off):</strong>
                </label>
                <input
                  type="number"
                  min="0"
                  step="5"
                  value={formMaxHoldMins}
                  onChange={(e) => {
                    setFormMaxHoldMins(e.target.value);
                    isFormDirty.current = true;
                  }}
                  style={inputStyle}
                />
              </div>
            </div>

            {/* Section C: Safety Gates & Honeypot Checks */}
            <div style={{display: "grid", gap: 10}}>
              <div className="muted" style={{fontSize: 11, fontWeight: 700, textTransform: "uppercase"}}>
                3. Safety Gates & Honeypot Filters
              </div>

              {/* Honeypot Pass Required */}
              <div style={{display: "flex", alignItems: "center", gap: 8}}>
                <input
                  type="checkbox"
                  id="reqHoneypot"
                  checked={formRequireHoneypot}
                  onChange={(e) => {
                    setFormRequireHoneypot(e.target.checked);
                    isFormDirty.current = true;
                  }}
                  style={{cursor: "pointer"}}
                />
                <label htmlFor="reqHoneypot" style={{fontSize: 11, cursor: "pointer"}}>
                  <strong>Require Honeypot Pass (Simulated Buy/Sell)</strong>
                </label>
              </div>

              {/* Min Pool Liquidity */}
              <div>
                <label style={{display: "block", fontSize: 11, marginBottom: 3}}>
                  <strong>Min Pool Liquidity (ETH):</strong>
                </label>
                <input
                  type="number"
                  step="0.5"
                  min="0.1"
                  value={formMinLiquidityEth}
                  onChange={(e) => {
                    setFormMinLiquidityEth(e.target.value);
                    isFormDirty.current = true;
                  }}
                  style={inputStyle}
                />
              </div>

              {/* Max Price Impact */}
              <div>
                <label style={{display: "block", fontSize: 11, marginBottom: 3}}>
                  <strong>Max Price Impact (%):</strong>
                </label>
                <input
                  type="number"
                  step="0.5"
                  min="0.5"
                  max="20"
                  value={formMaxPriceImpactPct}
                  onChange={(e) => {
                    setFormMaxPriceImpactPct(e.target.value);
                    isFormDirty.current = true;
                  }}
                  style={inputStyle}
                />
              </div>

              {/* Max Transfer Tax */}
              <div style={{display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8}}>
                <div>
                  <label style={{display: "block", fontSize: 10, marginBottom: 2}}>Max Buy Tax (%)</label>
                  <input
                    type="number"
                    step="0.5"
                    value={formMaxBuyTaxPct}
                    onChange={(e) => {
                      setFormMaxBuyTaxPct(e.target.value);
                      isFormDirty.current = true;
                    }}
                    style={inputStyle}
                  />
                </div>
                <div>
                  <label style={{display: "block", fontSize: 10, marginBottom: 2}}>Max Sell Tax (%)</label>
                  <input
                    type="number"
                    step="0.5"
                    value={formMaxSellTaxPct}
                    onChange={(e) => {
                      setFormMaxSellTaxPct(e.target.value);
                      isFormDirty.current = true;
                    }}
                    style={inputStyle}
                  />
                </div>
              </div>

              {/* Min Hold Blocks */}
              <div>
                <label style={{display: "block", fontSize: 11, marginBottom: 3}}>
                  <strong>Min Hold Blocks before Exit:</strong>
                </label>
                <input
                  type="number"
                  min="1"
                  max="20"
                  value={formMinHoldBlocks}
                  onChange={(e) => {
                    setFormMinHoldBlocks(e.target.value);
                    isFormDirty.current = true;
                  }}
                  style={inputStyle}
                />
              </div>

              {/* LP Locked Check */}
              <div style={{display: "flex", alignItems: "center", gap: 8}}>
                <input
                  type="checkbox"
                  id="reqLp"
                  checked={formRequireLpLocked}
                  onChange={(e) => {
                    setFormRequireLpLocked(e.target.checked);
                    isFormDirty.current = true;
                  }}
                  style={{cursor: "pointer"}}
                />
                <label htmlFor="reqLp" style={{fontSize: 11, cursor: "pointer"}}>
                  Require LP Burned / Locked
                </label>
              </div>
            </div>
          </div>

          {/* Save & Apply Action Buttons */}
          <div
            style={{
              display: "flex",
              justifyContent: "space-between",
              alignItems: "center",
              paddingTop: 12,
              borderTop: "1px solid var(--line)",
              flexWrap: "wrap",
              gap: 8,
            }}
          >
            <div style={{fontSize: 11, color: "var(--muted)"}}>
              Changes apply immediately to the running sniper lane.
            </div>
            <div style={{display: "flex", gap: 8}}>
              <button
                onClick={() => {
                  populateFormFromConfig(cfg.params);
                  isFormDirty.current = false;
                  setFeedback({type: "info", msg: "Form reset to saved backend parameters."});
                }}
                style={{
                  padding: "6px 14px",
                  background: "var(--panel-2)",
                  border: "1px solid var(--line)",
                  color: "var(--muted)",
                  borderRadius: 4,
                  fontSize: 11,
                  cursor: "pointer",
                }}
              >
                Reset to Saved
              </button>
              <button
                onClick={() => handleSaveParams()}
                disabled={isSaving}
                style={{
                  padding: "6px 20px",
                  background: "var(--green)",
                  border: "1px solid var(--green)",
                  color: "#05240f",
                  fontWeight: 700,
                  borderRadius: 4,
                  fontSize: 12,
                  cursor: "pointer",
                }}
              >
                {isSaving ? "Saving..." : "✓ Save & Apply Parameters"}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* ────────────────────────────────────────────────────────────────────────
          TAB 3: INSTANT SELL & DEX AGGREGATOR SWAPPING HUB
          ──────────────────────────────────────────────────────────────────────── */}
      {tab === "swap" && (
        <div className="panel" style={{padding: 16, display: "grid", gap: 14}}>
          <div className="panel-head" style={{padding: "0 0 8px 0"}}>
            <span>⚡ Instant Token Sell & DEX Aggregator Hub</span>
            <span className="muted" style={{fontSize: 11}}>
              Route via 1inch, Uniswap, KyberSwap or DexScreener
            </span>
          </div>

          <div style={{display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(280px, 1fr))", gap: 16}}>
            {/* Left: Position / Token Selection */}
            <div style={{display: "grid", gap: 10}}>
              <div>
                <label style={{display: "block", fontSize: 11, marginBottom: 4}}>
                  <strong>Select Token from Active Holdings:</strong>
                </label>
                <select
                  style={inputStyle}
                  value={swapTarget?.token || ""}
                  onChange={(e) => {
                    const match = unifiedHoldings.find((p) => p.token.toLowerCase() === e.target.value.toLowerCase());
                    if (match) {
                      setSwapTarget({
                        token: match.token,
                        symbol: match.symbol,
                        pair: match.pair,
                        markEth: match.markValueWei ? weiToEth(match.markValueWei, 4) : undefined,
                      });
                    } else if (e.target.value) {
                      setSwapTarget({token: e.target.value, symbol: "CUSTOM"});
                    } else {
                      setSwapTarget(null);
                    }
                  }}
                >
                  <option value="">-- Choose token from holdings or enter address --</option>
                  {unifiedHoldings.map((h) => (
                    <option key={h.token} value={h.token}>
                      {h.symbol} ({shortHash(h.token, 4)}) {h.inBot ? `— Mark: ${weiToEth(h.markValueWei, 3)} ETH` : `— Balance: ${h.walletBalance}`}
                    </option>
                  ))}
                </select>
              </div>

              <div>
                <label style={{display: "block", fontSize: 11, marginBottom: 4}}>
                  <strong>Or Enter Any Token Contract Address:</strong>
                </label>
                <input
                  type="text"
                  value={swapTarget?.token || ""}
                  onChange={(e) =>
                    setSwapTarget({
                      token: e.target.value,
                      symbol: "CUSTOM",
                    })
                  }
                  placeholder="0x..."
                  style={inputStyle}
                />
              </div>

              {/* Sell Quantity Fraction */}
              <div>
                <label style={{display: "block", fontSize: 11, marginBottom: 4}}>
                  <strong>Sell Amount (% of Holdings):</strong>
                </label>
                <div style={{display: "flex", gap: 6}}>
                  {[25, 50, 75, 100].map((f) => (
                    <button
                      key={f}
                      onClick={() => setSwapFraction(f)}
                      style={{
                        flex: 1,
                        padding: "6px",
                        fontSize: 11,
                        fontWeight: swapFraction === f ? 700 : 500,
                        background: swapFraction === f ? "var(--cyan)" : "var(--panel-2)",
                        color: swapFraction === f ? "#05240f" : "var(--text)",
                        border: "1px solid",
                        borderColor: swapFraction === f ? "var(--cyan)" : "var(--line)",
                        borderRadius: 3,
                        cursor: "pointer",
                      }}
                    >
                      {f}%
                    </button>
                  ))}
                </div>
              </div>
            </div>

            {/* Right: Aggregator Links & Launchpad */}
            <div style={{display: "grid", gap: 10}}>
              {swapTarget && isAddress(swapTarget.token) ? (
                (() => {
                  const links = getAggregatorLinks(swapTarget.token, currentChainId, activeChainSlug);
                  return (
                    <div style={{background: "var(--panel-2)", padding: 12, borderRadius: 6, border: "1px solid var(--line)"}}>
                      <div style={{fontSize: 12, fontWeight: 700, marginBottom: 8, color: "var(--cyan)"}}>
                        DEX Aggregator Execution for {swapTarget.symbol || "Token"}:
                      </div>

                      <div style={{display: "grid", gap: 8}}>
                        <a
                          href={links.oneInch}
                          target="_blank"
                          rel="noreferrer"
                          style={{
                            display: "flex",
                            alignItems: "center",
                            justifyContent: "space-between",
                            padding: "8px 12px",
                            background: "rgba(34, 211, 238, 0.12)",
                            border: "1px solid var(--cyan)",
                            borderRadius: 4,
                            color: "var(--cyan)",
                            textDecoration: "none",
                            fontWeight: 700,
                            fontSize: 12,
                          }}
                        >
                          <span>🦄 1inch DEX Aggregator (Optimal Route)</span>
                          <span>Open ↗</span>
                        </a>

                        <a
                          href={links.uniswap}
                          target="_blank"
                          rel="noreferrer"
                          style={{
                            display: "flex",
                            alignItems: "center",
                            justifyContent: "space-between",
                            padding: "8px 12px",
                            background: "var(--panel)",
                            border: "1px solid var(--line)",
                            borderRadius: 4,
                            color: "var(--text)",
                            textDecoration: "none",
                            fontSize: 12,
                          }}
                        >
                          <span>🦄 Uniswap / Aerodrome Direct Swap</span>
                          <span>Open ↗</span>
                        </a>

                        <a
                          href={links.kyberswap}
                          target="_blank"
                          rel="noreferrer"
                          style={{
                            display: "flex",
                            alignItems: "center",
                            justifyContent: "space-between",
                            padding: "8px 12px",
                            background: "var(--panel)",
                            border: "1px solid var(--line)",
                            borderRadius: 4,
                            color: "var(--text)",
                            textDecoration: "none",
                            fontSize: 12,
                          }}
                        >
                          <span>⚡ KyberSwap Meta-Aggregator</span>
                          <span>Open ↗</span>
                        </a>

                        <a
                          href={links.dexscreener}
                          target="_blank"
                          rel="noreferrer"
                          style={{
                            display: "flex",
                            alignItems: "center",
                            justifyContent: "space-between",
                            padding: "8px 12px",
                            background: "var(--panel)",
                            border: "1px solid var(--line)",
                            borderRadius: 4,
                            color: "var(--amber)",
                            textDecoration: "none",
                            fontSize: 12,
                          }}
                        >
                          <span>📈 DexScreener Live Pool & Chart</span>
                          <span>View ↗</span>
                        </a>
                      </div>
                    </div>
                  );
                })()
              ) : (
                <div
                  style={{
                    padding: 24,
                    textAlign: "center",
                    color: "var(--muted)",
                    border: "1px dashed var(--line)",
                    borderRadius: 6,
                    fontSize: 11,
                  }}
                >
                  Select a token from your holdings or enter an address on the left to generate aggregator swap links.
                </div>
              )}
            </div>
          </div>
        </div>
      )}

      {/* ────────────────────────────────────────────────────────────────────────
          TAB 4: TRADE HISTORY (SIMULATIONS & REAL TRADES)
          ──────────────────────────────────────────────────────────────────────── */}
      {tab === "history" && (
        <div className="panel" style={{padding: 14, display: "grid", gap: 12}}>
          <div className="panel-head" style={{padding: 0}}>
            <span>📜 Historical Snipes & Trade Logs</span>
            <span className="muted" style={{fontSize: 11}}>
              Displays both simulated paper executions and live closed trades
            </span>
          </div>

          {pf.recentClosed.length === 0 ? (
            <div className="muted" style={{padding: 24, textAlign: "center", fontSize: 11}}>
              No trade history recorded yet.
            </div>
          ) : (
            <div style={{overflowX: "auto"}}>
              <table className="grid" style={{width: "100%", fontSize: 12}}>
                <thead>
                  <tr>
                    <th>TYPE / MODE</th>
                    <th>TOKEN</th>
                    <th style={{textAlign: "right"}}>ENTRY (ETH)</th>
                    <th style={{textAlign: "right"}}>REALISED (ETH)</th>
                    <th style={{textAlign: "right"}}>NET PNL</th>
                    <th>EXIT REASON</th>
                    <th style={{textAlign: "right"}}>CLOSED</th>
                  </tr>
                </thead>
                <tbody>
                  {pf.recentClosed.map((pos) => {
                    const isLive = isArmed;
                    return (
                      <tr key={pos.id}>
                        <td>
                          <span
                            className="badge"
                            style={{
                              fontSize: 9,
                              color: isLive ? "var(--green)" : "var(--amber)",
                              borderColor: isLive ? "var(--green)" : "var(--amber)",
                            }}
                          >
                            {isLive ? "LIVE TRADE" : "SIMULATION"}
                          </span>
                        </td>
                        <td>
                          <strong>{pos.symbol || shortHash(pos.token, 4)}</strong>
                          <span className="muted" style={{marginLeft: 6, fontSize: 10}}>
                            {pos.venue}
                          </span>
                        </td>
                        <td style={{textAlign: "right", fontVariantNumeric: "tabular-nums"}}>
                          {weiToEth(pos.entryCostWei, 4)} Ξ
                        </td>
                        <td style={{textAlign: "right", fontVariantNumeric: "tabular-nums", color: pnlColor(pos.realizedWei)}}>
                          {weiToEth(pos.realizedWei, 4)} Ξ
                        </td>
                        <td style={{textAlign: "right", fontVariantNumeric: "tabular-nums", color: pnlColor(pos.netPnlWei)}}>
                          {signedEth(pos.netPnlWei, 4)} Ξ ({bpsFormatted(pos.netPnlBps)})
                        </td>
                        <td className="muted" style={{fontSize: 11}}>
                          {pos.exitReason ? EXIT_LABEL[pos.exitReason] || pos.exitReason : "Closed"}
                        </td>
                        <td style={{textAlign: "right", color: "var(--muted)", fontSize: 11}}>
                          {pos.closedAtMs ? ago(pos.closedAtMs) : "—"}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </div>
      )}

      {/* ────────────────────────────────────────────────────────────────────────
          TAB 5: SAFETY GATES & HONEYPOT DIAGNOSTICS
          ──────────────────────────────────────────────────────────────────────── */}
      {tab === "gates" && (
        <div className="panel" style={{padding: 16, display: "grid", gap: 14}}>
          <div className="panel-head" style={{padding: 0}}>
            <span>🛡️ Launch Filter Diagnostics & Honeypot Analytics</span>
          </div>

          <div className="muted" style={{fontSize: 11}}>
            Every evaluated token launch is scored through the safety filter suite. Rejections are counted by code below to
            diagnose exactly why candidate launches were approved or turned down.
          </div>

          <div style={{display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(200px, 1fr))", gap: 12}}>
            {Object.entries(cfg.rejections ?? {})
              .sort((a, b) => b[1] - a[1])
              .map(([code, count]) => (
                <div key={code} style={{background: "var(--panel-2)", border: "1px solid var(--line)", borderRadius: 4, padding: "8px 12px"}}>
                  <div className="muted" style={{fontSize: 10, textTransform: "uppercase"}}>
                    {code.replace(/_/g, " ")}
                  </div>
                  <div style={{fontSize: 18, fontWeight: 700, marginTop: 4}}>
                    {count}
                  </div>
                </div>
              ))}
          </div>

          <div className="panel" style={{padding: 12, background: "var(--panel-2)", marginTop: 6}}>
            <div style={{fontSize: 12, fontWeight: 700, marginBottom: 6}}>
              Durable .env Configuration Snippet:
            </div>
            <pre
              style={{
                background: "#070b11",
                padding: 10,
                borderRadius: 4,
                fontSize: 11,
                overflowX: "auto",
                color: "var(--cyan)",
              }}
            >
              {cfg.envSnippet}
            </pre>
            <button
              onClick={() => {
                navigator.clipboard.writeText(cfg.envSnippet);
                setFeedback({type: "info", msg: "Copied .env configuration snippet to clipboard!"});
              }}
              style={{
                marginTop: 8,
                padding: "4px 12px",
                fontSize: 11,
                background: "var(--panel)",
                border: "1px solid var(--line)",
                color: "var(--text)",
                borderRadius: 3,
                cursor: "pointer",
              }}
            >
              📋 Copy .env Snippet
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

function MetricCard({
  label,
  value,
  sub,
  tone,
  title,
}: {
  label: string;
  value: string;
  sub?: string;
  tone?: "pos" | "neg" | "amber" | "muted";
  title?: string;
}) {
  return (
    <div
      style={{
        background: "var(--panel-2)",
        border: "1px solid var(--line)",
        borderRadius: 4,
        padding: "8px 10px",
      }}
      title={title}
    >
      <div className="muted" style={{fontSize: 10, textTransform: "uppercase", letterSpacing: "0.05em"}}>
        {label}
      </div>
      <div
        className={tone}
        style={{
          fontSize: 14,
          fontWeight: 700,
          marginTop: 2,
          fontVariantNumeric: "tabular-nums",
          color:
            tone === "pos"
              ? "var(--green)"
              : tone === "neg"
                ? "var(--red)"
                : tone === "amber"
                  ? "var(--amber)"
                  : "inherit",
        }}
      >
        {value}
      </div>
      {sub && (
        <div className="muted" style={{fontSize: 10, marginTop: 1}}>
          {sub}
        </div>
      )}
    </div>
  );
}

const inputStyle: React.CSSProperties = {
  width: "100%",
  background: "#070b11",
  border: "1px solid var(--line)",
  borderRadius: 4,
  color: "var(--text)",
  padding: "6px 8px",
  fontSize: 12,
  fontFamily: "inherit",
};

const chipButtonStyle: React.CSSProperties = {
  padding: "2px 6px",
  fontSize: 10,
  background: "var(--panel-2)",
  border: "1px solid var(--line)",
  color: "var(--cyan)",
  borderRadius: 3,
  cursor: "pointer",
};

export default memo(SniperPanel);
