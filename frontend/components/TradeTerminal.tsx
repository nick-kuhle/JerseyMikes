"use client";

import {useCallback, useEffect, useMemo, useState, type CSSProperties} from "react";
import {createWalletClient, custom, encodeFunctionData, formatEther, formatUnits, isAddress, parseEther, parseUnits, type Address, type PublicClient} from "viem";
import type {WalletState} from "@/lib/wallet";
import {
  calculatePlatformFee,
  dexScreenerEmbedUrl,
  dexToolsEmbedUrl,
  ERC20_ABI,
  getAggregatorLinks,
  PLATFORM_FEE_ROUTER_ABI,
  platformFeeConfig,
  V2_FACTORY_ABI,
  V2_FACTORY_BY_CHAIN,
  V2_ROUTER_ABI,
  V2_ROUTER_BY_CHAIN,
  type TradeRoutePreference,
  type TradeSide,
} from "@/lib/swap";
import type {SniperPortfolio} from "@/lib/types";

const PAIR_ABI = [
  {type: "function", name: "token0", stateMutability: "view", inputs: [], outputs: [{type: "address"}]},
  {type: "function", name: "getReserves", stateMutability: "view", inputs: [], outputs: [{name: "reserve0", type: "uint112"}, {name: "reserve1", type: "uint112"}, {name: "blockTimestampLast", type: "uint32"}]},
] as const;
const GAS_RESERVE = parseEther("0.005");
const WETH_BY_CHAIN: Record<string, Address> = {
  ethereum: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
  base: "0x4200000000000000000000000000000000000006",
};

type Target = {token: string; symbol: string; pair?: string; qty?: string; markEth?: string} | null;
type TokenDetails = {token: Address; symbol: string; decimals: number; pair: Address | null; token0: Address | null; reserve0: bigint; reserve1: bigint};

function short(value: string | null | undefined) { return value && value.length > 14 ? `${value.slice(0, 10)}…${value.slice(-4)}` : value || "—"; }
function errorText(error: unknown) { return (error instanceof Error ? error.message : String(error)).split("\n")[0]; }
function quoteV2(amountIn: bigint, reserveIn: bigint, reserveOut: bigint): bigint {
  if (amountIn <= 0n || reserveIn <= 0n || reserveOut <= 0n) return 0n;
  const withFee = amountIn * 997n;
  return (withFee * reserveOut) / (reserveIn * 1000n + withFee);
}
function amountForPercent(balance: bigint, pct: number, buy: boolean): bigint {
  if (buy) return balance > GAS_RESERVE ? ((balance - GAS_RESERVE) * BigInt(pct)) / 100n : 0n;
  return (balance * BigInt(pct)) / 100n;
}

