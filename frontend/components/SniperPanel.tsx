"use client";

/**
 * Sniper — New Token Portfolio & Directional Execution Console.
 *
 * Designed for high functionality, performance, and clear execution controls:
 * 1. Master On/Off Switch & Sim/Live Mode Switch.
 * 2. Parameter configuration: Initial Investment (ETH), Auto-Sell Take-Profit (% and absolute ETH),
 *    Sell Fraction %, Stop Loss %, Trailing Stop %, Budgets, and Safety Filters.
 * 3. Mini Portfolio: Active positions, unrealized & realized PnL, and connected wallet holdings.
 * 4. Swapping & Selling Features: 1-click partial/full sells and DEX Aggregators (1inch, Uniswap, KyberSwap, DexScreener).
 */

import {memo, useCallback, useEffect, useMemo, useRef, useState} from "react";
import {readActiveChain, withChain} from "@/lib/chain";
import {ago, shortHash, signedEth, weiToEth} from "@/lib/format";
import {addressUrl, txUrl} from "@/lib/explorer";
import {useWallet} from "@/lib/wallet";
import {getAggregatorLinks, ERC20_ABI} from "@/lib/swap";
import SniperVaultWizard from "./SniperVaultWizard";
import TradeTerminal from "./TradeTerminal";
import {
  createPublicClient,
  createWalletClient,
  custom,
  formatEther,
  formatUnits,
  http,
  isAddress,
  parseEther,
  type Address,
} from "viem";
import {base, mainnet} from "viem/chains";
import type {
  SniperModeResponse,
  SniperParams,
  SniperParamsPatch,
  SniperParamsResponse,
  SniperPortfolio,
  SniperPortfolioRow,
  SniperVaultStatus,
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

// Preset definitions for quick parameter setup
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
  const [vault, setVault] = useState<SniperVaultStatus | null>(null);
  /** The sniper's own independent mode payload (never the atomic mode). */
  const [modeInfo, setModeInfo] = useState<SniperModeResponse | null>(null);
  const [demo, setDemo] = useState(false);
  const [tab, setTab] = useState<"portfolio" | "parameters" | "swap" | "gates">("portfolio");
  const [portfolioSubTab, setPortfolioSubTab] = useState<"all" | "open" | "wallet" | "closed">("all");
  /** Two-ledger portfolio view: simulation / live / labelled combination. */
  const [ledgerView, setLedgerView] = useState<"simulation" | "live" | "all">("simulation");
  /** Mode-switch confirmation dialogs. */
  const [modeDialog, setModeDialog] = useState<"none" | "confirm-live" | "confirm-sim">("none");
  const [modeBusy, setModeBusy] = useState(false);
  const [modeNote, setModeNote] = useState<string | null>(null);

  // Wallet and chain state
  const wallet = useWallet();
  const rawChainSlug = readActiveChain();
  const activeChainSlug = rawChainSlug || "ethereum";
  const currentChainId = activeChainSlug === "base" ? 8453 : 1;

  // Notification / Feedback state
  const [feedback, setFeedback] = useState<{type: "success" | "error" | "info"; msg: string} | null>(null);
  const [isSaving, setIsSaving] = useState(false);
  const [isHalting, setIsHalting] = useState(false);

  // Do not let the 4-second refresh overwrite an operator's unsaved form.
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

  // Swap Drawer / Modal state
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
  // Manual controls require both addresses because an operator must identify
  // the exact V2 pair. The bot validates the pair against the configured WETH
  // and still enforces the SniperVault budget/slippage guards.
  const [manualBuyToken, setManualBuyToken] = useState("");
  const [manualBuyPair, setManualBuyPair] = useState("");
  const [manualBuySizeEth, setManualBuySizeEth] = useState("0.05");

  // Sync form with fetched config
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
      const [pRes, cRes, vRes, mRes] = await Promise.all([
        fetch(withChain("/api/bot/sniper/portfolio", chain), {cache: "no-store"}),
        fetch(withChain("/api/bot/sniper/params", chain), {cache: "no-store"}),
        fetch(withChain("/api/bot/sniper/vault", chain), {cache: "no-store"}),
        fetch(withChain("/api/bot/sniper/mode", chain), {cache: "no-store"}),
      ]);
      if (pRes.ok && cRes.ok) {
        const p = (await pRes.json()) as SniperPortfolio & {demo?: boolean};
        const c = (await cRes.json()) as SniperParamsResponse & {demo?: boolean};
        setPf(p);
        setCfg(c);
        setDemo(Boolean(p.demo || c.demo));
        if (!formInitialized.current || !isFormDirty.current) {
          populateFormFromConfig(c.params);
          formInitialized.current = true;
        }
      }
      if (vRes.ok) setVault((await vRes.json()) as SniperVaultStatus & {demo?: boolean});
      if (mRes.ok) setModeInfo((await mRes.json()) as SniperModeResponse & {demo?: boolean});
    } catch {
      /* maintain existing data on transient fetch error */
    }
  }, [populateFormFromConfig]);

  /**
   * Flip the sniper's independent execution mode. The atomic engine's mode is
   * never touched by this control.
   */
  const handleSetSniperMode = useCallback(async (target: "simulation" | "live") => {
    setModeBusy(true);
    setModeNote(null);
    const chain = readActiveChain();
    try {
      const res = await fetch(withChain("/api/bot/sniper/mode", chain), {
        method: "POST",
        headers: {"content-type": "application/json"},
        body: JSON.stringify({mode: target}),
      });
      const body = (await res.json()) as {ok?: boolean; error?: string; blockers?: string[]; note?: string};
      if (!res.ok || body.ok === false) {
        const blockers = body.blockers?.length ? body.blockers.join(" · ") : "";
        setModeNote(blockers || body.error || `switch refused (HTTP ${res.status})`);
        return;
      }
      setModeDialog("none");
      setModeNote(null);
      await load();
    } catch (error) {
      setModeNote((error instanceof Error ? error.message : String(error)).split("\n")[0]);
    } finally {
      setModeBusy(false);
    }
  }, [load]);

  // Initial load and periodic polling
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

  // Scan user wallet holdings for portfolio tokens or recent snipes
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
          // ignore failed token read
        }
      }
      setWalletTokens(results);
    } catch (e) {
      console.error("Wallet token scan error:", e);
    } finally {
      setIsScanningWallet(false);
    }
  }, [wallet.address, publicClient, pf, customTokenInput]);

  useEffect(() => {
    if (wallet.address && tab === "portfolio") {
      scanWalletBalances();
    }
  }, [wallet.address, tab, scanWalletBalances]);

  // Combine bot positions and connected-wallet balances without losing the
  // provenance of either source. This is intentionally computed before the
  // loading return so hook order is invariant across polling updates.
  const unifiedHoldings = useMemo(() => {
    const holdings = new Map<string, {
      token: string;
      symbol: string;
      pair?: string;
      state?: string;
      entryCostWei: string;
      markValueWei: string;
      unrealizedPnlWei: string;
      netPnlBps: number;
      markStale: boolean;
      inBot: boolean;
      inWallet: boolean;
      walletBalance?: string;
      /** Which ledger owns the bot side: simulation paper or live vault. */
      executionMode?: "simulation" | "live";
    }>();
    for (const pos of pf?.open ?? []) {
      holdings.set(pos.token.toLowerCase(), {
        token: pos.token,
        symbol: pos.symbol || shortHash(pos.token, 4),
        pair: pos.pair,
        state: pos.state,
        entryCostWei: pos.entryCostWei,
        markValueWei: pos.markValueWei,
        unrealizedPnlWei: pos.unrealizedPnlWei,
        netPnlBps: pos.netPnlBps,
        markStale: pos.markStale,
        inBot: true,
        inWallet: false,
        executionMode: pos.executionMode ?? "live",
      });
    }
    for (const token of walletTokens) {
      const current = holdings.get(token.address.toLowerCase());
      if (current) {
        current.inWallet = true;
        current.walletBalance = token.balance;
      } else {
        holdings.set(token.address.toLowerCase(), {
          token: token.address,
          symbol: token.symbol,
          entryCostWei: "0",
          markValueWei: "0",
          unrealizedPnlWei: "0",
          netPnlBps: 0,
          markStale: false,
          inBot: false,
          inWallet: true,
          walletBalance: token.balance,
        });
      }
    }
    return [...holdings.values()];
  }, [pf?.open, walletTokens]);

  /**
   * Two-ledger filtering. Simulation rows and live rows are never added
   * together; "All" renders them as separate, labelled sections downstream.
   */
  const matchesLedger = useCallback(
    (executionMode: "simulation" | "live" | undefined): boolean => {
      const mode = executionMode ?? "live";
      if (ledgerView === "all") return true;
      return mode === ledgerView;
    },
    [ledgerView],
  );

  const filteredOpen = useMemo(
    () => (pf?.open ?? []).filter((row) => matchesLedger(row.executionMode)),
    [pf?.open, matchesLedger],
  );
  const filteredClosed = useMemo(
    () => (pf?.recentClosed ?? []).filter((row) => matchesLedger(row.executionMode)),
    [pf?.recentClosed, matchesLedger],
  );
  const filteredHoldings = useMemo(
    () =>
      unifiedHoldings.filter((h) => {
        if (h.inWallet && !h.inBot) return ledgerView !== "simulation"; // wallet rows are never simulation
        return matchesLedger(h.executionMode);
      }),
    [unifiedHoldings, ledgerView, matchesLedger],
  );

  // Patch parameters API call
  const handleSaveParams = async (overridePatch?: Partial<SniperParamsPatch>) => {
    setIsSaving(true);
    setFeedback(null);
    const chain = readActiveChain();

    const patch: SniperParamsPatch = {
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
      requireHoneypotPass: overridePatch?.requireHoneypotPass ?? formRequireHoneypot,
      requireLpLocked: overridePatch?.requireLpLocked ?? formRequireLpLocked,
      enabled: overridePatch?.enabled ?? cfg?.params.enabled,
    };

    try {
      const res = await fetch(withChain("/api/bot/sniper/params", chain), {
        method: "POST",
        headers: {"content-type": "application/json"},
        body: JSON.stringify(patch),
      });
      const data = await res.json();
      if (res.ok && data.ok) {
        setFeedback({type: "success", msg: "Sniper parameters updated successfully!"});
        isFormDirty.current = false;
        if (data.params) populateFormFromConfig(data.params);
        await load();
      } else {
        const errorMsg = data.errors?.join("; ") || data.error || "Failed to update sniper parameters";
        setFeedback({type: "error", msg: errorMsg});
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

  const handleManualBuy = async () => {
    if (!isAddress(manualBuyToken) || !isAddress(manualBuyPair)) {
      setFeedback({type: "error", msg: "Manual buy needs a valid token address and V2 pair address."});
      return;
    }
    const sizeWei = weiFromEth(manualBuySizeEth);
    if (sizeWei === "0") {
      setFeedback({type: "error", msg: "Manual buy size must be greater than zero."});
      return;
    }
    if (!window.confirm("Submit this manual buy? This explicitly bypasses the automatic launch probe; the on-chain vault budget and slippage guards still apply.")) return;
    setIsSaving(true);
    setFeedback(null);
    try {
      const response = await fetch(withChain("/api/bot/sniper/buy", readActiveChain()), {
        method: "POST",
        headers: {"content-type": "application/json"},
        body: JSON.stringify({token: manualBuyToken, pair: manualBuyPair, sizeWei}),
      });
      const data = (await response.json()) as {ok?: boolean; error?: string; manualProbeBypass?: boolean};
      if (!response.ok || !data.ok) throw new Error(data.error || "manual buy rejected");
      setFeedback({type: "success", msg: "Manual buy submitted through SniperVault. The action bypassed the automatic launch probe by operator request."});
      await load();
    } catch (error) {
      setFeedback({type: "error", msg: `Manual buy failed: ${(error as Error).message}`});
    } finally {
      setIsSaving(false);
    }
  };

  const handleManualSell = async (id: string, fractionBps: number) => {
    if (!window.confirm(`Submit a ${fractionBps === 5000 ? "50%" : "100%"} manual exit for this position?`)) return;
    setIsSaving(true);
    setFeedback(null);
    try {
      const response = await fetch(withChain("/api/bot/sniper/sell", readActiveChain()), {
        method: "POST",
        headers: {"content-type": "application/json"},
        body: JSON.stringify({id, sellFractionBps: fractionBps}),
      });
      const data = (await response.json()) as {ok?: boolean; error?: string; txHash?: string};
      if (!response.ok || !data.ok) throw new Error(data.error || "manual exit rejected");
      setFeedback({type: "success", msg: `Manual ${fractionBps === 5000 ? "50%" : "100%"} exit submitted${data.txHash ? ` (${shortHash(data.txHash, 5)})` : ""}.`});
      await load();
    } catch (error) {
      setFeedback({type: "error", msg: `Manual exit failed: ${(error as Error).message}`});
    } finally {
      setIsSaving(false);
    }
  };

  const handleResetPaper = async () => {
    if (!window.confirm("Reset the virtual simulation bankroll to 1 ETH? This does not touch an on-chain balance.")) return;
    setIsSaving(true);
    try {
      const response = await fetch(withChain("/api/bot/sniper/paper/reset", readActiveChain()), {method: "POST", headers: {"content-type": "application/json"}});
      const data = (await response.json()) as {ok?: boolean; error?: string; simulationBalanceWei?: string};
      if (!response.ok || !data.ok) throw new Error(data.error || "paper funds reset rejected");
      setFeedback({type: "success", msg: "Virtual simulation bankroll reset to 1 ETH."});
      await load();
    } catch (error) {
      setFeedback({type: "error", msg: `Paper funds reset failed: ${(error as Error).message}`});
    } finally {
      setIsSaving(false);
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

          {/* SNIPER MODE — an independent, clickable execution-mode control.
              It is deliberately NOT the atomic MEV engine's mode switch, and
              it is a real control (buttons), not a status badge. */}
          <SniperModeControl
            modeInfo={modeInfo}
            armed={isArmed}
            onOpenDialog={setModeDialog}
          />
          {demo && (
            <span className="badge" style={{color: "var(--amber)"}}>
              DEMO DATA
            </span>
          )}

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

        {/* Quick Parameters Display in Header */}
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

      {(cfg.sniperMode ?? (cfg.paperMode ? "simulation" : "live")) === "simulation" && (
        <div className="panel" style={{padding: "8px 12px", display: "flex", justifyContent: "space-between", alignItems: "center", gap: 10, flexWrap: "wrap", borderColor: "rgba(34,211,238,0.45)", background: "rgba(34,211,238,0.05)"}}>
          <div style={{display: "flex", alignItems: "center", gap: 8, fontSize: 11, flexWrap: "wrap"}}>
            <span className="badge" style={{color: "var(--cyan)", borderColor: "var(--cyan)"}}>SIMULATION WALLET</span>
            <span className="muted">virtual balance</span>
            <strong style={{color: "var(--cyan)", fontVariantNumeric: "tabular-nums"}}>
              {ethFromWei(modeInfo?.simulationBalanceWei ?? cfg.simulationBalanceWei ?? "1000000000000000000", 4)} Ξ
            </strong>
            <span className="muted">paper only · no RPC funds</span>
            {modeInfo?.simulationVaultAddress && (
              <span className="muted">
                Simulation vault · local Anvil only: <code>{shortHash(modeInfo.simulationVaultAddress, 6)}</code>
              </span>
            )}
          </div>
          <button onClick={() => void handleResetPaper()} disabled={isSaving} style={{...chipButtonStyle, color: "var(--cyan)", borderColor: "var(--cyan)"}}>↻ reset 1 ETH paper funds</button>
        </div>
      )}

      {/* ── Status Alerts & Feedback Banner ── */}
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

      {/* ── SniperVault onboarding ── */}
      <SniperVaultWizard params={cfg.params} onBound={() => void load()} />

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

      {/* ── Mini Portfolio Summary Metrics ── */}
      <div
        className="panel"
        style={{
          display: "grid",
          gridTemplateColumns: "repeat(auto-fit, minmax(130px, 1fr))",
          gap: 10,
          padding: "12px 16px",
        }}
      >
        <MetricCard label="Open Positions" value={`${totals.openPositions}`} />
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

      {/* ── On-chain vault status ── */}
      <div className="panel" style={{padding: "10px 14px", display: "grid", gap: 7, borderColor: vault?.configured ? "rgba(34,197,94,0.45)" : "var(--line)"}}>
        <div style={{display: "flex", justifyContent: "space-between", alignItems: "center", gap: 8, flexWrap: "wrap"}}>
          <strong style={{fontSize: 11}}>SniperVault status · {activeChainSlug === "base" ? "Base" : "Ethereum"}</strong>
          <span className="badge" style={{color: vault?.configured ? "var(--green)" : "var(--amber)", borderColor: vault?.configured ? "var(--green)" : "var(--amber)"}}>{vault?.configured ? "ON-CHAIN CONFIGURED" : "NOT CONFIGURED"}</span>
        </div>
        <div style={{display: "flex", gap: 14, alignItems: "center", flexWrap: "wrap", fontSize: 11}}>
          <span className="muted">address <code>{vault?.address ? shortHash(vault.address, 7) : "—"}</code></span>
          <span className="muted">spendable <strong>{vault?.spendableRemainingWei ? `${ethFromWei(vault.spendableRemainingWei, 4)} Ξ` : "—"}</strong></span>
          <span className="muted">daily cap <strong>{vault?.dailyBudgetWei ? `${ethFromWei(vault.dailyBudgetWei, 4)} Ξ` : "—"}</strong></span>
          <span className="muted">window reset <strong>{vault?.windowResetTimeSecs ? new Date(vault.windowResetTimeSecs * 1000).toLocaleString() : "—"}</strong></span>
          {vault?.demo && <span className="badge" style={{color: "var(--amber)"}}>DEMO DATA</span>}
        </div>
      </div>

      {/* ── Sub-Navigation Tabs ── */}
      <div style={{display: "flex", gap: 8, borderBottom: "1px solid var(--line)", paddingBottom: 6}}>
        {[
          {id: "portfolio", label: `📊 Mini Portfolio (${pf.open.length} Active)`},
          {id: "parameters", label: "⚙️ Strategy & Investment Parameters"},
          {id: "swap", label: "📈 Trade / Charts"},
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
          TAB 1: MINI PORTFOLIO & POSITION MANAGEMENT
          ──────────────────────────────────────────────────────────────────────── */}
      {tab === "portfolio" && (
        <div className="panel" style={{padding: 14, display: "grid", gap: 12}}>
          {/* Two-ledger portfolio switcher: simulation paper and live vault
              rows are never merged into one unlabeled number. */}
          <div style={{display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap"}}>
            <span className="muted" style={{fontSize: 10, letterSpacing: "0.05em"}}>PORTFOLIO VIEW</span>
            {([
              ["simulation", "Simulation"],
              ["live", "Live"],
              ["all", "All (separate sections)"],
            ] as const).map(([value, label]) => (
              <button
                key={value}
                onClick={() => setLedgerView(value)}
                aria-pressed={ledgerView === value}
                style={{
                  padding: "3px 10px",
                  fontSize: 11,
                  borderRadius: 3,
                  background: ledgerView === value ? "rgba(34,211,238,0.15)" : "transparent",
                  color: ledgerView === value ? "var(--cyan)" : "var(--muted)",
                  border: `1px solid ${ledgerView === value ? "var(--cyan)" : "var(--line)"}`,
                  cursor: "pointer",
                  fontFamily: "inherit",
                }}
              >
                {label}
              </button>
            ))}
            {ledgerView !== "all" && pf?.totalsByMode && (
              <span className="muted" style={{fontSize: 10}}>
                {ledgerView === "simulation"
                  ? `sim totals: ${weiToEth(pf.totalsByMode.simulation.totalPnlWei, 4)} Ξ across ${pf.totalsByMode.simulation.openPositions + pf.totalsByMode.simulation.closedPositions} positions`
                  : `live totals: ${weiToEth(pf.totalsByMode.live.totalPnlWei, 4)} Ξ across ${pf.totalsByMode.live.openPositions + pf.totalsByMode.live.closedPositions} positions`}
              </span>
            )}
          </div>

          {/* Portfolio View Switcher */}
          <div style={{display: "flex", justifyContent: "space-between", alignItems: "center", flexWrap: "wrap", gap: 8}}>
            <div style={{display: "flex", gap: 6}}>
              <button
                onClick={() => setPortfolioSubTab("all")}
                style={{
                  padding: "4px 10px",
                  fontSize: 11,
                  borderRadius: 3,
                  background: portfolioSubTab === "all" ? "var(--line)" : "transparent",
                  color: portfolioSubTab === "all" ? "var(--text)" : "var(--muted)",
                  border: "1px solid var(--line)",
                  cursor: "pointer",
                }}
              >
                All Holdings ({filteredHoldings.length})
              </button>
              <button
                onClick={() => setPortfolioSubTab("open")}
                style={{
                  padding: "4px 10px",
                  fontSize: 11,
                  borderRadius: 3,
                  background: portfolioSubTab === "open" ? "var(--line)" : "transparent",
                  color: portfolioSubTab === "open" ? "var(--text)" : "var(--muted)",
                  border: "1px solid var(--line)",
                  cursor: "pointer",
                }}
              >
                Active Positions ({filteredOpen.length})
              </button>
              <button
                onClick={() => setPortfolioSubTab("wallet")}
                style={{
                  padding: "4px 10px",
                  fontSize: 11,
                  borderRadius: 3,
                  background: portfolioSubTab === "wallet" ? "var(--line)" : "transparent",
                  color: portfolioSubTab === "wallet" ? "var(--text)" : "var(--muted)",
                  border: "1px solid var(--line)",
                  cursor: "pointer",
                }}
              >
                Wallet Holdings ({walletTokens.length})
              </button>
              <button
                onClick={() => setPortfolioSubTab("closed")}
                style={{
                  padding: "4px 10px",
                  fontSize: 11,
                  borderRadius: 3,
                  background: portfolioSubTab === "closed" ? "var(--line)" : "transparent",
                  color: portfolioSubTab === "closed" ? "var(--text)" : "var(--muted)",
                  border: "1px solid var(--line)",
                  cursor: "pointer",
                }}
              >
                Closed History ({filteredClosed.length})
              </button>
            </div>

            {portfolioSubTab === "wallet" && (
              <button
                onClick={() => scanWalletBalances()}
                disabled={isScanningWallet || !wallet.address}
                style={{
                  padding: "3px 8px",
                  fontSize: 11,
                  background: "var(--panel-2)",
                  border: "1px solid var(--line)",
                  color: "var(--cyan)",
                  borderRadius: 3,
                  cursor: "pointer",
                }}
              >
                {isScanningWallet ? "Scanning..." : "↻ Refresh Wallet Balances"}
              </button>
            )}
          </div>

          {/* Explicit manual buy control. This is separate from the automatic
              launch detector and is intentionally gated by a confirmation. */}
          <div style={{display: "grid", gap: 7, padding: "9px 10px", border: "1px solid var(--amber)", borderRadius: 4, background: "rgba(245,181,68,0.06)"}}>
            <div style={{fontSize: 11, fontWeight: 700, color: "var(--amber)"}}>Manual limit buy · operator override</div>
            <div className="muted" style={{fontSize: 10}}>Provide the exact Uniswap V2-compatible pair. The bot verifies the pair contains configured WETH and submits through SniperVault; this does not run the automatic honeypot probe.</div>
            <div style={{display: "flex", gap: 6, flexWrap: "wrap"}}>
              <input value={manualBuyToken} onChange={(e) => setManualBuyToken(e.target.value)} placeholder="token 0x…" style={{...inputStyle, flex: "1 1 220px", width: 0}} />
              <input value={manualBuyPair} onChange={(e) => setManualBuyPair(e.target.value)} placeholder="V2 pair 0x…" style={{...inputStyle, flex: "1 1 220px", width: 0}} />
              <input value={manualBuySizeEth} onChange={(e) => setManualBuySizeEth(e.target.value)} placeholder="ETH size" inputMode="decimal" style={{...inputStyle, flex: "0 0 100px", width: 100}} />
              <button onClick={() => void handleManualBuy()} disabled={isSaving || !isArmed} style={{...chipButtonStyle, color: "var(--amber)", borderColor: "var(--amber)", padding: "6px 10px"}}>{isArmed ? "submit manual buy" : "lane not armed"}</button>
            </div>
          </div>

          {/* Unified Holdings Table */}
          {portfolioSubTab === "all" && (
            <div style={{overflowX: "auto"}}>
              {filteredHoldings.length === 0 ? (
                <div className="muted" style={{padding: "24px 0", textAlign: "center", fontSize: 12}}>No bot positions or connected-wallet token balances yet.</div>
              ) : (
                <table className="grid" style={{width: "100%", fontSize: 12}}>
                  <thead><tr><th>TOKEN</th><th>SOURCE</th><th style={{textAlign: "right"}}>ENTRY</th><th style={{textAlign: "right"}}>MARK</th><th style={{textAlign: "right"}}>UNREALIZED PNL</th><th style={{textAlign: "right"}}>WALLET BALANCE</th><th>ACTION</th></tr></thead>
                  <tbody>{filteredHoldings.map((holding) => {
                    const positive = BigInt(holding.unrealizedPnlWei || "0") > 0n;
                    const negative = BigInt(holding.unrealizedPnlWei || "0") < 0n;
                    const links = getAggregatorLinks(holding.token, currentChainId, activeChainSlug);
                    return <tr key={holding.token}>
                      <td><strong>{holding.symbol}</strong><div className="muted" style={{fontSize: 10}}>{shortHash(holding.token, 6)}</div></td>
                      <td>
                        <div style={{display: "flex", gap: 3, flexWrap: "wrap"}}>
                          {holding.inBot && (
                            <span
                              className="badge"
                              style={{
                                fontSize: 9,
                                color: holding.executionMode === "simulation" ? "var(--cyan)" : "var(--green)",
                                borderColor: holding.executionMode === "simulation" ? "var(--cyan)" : "var(--green)",
                              }}
                            >
                              {holding.executionMode === "simulation" ? "SIMULATION" : "LIVE VAULT"}
                            </span>
                          )}
                          {holding.inWallet && (
                            <span className="badge" style={{fontSize: 9, color: "var(--muted)"}}>CONNECTED WALLET</span>
                          )}
                        </div>
                      </td>
                      <td style={{textAlign: "right"}}>{holding.inBot ? `${weiToEth(holding.entryCostWei, 4)} Ξ` : "—"}</td>
                      <td style={{textAlign: "right"}}>{holding.inBot ? `${weiToEth(holding.markValueWei, 4)} Ξ` : "—"}</td>
                      <td style={{textAlign: "right", color: positive ? "var(--green)" : negative ? "var(--red)" : "var(--muted)"}}>{holding.inBot ? `${signedEth(holding.unrealizedPnlWei, 4)} Ξ (${bpsFormatted(holding.netPnlBps)})` : "—"}</td>
                      <td style={{textAlign: "right"}}>{holding.walletBalance || "—"}</td>
                      <td><div style={{display: "inline-flex", gap: 4}}><button onClick={() => { setSwapTarget({token: holding.token, symbol: holding.symbol, pair: holding.pair}); setTab("swap"); }} style={{...chipButtonStyle, color: "var(--cyan)", borderColor: "var(--cyan)"}}>TRADE</button><a href={links.dexscreener} target="_blank" rel="noreferrer" style={{...chipButtonStyle, textDecoration: "none"}}>CHART ↗</a></div></td>
                    </tr>;
                  })}</tbody>
                </table>
              )}
            </div>
          )}

          {/* Active Open Positions Table */}
          {portfolioSubTab === "open" && (
            <div>
              {filteredOpen.length === 0 ? (
                <div className="muted" style={{padding: "24px 0", textAlign: "center", fontSize: 12}}>
                  No active open positions currently held.
                  <br />
                  <span style={{fontSize: 11, color: "var(--muted)"}}>
                    When the sniper back-runs a new pair launch, your active position and live mark value will appear here.
                  </span>
                </div>
              ) : (
                <div style={{overflowX: "auto"}}>
                  <table className="grid" style={{width: "100%", fontSize: 12}}>
                    <thead>
                      <tr>
                        <th>TOKEN</th>
                        <th>VENUE</th>
                        <th style={{textAlign: "right"}}>ENTRY (ETH)</th>
                        <th style={{textAlign: "right"}}>CURRENT MARK</th>
                        <th style={{textAlign: "right"}}>UNREALIZED PNL</th>
                        <th style={{textAlign: "right"}}>AGE</th>
                        <th style={{textAlign: "center"}}>QUICK ACTIONS (SELL / SWAP)</th>
                      </tr>
                    </thead>
                    <tbody>
                      {filteredOpen.map((pos) => {
                        const aggLinks = getAggregatorLinks(pos.token, currentChainId, activeChainSlug);
                        const isPos = BigInt(pos.unrealizedPnlWei || "0") > 0n;
                        const isNeg = BigInt(pos.unrealizedPnlWei || "0") < 0n;
                        return (
                          <tr key={pos.id} style={{opacity: pos.markStale ? 0.7 : 1}}>
                            <td>
                              <div style={{display: "flex", alignItems: "center", gap: 6}}>
                                <span
                                  style={{
                                    width: 7,
                                    height: 7,
                                    borderRadius: "50%",
                                    background: STATE_COLOR[pos.state] || "var(--green)",
                                  }}
                                />
                                <strong>{pos.symbol || shortHash(pos.token, 4)}</strong>
                                <span
                                  className="badge"
                                  style={{
                                    fontSize: 8,
                                    color: pos.executionMode === "simulation" ? "var(--cyan)" : "var(--green)",
                                    borderColor: pos.executionMode === "simulation" ? "var(--cyan)" : "var(--green)",
                                  }}
                                  title={pos.executionMode === "simulation" ? "Simulation position — local Anvil fixture, paper settlement" : "Live position — production SniperVault, on-chain settlement"}
                                >
                                  {pos.executionMode === "simulation" ? "SIMULATION" : "LIVE VAULT"}
                                </span>
                                {pos.executionMode !== "simulation" && (
                                <a
                                  href={addressUrl(currentChainId, pos.token) || undefined}
                                  target="_blank"
                                  rel="noreferrer"
                                  className="muted"
                                  style={{fontSize: 10, textDecoration: "none"}}
                                  title="View on Explorer"
                                >
                                  ↗
                                </a>
                                )}
                              </div>
                              <div className="muted" style={{fontSize: 10}}>
                                {shortHash(pos.token, 6)}
                              </div>
                            </td>
                            <td>
                              <span className="badge" style={{fontSize: 9, color: "var(--cyan)"}}>
                                {pos.venue}
                              </span>
                            </td>
                            <td style={{textAlign: "right", fontVariantNumeric: "tabular-nums"}}>
                              {weiToEth(pos.entryCostWei, 4)} Ξ
                            </td>
                            <td style={{textAlign: "right", fontVariantNumeric: "tabular-nums"}}>
                              {weiToEth(pos.markValueWei, 4)} Ξ
                              {pos.markStale && (
                                <span style={{color: "var(--amber)", fontSize: 9, marginLeft: 4}}>STALE</span>
                              )}
                            </td>
                            <td
                              style={{
                                textAlign: "right",
                                fontVariantNumeric: "tabular-nums",
                                color: isPos ? "var(--green)" : isNeg ? "var(--red)" : "var(--muted)",
                              }}
                            >
                              <div>{signedEth(pos.unrealizedPnlWei, 4)} Ξ</div>
                              <div style={{fontSize: 10}}>{bpsFormatted(pos.netPnlBps)}</div>
                            </td>
                            <td style={{textAlign: "right", color: "var(--muted)", fontSize: 11}}>
                              {ago(pos.openedAtMs)}
                            </td>
                            <td style={{textAlign: "center"}}>
                              <div style={{display: "inline-flex", gap: 4}}>
                                <button
                                  onClick={() => {
                                    setSwapTarget({
                                      token: pos.token,
                                      symbol: pos.symbol || "TOKEN",
                                      pair: pos.pair,
                                      qty: pos.remainingQty,
                                      markEth: weiToEth(pos.markValueWei, 4),
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
                                <button
                                  onClick={() => void handleManualSell(pos.id, 5000)}
                                  disabled={isSaving}
                                  style={{...chipButtonStyle, padding: "3px 6px", color: "var(--amber)", borderColor: "var(--amber)"}}
                                  title="Sell 50% through the bot's SniperVault"
                                >
                                  50% EXIT
                                </button>
                                <button
                                  onClick={() => void handleManualSell(pos.id, 10000)}
                                  disabled={isSaving}
                                  style={{...chipButtonStyle, padding: "3px 6px", color: "var(--red)", borderColor: "var(--red)"}}
                                  title="Sell 100% through the bot's SniperVault"
                                >
                                  100% EXIT
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

          {/* Connected Wallet Holdings View */}
          {portfolioSubTab === "wallet" && (
            <div style={{display: "grid", gap: 10}}>
              {!wallet.address ? (
                <div style={{textAlign: "center", padding: 20, color: "var(--muted)"}}>
                  Connect your wallet to view tokens in your address and sell them directly.
                  <div style={{marginTop: 8}}>
                    <button
                      onClick={() => wallet.connect()}
                      style={{
                        padding: "6px 14px",
                        background: "var(--panel-2)",
                        border: "1px solid var(--cyan)",
                        color: "var(--cyan)",
                        borderRadius: 4,
                        cursor: "pointer",
                      }}
                    >
                      Connect Wallet
                    </button>
                  </div>
                </div>
              ) : (
                <>
                  <div style={{display: "flex", gap: 8, alignItems: "center"}}>
                    <input
                      value={customTokenInput}
                      onChange={(e) => setCustomTokenInput(e.target.value)}
                      placeholder="Add custom ERC20 Token Address to scan & sell..."
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
                      style={{
                        padding: "5px 12px",
                        background: "var(--panel-2)",
                        border: "1px solid var(--line)",
                        color: "var(--cyan)",
                        borderRadius: 3,
                        cursor: "pointer",
                        fontSize: 11,
                      }}
                    >
                      Scan Token
                    </button>
                  </div>

                  {walletTokens.length === 0 ? (
                    <div className="muted" style={{padding: 16, textAlign: "center", fontSize: 11}}>
                      {isScanningWallet
                        ? "Scanning connected wallet for token balances..."
                        : "No balances found for sniped tokens in your connected wallet. Paste a token address above to inspect & sell."}
                    </div>
                  ) : (
                    <table className="grid" style={{width: "100%", fontSize: 12}}>
                      <thead>
                        <tr>
                          <th>TOKEN</th>
                          <th>CONTRACT ADDRESS</th>
                          <th style={{textAlign: "right"}}>BALANCE</th>
                          <th style={{textAlign: "center"}}>AGGREGATOR ACTIONS</th>
                        </tr>
                      </thead>
                      <tbody>
                        {walletTokens.map((t) => {
                          const links = getAggregatorLinks(t.address, currentChainId, activeChainSlug);
                          return (
                            <tr key={t.address}>
                              <td>
                                <strong>{t.symbol}</strong>
                              </td>
                              <td className="muted" style={{fontSize: 11}}>
                                {shortHash(t.address, 6)}
                              </td>
                              <td style={{textAlign: "right", fontVariantNumeric: "tabular-nums"}}>
                                <strong>{Number(t.balance).toLocaleString(undefined, {maximumFractionDigits: 4})}</strong>
                              </td>
                              <td style={{textAlign: "center"}}>
                                <div style={{display: "inline-flex", gap: 4}}>
                                  <a
                                    href={links.oneInch}
                                    target="_blank"
                                    rel="noreferrer"
                                    style={{
                                      padding: "3px 8px",
                                      background: "rgba(34, 211, 238, 0.15)",
                                      border: "1px solid var(--cyan)",
                                      color: "var(--cyan)",
                                      borderRadius: 3,
                                      fontSize: 10,
                                      textDecoration: "none",
                                      fontWeight: 600,
                                    }}
                                  >
                                    1inch Swap ↗
                                  </a>
                                  <a
                                    href={links.uniswap}
                                    target="_blank"
                                    rel="noreferrer"
                                    style={{
                                      padding: "3px 6px",
                                      background: "var(--panel-2)",
                                      border: "1px solid var(--line)",
                                      color: "var(--text)",
                                      borderRadius: 3,
                                      fontSize: 10,
                                      textDecoration: "none",
                                    }}
                                  >
                                    Uniswap ↗
                                  </a>
                                  <a
                                    href={links.dexscreener}
                                    target="_blank"
                                    rel="noreferrer"
                                    style={{
                                      padding: "3px 6px",
                                      background: "var(--panel-2)",
                                      border: "1px solid var(--line)",
                                      color: "var(--text)",
                                      borderRadius: 3,
                                      fontSize: 10,
                                      textDecoration: "none",
                                    }}
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
                  )}
                </>
              )}
            </div>
          )}

          {/* Closed Positions History */}
          {portfolioSubTab === "closed" && (
            <div style={{overflowX: "auto"}}>
              {filteredClosed.length === 0 ? (
                <div className="muted" style={{padding: 20, textAlign: "center", fontSize: 11}}>
                  No closed positions yet.
                </div>
              ) : (
                <table className="grid" style={{width: "100%", fontSize: 12}}>
                  <thead>
                    <tr>
                      <th>TOKEN</th>
                      <th style={{textAlign: "right"}}>ENTRY (ETH)</th>
                      <th style={{textAlign: "right"}}>REALISED (ETH)</th>
                      <th style={{textAlign: "right"}}>NET PNL</th>
                      <th>EXIT REASON</th>
                      <th style={{textAlign: "right"}}>CLOSED</th>
                    </tr>
                  </thead>
                  <tbody>
                    {filteredClosed.map((pos) => (
                      <tr key={pos.id}>
                        <td>
                          <strong>{pos.symbol || shortHash(pos.token, 4)}</strong>
                          <span className="muted" style={{marginLeft: 6, fontSize: 10}}>
                            {pos.venue}
                          </span>
                        </td>
                        <td style={{textAlign: "right"}}>{weiToEth(pos.entryCostWei, 4)} Ξ</td>
                        <td style={{textAlign: "right", color: pnlColor(pos.realizedWei)}}>
                          {weiToEth(pos.realizedWei, 4)} Ξ
                        </td>
                        <td style={{textAlign: "right", color: pnlColor(pos.netPnlWei)}}>
                          {signedEth(pos.netPnlWei, 4)} Ξ ({bpsFormatted(pos.netPnlBps)})
                        </td>
                        <td className="muted" style={{fontSize: 11}}>
                          {pos.exitReason ? EXIT_LABEL[pos.exitReason] || pos.exitReason : "Closed"}
                        </td>
                        <td style={{textAlign: "right", color: "var(--muted)", fontSize: 11}}>
                          {pos.closedAtMs ? ago(pos.closedAtMs) : "—"}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              )}
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
                  onChange={(e) => { isFormDirty.current = true; setFormBuySizeEth(e.target.value); }}
                  style={inputStyle}
                  placeholder="e.g. 0.05"
                />
                <div style={{display: "flex", gap: 4, marginTop: 4}}>
                  {["0.01", "0.025", "0.05", "0.1", "0.25"].map((val) => (
                    <button
                      key={val}
                      onClick={() => { isFormDirty.current = true; setFormBuySizeEth(val); }}
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
                  onChange={(e) => { isFormDirty.current = true; setFormDailyBudgetEth(e.target.value); }}
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
                  onChange={(e) => { isFormDirty.current = true; setFormTotalBudgetEth(e.target.value); }}
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
                  onChange={(e) => { isFormDirty.current = true; setFormMaxPositions(e.target.value); }}
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
                  onChange={(e) => { isFormDirty.current = true; setFormTakeProfitPct(e.target.value); }}
                  style={inputStyle}
                  placeholder="e.g. 100"
                />
                <div style={{display: "flex", gap: 4, marginTop: 4}}>
                  {["25", "50", "100", "200", "500"].map((val) => (
                    <button
                      key={val}
                      onClick={() => { isFormDirty.current = true; setFormTakeProfitPct(val); }}
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
                  onChange={(e) => { isFormDirty.current = true; setFormTakeProfitAbsEth(e.target.value); }}
                  style={inputStyle}
                  placeholder="0 (off)"
                />
                <span className="muted" style={{fontSize: 10}}>
                  Triggers if position reaches either +% gain OR this ETH profit.
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
                  onChange={(e) => { isFormDirty.current = true; setFormSellFractionPct(e.target.value); }}
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
                      onClick={() => setFormSellFractionPct(item.val)}
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
                  onChange={(e) => { isFormDirty.current = true; setFormStopLossPct(e.target.value); }}
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
                  onChange={(e) => { isFormDirty.current = true; setFormTrailingStopPct(e.target.value); }}
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
                  onChange={(e) => { isFormDirty.current = true; setFormMaxHoldMins(e.target.value); }}
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
                  onChange={(e) => { isFormDirty.current = true; setFormRequireHoneypot(e.target.checked); }}
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
                  onChange={(e) => { isFormDirty.current = true; setFormMinLiquidityEth(e.target.value); }}
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
                  onChange={(e) => { isFormDirty.current = true; setFormMaxPriceImpactPct(e.target.value); }}
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
                    onChange={(e) => { isFormDirty.current = true; setFormMaxBuyTaxPct(e.target.value); }}
                    style={inputStyle}
                  />
                </div>
                <div>
                  <label style={{display: "block", fontSize: 10, marginBottom: 2}}>Max Sell Tax (%)</label>
                  <input
                    type="number"
                    step="0.5"
                    value={formMaxSellTaxPct}
                    onChange={(e) => { isFormDirty.current = true; setFormMaxSellTaxPct(e.target.value); }}
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
                  onChange={(e) => { isFormDirty.current = true; setFormMinHoldBlocks(e.target.value); }}
                  style={inputStyle}
                />
              </div>

              {/* LP Locked Check */}
              <div style={{display: "flex", alignItems: "center", gap: 8}}>
                <input
                  type="checkbox"
                  id="reqLp"
                  checked={formRequireLpLocked}
                  onChange={(e) => { isFormDirty.current = true; setFormRequireLpLocked(e.target.checked); }}
                  style={{cursor: "pointer"}}
                />
                <label htmlFor="reqLp" style={{fontSize: 11, cursor: "pointer"}}>
                  Require LP Burned / Locked
                  <span
                    className="muted"
                    style={{display: "block", fontSize: 9, marginTop: 1, cursor: "pointer"}}
                  >
                    on-chain probe: ≥95% of V2-style LP supply (incl. Aerodrome volatile) in burn
                    addresses; unprobable venues (UniV3) and failed probes read as not-locked
                  </span>
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
                onClick={() => { isFormDirty.current = false; populateFormFromConfig(cfg.params); }}
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
          TAB 3: TRADE / CHARTS TERMINAL
          ──────────────────────────────────────────────────────────────────────── */}
      {tab === "swap" && (
        <TradeTerminal
          pf={pf}
          wallet={wallet}
          publicClient={publicClient}
          activeChainSlug={activeChainSlug}
          currentChainId={currentChainId}
          initialTarget={swapTarget}
          sniperMode={modeInfo?.sniperMode ?? cfg?.sniperMode ?? (cfg?.paperMode ? "simulation" : "live")}
        />
      )}

      {/* ────────────────────────────────────────────────────────────────────────
          TAB 4: SAFETY GATES & HONEYPOT DIAGNOSTICS
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

      {/* Sniper mode confirmation dialogs */}
      {modeDialog !== "none" && (
        <div
          role="dialog"
          aria-modal="true"
          aria-label={modeDialog === "confirm-live" ? "Switch sniper to live" : "Switch sniper to simulation"}
          onClick={() => (modeBusy ? undefined : setModeDialog("none"))}
          style={{
            position: "fixed",
            inset: 0,
            background: "rgba(0,0,0,0.65)",
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            zIndex: 100,
          }}
        >
          <div
            onClick={(e) => e.stopPropagation()}
            className="panel"
            style={{maxWidth: 560, width: "92vw", padding: 16}}
          >
            {modeDialog === "confirm-live" ? (
              <div style={{display: "grid", gap: 10}}>
                <div style={{fontWeight: 700, color: "#ff5c5c"}}>Switch Sniper to LIVE</div>
                <p className="muted" style={{margin: 0, fontSize: 12, lineHeight: 1.6}}>
                  Sniper Live submits real trades through the configured bot signer and
                  SniperVault. This control does <b>not</b> change the atomic MEV engine mode. A
                  successful buy can lose the full amount — the sniper has no profit-or-revert
                  guard, only bounded-loss budgets.
                </p>
                <div style={{fontSize: 11, display: "grid", gap: 4, background: "#040608", border: "1px solid #1b2532", borderRadius: 4, padding: "8px 10px"}}>
                  <div><span className="muted">Chain:</span> {activeChainSlug} (id {currentChainId})</div>
                  <div><span className="muted">Production vault:</span> {modeInfo?.productionVaultAddress ?? cfg?.productionVaultAddress ?? cfg?.params.vaultAddress ?? "not configured"}</div>
                  <div><span className="muted">Connected owner wallet:</span> {wallet.address ?? "not connected"}</div>
                  <div><span className="muted">Bot searcher (SNIPER_SEARCHER_ADDRESS):</span> {modeInfo?.fixture?.searcher ?? "see /api/bot/config"}</div>
                  <div><span className="muted">Daily budget:</span> {ethFromWei(cfg?.params.dailyBudgetWei ?? "0", 4)} Ξ · <span className="muted">Total:</span> {cfg?.params.totalBudgetWei === "0" ? "unlimited" : `${ethFromWei(cfg?.params.totalBudgetWei ?? "0", 4)} Ξ`}</div>
                  <div><span className="muted">Buy size:</span> {ethFromWei(cfg?.params.buySizeWei ?? "0", 4)} Ξ</div>
                </div>
                {(!modeInfo?.canSwitchLive) && modeInfo && (
                  <div style={{fontSize: 11, color: "var(--red)"}}>
                    {modeInfo.blockers.map((b) => (
                      <div key={b}>✗ {b}</div>
                    ))}
                  </div>
                )}
                {modeNote && <div style={{fontSize: 11, color: "var(--red)"}} role="alert">{modeNote}</div>}
                <div style={{display: "flex", gap: 8}}>
                  <button
                    onClick={() => void handleSetSniperMode("live")}
                    disabled={modeBusy || !modeInfo?.canSwitchLive}
                    style={{...chipButtonStyle, borderColor: "#ff5c5c", color: "#ff5c5c", padding: "6px 12px", fontSize: 12}}
                  >
                    {modeBusy ? "switching…" : "go live"}
                  </button>
                  <button onClick={() => setModeDialog("none")} disabled={modeBusy} style={{...chipButtonStyle, padding: "6px 12px", fontSize: 12}}>
                    cancel
                  </button>
                </div>
              </div>
            ) : (
              <div style={{display: "grid", gap: 10}}>
                <div style={{fontWeight: 700, color: "var(--cyan)"}}>Switch Sniper to SIMULATION</div>
                <p className="muted" style={{margin: 0, fontSize: 12, lineHeight: 1.6}}>
                  New entries run contract-backed trades on the local Anvil fixture and cannot
                  spend real funds. Live positions and receipts <b>remain live data</b> — they
                  keep live exit management and are never converted into paper positions. Flatten
                  them explicitly before any migration or handoff.
                </p>
                {modeNote && <div style={{fontSize: 11, color: "var(--red)"}} role="alert">{modeNote}</div>}
                <div style={{display: "flex", gap: 8}}>
                  <button
                    onClick={() => void handleSetSniperMode("simulation")}
                    disabled={modeBusy}
                    style={{...chipButtonStyle, borderColor: "var(--cyan)", color: "var(--cyan)", padding: "6px 12px", fontSize: 12}}
                  >
                    {modeBusy ? "switching…" : "pause to simulation"}
                  </button>
                  <button onClick={() => setModeDialog("none")} disabled={modeBusy} style={{...chipButtonStyle, padding: "6px 12px", fontSize: 12}}>
                    cancel
                  </button>
                </div>
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

/**
 * The sniper's independent SIMULATION/LIVE control.
 *
 * Rendered as an actual segmented control (two buttons with `aria-pressed`),
 * never a passive badge. The atomic MEV engine's mode is shown only as
 * context underneath — flipping this control never changes it.
 */
function SniperModeControl({
  modeInfo,
  armed,
  onOpenDialog,
}: {
  modeInfo: SniperModeResponse | null;
  armed: boolean;
  onOpenDialog: (dialog: "none" | "confirm-live" | "confirm-sim") => void;
}) {
  const sniperMode: "simulation" | "live" =
    modeInfo?.sniperMode ?? (armed ? "live" : "simulation");
  const atomicMode = modeInfo?.atomicMode;

  const seg = (value: "simulation" | "live", label: string, tone: string): React.ReactNode => {
    const active = sniperMode === value;
    return (
      <button
        onClick={() => {
          if (value === sniperMode) return;
          onOpenDialog(value === "live" ? "confirm-live" : "confirm-sim");
        }}
        aria-pressed={active}
        title={
          value === "live"
            ? "Sniper Live submits real trades through the configured vault and bot signer"
            : "Sniper Simulation runs contract-backed trades on a local fork and cannot spend real funds"
        }
        style={{
          padding: "4px 10px",
          fontSize: 11,
          fontWeight: 700,
          background: active ? (value === "live" ? "rgba(255,92,92,0.18)" : "rgba(34,211,238,0.15)") : "transparent",
          color: active ? tone : "var(--muted)",
          border: `1px solid ${active ? tone : "var(--line)"}`,
          borderRadius: value === "simulation" ? "4px 0 0 4px" : "0 4px 4px 0",
          cursor: "pointer",
          fontFamily: "inherit",
        }}
      >
        {label}
      </button>
    );
  };

  return (
    <div style={{display: "flex", flexDirection: "column", gap: 2}}>
      <div style={{display: "flex", alignItems: "center", gap: 6}}>
        <span className="muted" style={{fontSize: 10, letterSpacing: "0.05em"}}>SNIPER MODE</span>
        <div role="group" aria-label="Sniper execution mode" style={{display: "inline-flex"}}>
          {seg("simulation", "SIMULATION", "var(--cyan)")}
          {seg("live", "LIVE", "#ff5c5c")}
        </div>
      </div>
      <span className="muted" style={{fontSize: 10}}>
        ATOMIC MEV: {atomicMode ? atomicMode.toUpperCase() : "—"} · this control does not change it
      </span>
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