export default function TradeTerminal({pf, wallet: walletProp, publicClient, activeChainSlug, currentChainId, initialTarget}: {
  pf: SniperPortfolio;
  wallet: WalletState;
  publicClient: Pick<PublicClient, "readContract" | "waitForTransactionReceipt">;
  activeChainSlug: string;
  currentChainId: number;
  initialTarget: Target;
}) {
  // `walletProp` is passed from SniperPanel so this terminal shares the same
  // EIP-6963 connection and provider event subscriptions.
  const wallet = walletProp;
  const [tokenInput, setTokenInput] = useState(initialTarget?.token || "");
  const [details, setDetails] = useState<TokenDetails | null>(null);
  const [side, setSide] = useState<TradeSide>("buy");
  const [route, setRoute] = useState<TradeRoutePreference>("best_price");
  const [amount, setAmount] = useState("");
  const [slippage, setSlippage] = useState("1.0");
  const [lookupBusy, setLookupBusy] = useState(false);
  const [tradeBusy, setTradeBusy] = useState(false);
  const [notice, setNotice] = useState<{tone: "good" | "bad" | "warn"; text: string} | null>(null);
  const [walletTokenBalance, setWalletTokenBalance] = useState<bigint>(0n);
  const [tokenBalance, setTokenBalance] = useState<bigint>(0n);

  const feeConfig = useMemo(() => platformFeeConfig(), []);
  const router = V2_ROUTER_BY_CHAIN[activeChainSlug] || V2_ROUTER_BY_CHAIN.ethereum;
  const weth = WETH_BY_CHAIN[activeChainSlug] || WETH_BY_CHAIN.ethereum;
  const aggregatorLinks = details ? getAggregatorLinks(details.token, currentChainId, activeChainSlug) : null;

  const resolveToken = useCallback(async (raw: string, target?: Target) => {
    if (!isAddress(raw)) { setNotice({tone: "bad", text: "Paste a valid ERC-20 contract address."}); return; }
    setLookupBusy(true); setNotice(null);
    try {
      const token = raw as Address;
      const [symbol, decimals] = await Promise.all([
        publicClient.readContract({address: token, abi: ERC20_ABI, functionName: "symbol"}).catch(() => short(token)),
        publicClient.readContract({address: token, abi: ERC20_ABI, functionName: "decimals"}).catch(() => 18),
      ]);
      let pair: Address | null = target?.pair && isAddress(target.pair) ? target.pair as Address : null;
      if (!pair) {
        const factory = V2_FACTORY_BY_CHAIN[activeChainSlug] || V2_FACTORY_BY_CHAIN.ethereum;
        pair = await publicClient.readContract({address: factory, abi: V2_FACTORY_ABI, functionName: "getPair", args: [weth, token]}).catch(() => null) as Address | null;
        if (pair === "0x0000000000000000000000000000000000000000") pair = null;
      }
      let token0: Address | null = null; let reserve0 = 0n; let reserve1 = 0n;
      if (pair) {
        const [zero, reserves] = await Promise.all([
          publicClient.readContract({address: pair, abi: PAIR_ABI, functionName: "token0"}),
          publicClient.readContract({address: pair, abi: PAIR_ABI, functionName: "getReserves"}),
        ]);
        token0 = zero as Address;
        const r = reserves as readonly [bigint, bigint, number];
        reserve0 = BigInt(r[0]); reserve1 = BigInt(r[1]);
      }
      setDetails({token, symbol: String(symbol), decimals: Number(decimals), pair, token0, reserve0, reserve1});
      setTokenInput(token);
      if (wallet.address) {
        const balance = await publicClient.readContract({address: token, abi: ERC20_ABI, functionName: "balanceOf", args: [wallet.address as Address]}).catch(() => 0n);
        setWalletTokenBalance(BigInt(balance as bigint));
      }
      setAmount(target?.qty && Number(decimals) === 18 ? formatUnits(BigInt(target.qty), 18) : "");
    } catch (error) { setDetails(null); setNotice({tone: "bad", text: `Token lookup failed: ${errorText(error)}`}); }
    finally { setLookupBusy(false); }
  }, [activeChainSlug, publicClient, wallet.address, weth]);

  useEffect(() => {
    if (initialTarget?.token && initialTarget.token !== tokenInput) {
      setTokenInput(initialTarget.token);
      void resolveToken(initialTarget.token, initialTarget);
    } else if (initialTarget?.token && !details) {
      void resolveToken(initialTarget.token, initialTarget);
    }
  }, [details, initialTarget, resolveToken, tokenInput]);

  useEffect(() => {
    if (!details || !wallet.address) { setTokenBalance(0n); return; }
    publicClient.readContract({address: details.token, abi: ERC20_ABI, functionName: "balanceOf", args: [wallet.address as Address]})
      .then((value) => setTokenBalance(BigInt(value as bigint))).catch(() => setTokenBalance(0n));
  }, [details, publicClient, wallet.address]);

  const maxBuy = wallet.balanceWei && wallet.balanceWei > GAS_RESERVE ? wallet.balanceWei - GAS_RESERVE : 0n;
  const maxSell = tokenBalance;
  const amountIn = useMemo(() => {
    try { return side === "buy" ? parseEther(amount || "0") : parseUnits(amount || "0", details?.decimals ?? 18); } catch { return 0n; }
  }, [amount, details?.decimals, side]);
  const quote = useMemo(() => {
    if (!details?.pair || !details.token0 || amountIn === 0n) return 0n;
    const tokenIs0 = details.token0.toLowerCase() === details.token.toLowerCase();
    const reserveToken = tokenIs0 ? details.reserve0 : details.reserve1;
    const reserveWeth = tokenIs0 ? details.reserve1 : details.reserve0;
    return side === "buy" ? quoteV2(amountIn, reserveWeth, reserveToken) : quoteV2(amountIn, reserveToken, reserveWeth);
  }, [amountIn, details, side]);
  const slippageBps = Math.max(0, Math.min(5_000, Math.round(Number(slippage || "1") * 100)));
  const minOut = (quote * BigInt(10_000 - slippageBps)) / 10_000n;
  const fee = calculatePlatformFee(amountIn);
  const feeConfigured = Boolean(feeConfig.feeRecipient);
  const feeReady = !feeConfigured || Boolean(feeConfig.feeRouter);
  const chartKey = details?.pair || (isAddress(tokenInput) ? tokenInput : "");

  const setPreset = (pct: number) => {
    const base = side === "buy" ? maxBuy : maxSell;
    const value = amountForPercent(base, pct, side === "buy");
    try { setAmount(side === "buy" ? formatEther(value) : formatUnits(value, details?.decimals ?? 18)); } catch { setAmount("0"); }
  };

  const execute = async () => {
    if (!details || !wallet.address || !wallet.eip1193 || !details.pair || amountIn <= 0n || quote <= 0n) {
      setNotice({tone: "bad", text: "Connect a wallet and resolve a liquid V2 pair before trading."}); return;
    }
    if (wallet.chainId !== currentChainId) {
      setNotice({tone: "bad", text: `Switch the wallet to the active console chain (${activeChainSlug}) before signing.`}); return;
    }
    if (route === "mev_safe") {
      setNotice({tone: "warn", text: "Browser wallet execution cannot guarantee private ordering. Configure a private bot/relay route before using MEV-Safe."});
      return;
    }
    if (!feeReady) { setNotice({tone: "bad", text: "Treasury is configured but PLATFORM_FEE_ROUTER_ADDRESS is not configured; trade blocked to prevent fee bypass."}); return; }
    if (!window.confirm(`${side === "buy" ? "Buy" : "Sell"} ${amount} ${side === "buy" ? "ETH" : details.symbol} with ${Number(slippage).toFixed(2)}% slippage?`)) return;
    setTradeBusy(true); setNotice(null);
    try {
      const deadline = BigInt(Math.floor(Date.now() / 1000) + 300);
      const path = side === "buy" ? [weth, details.token] : [details.token, weth];
      const swapCalldata = side === "buy"
        ? encodeFunctionData({abi: V2_ROUTER_ABI, functionName: "swapExactETHForTokensSupportingFeeOnTransferTokens", args: [minOut, path, wallet.address as Address, deadline]})
        : encodeFunctionData({abi: V2_ROUTER_ABI, functionName: "swapExactTokensForETHSupportingFeeOnTransferTokens", args: [amountIn, minOut, path, wallet.address as Address, deadline]});
      const walletClient = createWalletClient({transport: custom(wallet.eip1193)});
      const spender = feeConfigured ? feeConfig.feeRouter! : router;
      if (side === "sell") {
        const allowance = await publicClient.readContract({address: details.token, abi: ERC20_ABI, functionName: "allowance", args: [wallet.address as Address, spender]}).catch(() => 0n);
        if (BigInt(allowance as bigint) < amountIn) {
          const approval = await walletClient.writeContract({account: wallet.address as Address, address: details.token, abi: ERC20_ABI, functionName: "approve", args: [spender, amountIn], chain: null});
          await publicClient.waitForTransactionReceipt({hash: approval});
        }
      }
      let hash: `0x${string}`;
      if (feeConfigured) {
        hash = await walletClient.writeContract({account: wallet.address as Address, address: feeConfig.feeRouter!, abi: PLATFORM_FEE_ROUTER_ABI, functionName: "executeSwapWithFee", args: [side === "buy" ? "0x0000000000000000000000000000000000000000" : details.token, side === "buy" ? details.token : weth, amountIn, minOut, router, swapCalldata], value: side === "buy" ? amountIn : 0n, chain: null});
      } else {
        hash = side === "buy"
          ? await walletClient.writeContract({account: wallet.address as Address, address: router, abi: V2_ROUTER_ABI, functionName: "swapExactETHForTokensSupportingFeeOnTransferTokens", args: [minOut, path, wallet.address as Address, deadline], value: amountIn, chain: null})
          : await walletClient.writeContract({account: wallet.address as Address, address: router, abi: V2_ROUTER_ABI, functionName: "swapExactTokensForETHSupportingFeeOnTransferTokens", args: [amountIn, minOut, path, wallet.address as Address, deadline], chain: null});
      }
      await publicClient.waitForTransactionReceipt({hash});
      setNotice({tone: "good", text: `Trade confirmed: ${short(hash)}. ${feeConfigured ? "1% fee routed atomically to treasury." : "No platform fee charged; treasury is not configured."}`});
      setAmount("");
    } catch (error) { setNotice({tone: "bad", text: `Trade failed: ${errorText(error)}`}); }
    finally { setTradeBusy(false); }
  };

  const chart = chartKey ? dexScreenerEmbedUrl(activeChainSlug, chartKey) : "";
  const fallback = details?.pair ? dexToolsEmbedUrl(activeChainSlug, details.pair) : "";

  return (
    <section className="panel" style={{padding: 0, overflow: "hidden"}}>
      <div style={{padding: "10px 12px", borderBottom: "1px solid var(--line)", display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap"}}>
        <strong>📈 Trade / Charts</strong>
        <input value={tokenInput} onChange={(event) => setTokenInput(event.target.value)} onKeyDown={(event) => {if (event.key === "Enter") void resolveToken(tokenInput);}} placeholder="Paste ERC-20 address…" style={inputStyle} />
        <button style={buttonStyle} disabled={lookupBusy} onClick={() => void resolveToken(tokenInput)}>{lookupBusy ? "resolving…" : "lookup token"}</button>
        <span className="muted" style={{fontSize: 10}}>current chain: {activeChainSlug}</span>
      </div>
      <div style={{display: "grid", gridTemplateColumns: "minmax(0, 1.85fr) minmax(280px, 1fr)", gap: 0}}>
        <div style={{minHeight: 520, background: "#070b11", borderRight: "1px solid var(--line)"}}>
          <div style={{padding: "8px 12px", display: "flex", justifyContent: "space-between", gap: 8, flexWrap: "wrap"}}>
            <div><strong>{details?.symbol || "Select a token"}</strong>{details?.token && <span className="muted" style={{marginLeft: 8}}>{short(details.token)} · pair {short(details.pair)}</span>}</div>
            {details?.pair && <a href={aggregatorLinks?.dexscreener} target="_blank" rel="noreferrer" className="muted" style={{fontSize: 10}}>DexScreener ↗</a>}
          </div>
          {chart ? <iframe title="DexScreener live chart" src={chart} style={{width: "100%", height: 480, border: 0, borderRadius: 6}} /> : <div style={{height: 480, display: "grid", placeItems: "center", padding: 30, textAlign: "center"}} className="muted">Resolve a token address to load the embedded DexScreener chart. Token-level lookup remains available while a pair is pending.</div>}
          {fallback && <div style={{padding: "5px 12px", fontSize: 10}} className="muted">Chart fallback: <a href={fallback} target="_blank" rel="noreferrer" style={{color: "var(--cyan)"}}>DexTools ↗</a>. External embeds require network access in the operator browser.</div>}
        </div>
        <div style={{padding: 12, display: "grid", alignContent: "start", gap: 10}}>
          <div style={{fontWeight: 700, color: "var(--cyan)"}}>⚡ Quick Trade Console</div>
          <div style={{display: "flex", gap: 6}}>{(["buy", "sell"] as TradeSide[]).map((value) => <button key={value} onClick={() => {setSide(value); setAmount("");}} style={{...buttonStyle, flex: 1, color: side === value ? "#05240f" : "var(--text)", background: side === value ? "var(--cyan)" : "var(--panel-2)"}}>{value.toUpperCase()}</button>)}</div>
          <label style={labelStyle}>{side === "buy" ? "Amount (ETH)" : `Amount (${details?.symbol || "token"})`}<input value={amount} onChange={(event) => setAmount(event.target.value)} placeholder={side === "buy" ? "0.050" : "0.0"} inputMode="decimal" style={fieldStyle} /></label>
          <div style={{display: "grid", gridTemplateColumns: "repeat(4,1fr)", gap: 5}}>{[25, 50, 75].map((pct) => <button key={pct} style={chipStyle} onClick={() => setPreset(pct)}>{pct}%</button>)}<button style={{...chipStyle, color: "var(--amber)", borderColor: "var(--amber)"}} onClick={() => setPreset(100)}>MAX</button></div>
          <div className="muted" style={{fontSize: 10}}>Max {side === "buy" ? `reserves ${formatEther(GAS_RESERVE)} ETH gas` : `reads exact ${details?.symbol || "token"} balance`}</div>
          <label style={labelStyle}>Slippage tolerance<select value={slippage} onChange={(event) => setSlippage(event.target.value)} style={fieldStyle}><option value="0.5">0.5%</option><option value="1.0">1.0%</option><option value="3.0">3.0%</option></select></label>
          <label style={labelStyle}>Routing preference<div style={{display: "grid", gap: 5}}>{([["fastest", "⚡ Fastest"], ["mev_safe", "🛡️ MEV-Safe"], ["best_price", "💰 Best Price"]] as [TradeRoutePreference,string][]).map(([value, text]) => <button key={value} onClick={() => setRoute(value)} style={{...buttonStyle, textAlign: "left", color: route === value ? "var(--cyan)" : "var(--text)", borderColor: route === value ? "var(--cyan)" : "var(--line)"}}>{text}</button>)}</div></label>
          <div style={{background: "var(--panel-2)", padding: 9, borderRadius: 4, border: "1px solid var(--line)", fontSize: 11, display: "grid", gap: 4}}><div>Estimated output <strong>{quote ? (side === "buy" ? formatUnits(quote, details?.decimals ?? 18) : `${formatEther(quote)} ETH`) : "—"}</strong></div><div>Minimum after slippage <strong>{minOut ? (side === "buy" ? formatUnits(minOut, details?.decimals ?? 18) : `${formatEther(minOut)} ETH`) : "—"}</strong></div><div style={{color: feeConfigured ? "var(--amber)" : "var(--muted)"}}>Platform fee {feeConfigured ? "1.00%" : "0% · treasury not configured"} {fee ? `(${side === "buy" ? formatEther(fee) : formatUnits(fee, details?.decimals ?? 18)} ${side === "buy" ? "ETH" : details?.symbol || "token"})` : ""}</div></div>
          <button onClick={() => void execute()} disabled={tradeBusy || !details || !wallet.address || wallet.chainId !== currentChainId || !feeReady} style={{...executeButton, opacity: tradeBusy || !details || !wallet.address || wallet.chainId !== currentChainId || !feeReady ? 0.55 : 1}}>{tradeBusy ? "confirming…" : !wallet.address ? "connect wallet to trade" : wallet.chainId !== currentChainId ? `switch wallet to ${activeChainSlug}` : !feeReady ? "fee router not configured" : `execute ${side} · ${route.replace("_", " ")}`}</button>
          {notice && <div style={{fontSize: 10, color: notice.tone === "good" ? "var(--green)" : notice.tone === "bad" ? "var(--red)" : "var(--amber)"}} role="status">{notice.text}</div>}
          {aggregatorLinks && <div className="muted" style={{fontSize: 10}}>Research links: <a href={aggregatorLinks.oneInch} target="_blank" rel="noreferrer" style={{color: "var(--cyan)"}}>1inch</a> · <a href={aggregatorLinks.uniswap} target="_blank" rel="noreferrer" style={{color: "var(--cyan)"}}>Uniswap</a></div>}
        </div>
      </div>
    </section>
  );
}

const inputStyle: CSSProperties = {background: "#070b11", border: "1px solid var(--line)", borderRadius: 4, color: "var(--text)", padding: "6px 8px", fontFamily: "inherit", fontSize: 11, flex: "1 1 260px", minWidth: 180};
const buttonStyle: CSSProperties = {background: "#111a25", border: "1px solid #24334a", borderRadius: 4, color: "#d7e2f0", padding: "5px 9px", cursor: "pointer", fontFamily: "inherit", fontSize: 11};
const fieldStyle: CSSProperties = {...inputStyle, width: "100%", flex: "none", marginTop: 4};
const labelStyle: CSSProperties = {display: "grid", gap: 2, fontSize: 10, color: "var(--muted)"};
const chipStyle: CSSProperties = {...buttonStyle, padding: "5px 2px", color: "var(--cyan)"};
const executeButton: CSSProperties = {background: "var(--green)", border: "1px solid var(--green)", borderRadius: 4, color: "#05240f", padding: "8px 10px", fontWeight: 800, cursor: "pointer", fontFamily: "inherit", fontSize: 11};
