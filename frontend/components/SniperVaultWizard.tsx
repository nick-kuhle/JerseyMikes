"use client";

/**
 * Focused SniperVault onboarding flow for the Sniper panel.
 *
 * The larger Go-Live panel covers the complete platform. This compact wizard
 * keeps the sniper's critical path visible at the point where the operator
 * enables the lane: connect -> deploy -> allowlist -> fund/bind. It never
 * accepts or exposes a private key in the browser.
 */

import {useCallback, useEffect, useMemo, useState, type CSSProperties} from "react";
import {createPublicClient, createWalletClient, custom, encodeAbiParameters, formatEther, http, isAddress, parseEther, type Address} from "viem";
import {base, mainnet} from "viem/chains";
import {readActiveChain, withChain} from "@/lib/chain";
import {shortHash} from "@/lib/format";
import {useWallet} from "@/lib/wallet";
import type {SniperParams} from "@/lib/types";
import SNIPER_VAULT_ABI from "@/lib/SniperVault.abi.json";
import vaultCreationHex from "@/lib/SniperVault.creation.hex";

const WETH_BY_CHAIN: Record<string, Address> = {
  ethereum: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
  base: "0x4200000000000000000000000000000000000006",
};
const IDS: Record<string, number> = {ethereum: 1, base: 8453};
const WETH_ABI = [
  {type: "function", name: "deposit", stateMutability: "payable", inputs: [], outputs: []},
  {type: "function", name: "transfer", stateMutability: "nonpayable", inputs: [{name: "to", type: "address"}, {name: "value", type: "uint256"}], outputs: [{type: "bool"}]},
] as const;

function safeEth(raw: string): bigint | null {
  try {
    if (!raw.trim() || Number(raw) < 0 || !Number.isFinite(Number(raw))) return null;
    return parseEther(raw.trim());
  } catch {
    return null;
  }
}
function errorText(error: unknown): string { return (error instanceof Error ? error.message : String(error)).split("\n")[0]; }
function storageRead(key: string): string { try { return window.localStorage.getItem(key) || ""; } catch { return ""; } }
function storageWrite(key: string, value: string): void { try { window.localStorage.setItem(key, value); } catch { /* session still works */ } }

export default function SniperVaultWizard({params, onBound}: {params: SniperParams; onBound?: () => void}) {
  const wallet = useWallet();
  const slug = (readActiveChain() || "ethereum").toLowerCase();
  const expectedChainId = IDS[slug] || 1;
  const weth = WETH_BY_CHAIN[slug] || WETH_BY_CHAIN.ethereum;
  const client = useMemo(() => createPublicClient({chain: slug === "base" ? base : mainnet, transport: http(withChain("/api/eth", slug))}), [slug]);
  const storageKey = `jerseymikes.sniper-vault.${slug}`;

  const [open, setOpen] = useState(!params.vaultAddress);
  const [vault, setVault] = useState("");
  const [searcher, setSearcher] = useState("");
  const [daily, setDaily] = useState(() => {
    try { return Number(formatEther(BigInt(params.dailyBudgetWei || "0"))).toString(); } catch { return "0.25"; }
  });
  const [total, setTotal] = useState(() => {
    try { return Number(formatEther(BigInt(params.totalBudgetWei || "0"))).toString(); } catch { return "0"; }
  });
  const [fundAmount, setFundAmount] = useState("0.10");
  const [busy, setBusy] = useState("");
  const [message, setMessage] = useState("");
  const [verified, setVerified] = useState<{code: boolean; owner: string; allowed: boolean | null; weth: boolean}>({code: false, owner: "", allowed: null, weth: false});

  useEffect(() => {
    const saved = storageRead(storageKey);
    if (isAddress(saved)) setVault(saved);
    else if (params.vaultAddress && isAddress(params.vaultAddress)) setVault(params.vaultAddress);
    fetch(withChain("/api/bot/config", slug), {cache: "no-store"})
      .then((response) => response.json())
      .then((config: {sniperSearcher?: string; searcher?: string}) => {
        const address = config.sniperSearcher || config.searcher || "";
        if (isAddress(address)) setSearcher(address);
      })
      .catch(() => {});
  }, [params.vaultAddress, slug, storageKey]);

  const verify = useCallback(async () => {
    if (!isAddress(vault)) return;
    try {
      const code = await client.getCode({address: vault as Address});
      const [owner, actualWeth] = await Promise.all([
        client.readContract({address: vault as Address, abi: SNIPER_VAULT_ABI, functionName: "owner"}),
        client.readContract({address: vault as Address, abi: SNIPER_VAULT_ABI, functionName: "WETH"}),
      ]);
      const allowed = isAddress(searcher)
        ? Boolean(await client.readContract({address: vault as Address, abi: SNIPER_VAULT_ABI, functionName: "searchers", args: [searcher as Address]}).catch(() => false))
        : null;
      setVerified({code: Boolean(code && code !== "0x"), owner: String(owner), allowed, weth: String(actualWeth).toLowerCase() === weth.toLowerCase()});
    } catch (error) {
      setVerified({code: false, owner: "", allowed: null, weth: false});
      setMessage(`Vault verification failed: ${errorText(error)}`);
    }
  }, [client, searcher, vault, weth]);

  useEffect(() => { void verify(); }, [verify]);

  const deploy = async () => {
    if (!wallet.address || !wallet.eip1193 || wallet.chainId !== expectedChainId) { setMessage("Connect the operator wallet to the selected console chain first."); return; }
    const dailyWei = safeEth(daily); const totalWei = safeEth(total);
    if (dailyWei === null || totalWei === null) { setMessage("Enter valid non-negative ETH budgets."); return; }
    setBusy("deploy"); setMessage("Confirm SniperVault deployment in your wallet…");
    try {
      const args = encodeAbiParameters([{type: "address"}, {type: "uint256"}, {type: "uint256"}], [weth, dailyWei, totalWei]);
      const data = `${vaultCreationHex.trim()}${args.slice(2)}` as `0x${string}`;
      const walletClient = createWalletClient({transport: custom(wallet.eip1193)});
      const hash = await walletClient.sendTransaction({account: wallet.address as Address, data, chain: null});
      const receipt = await client.waitForTransactionReceipt({hash});
      if (!receipt.contractAddress) throw new Error("deployment receipt has no contract address");
      setVault(receipt.contractAddress); storageWrite(storageKey, receipt.contractAddress);
      setMessage(`SniperVault deployed at ${receipt.contractAddress}. Verify and allowlist the searcher.`);
      await verify();
    } catch (error) { setMessage(`Deployment failed: ${errorText(error)}`); }
    finally { setBusy(""); }
  };

  const allowlist = async () => {
    if (!wallet.address || !wallet.eip1193 || !isAddress(vault) || !isAddress(searcher)) { setMessage("A deployed vault, owner wallet and valid searcher address are required."); return; }
    setBusy("allowlist");
    try {
      const walletClient = createWalletClient({transport: custom(wallet.eip1193)});
      const hash = await walletClient.writeContract({account: wallet.address as Address, address: vault as Address, abi: SNIPER_VAULT_ABI, functionName: "setSearcher", args: [searcher as Address, true], chain: null});
      await client.waitForTransactionReceipt({hash}); setMessage(`Searcher allowlisted: ${shortHash(hash, 6)}.`); await verify();
    } catch (error) { setMessage(`Allowlist failed: ${errorText(error)}`); }
    finally { setBusy(""); }
  };

  const fundAndBind = async () => {
    const amount = safeEth(fundAmount);
    if (!amount || !wallet.address || !wallet.eip1193 || !isAddress(vault)) { setMessage("Choose a deployed vault, connected owner wallet and valid fund amount."); return; }
    setBusy("fund");
    try {
      const walletClient = createWalletClient({transport: custom(wallet.eip1193)});
      const wrapHash = await walletClient.writeContract({account: wallet.address as Address, address: weth, abi: WETH_ABI, functionName: "deposit", value: amount, chain: null});
      await client.waitForTransactionReceipt({hash: wrapHash});
      const transferHash = await walletClient.writeContract({account: wallet.address as Address, address: weth, abi: WETH_ABI, functionName: "transfer", args: [vault as Address, amount], chain: null});
      await client.waitForTransactionReceipt({hash: transferHash});
      const bind = await fetch(withChain("/api/bot/sniper/params", slug), {method: "POST", headers: {"content-type": "application/json"}, body: JSON.stringify({vaultAddress: vault})});
      const body = (await bind.json()) as {ok?: boolean; error?: string; demo?: boolean};
      if (!bind.ok || !body.ok || body.demo) throw new Error(body.error || "bot rejected vault binding");
      storageWrite(storageKey, vault); setMessage(`Funded ${shortHash(vault, 6)} and bound SNIPER_VAULT_ADDRESS in the running bot.`); onBound?.();
    } catch (error) { setMessage(`Funding/binding failed: ${errorText(error)}`); }
    finally { setBusy(""); }
  };

  const step = !wallet.address || wallet.chainId !== expectedChainId ? 1 : !verified.code ? 2 : verified.allowed !== true ? 3 : 4;
  const configured = verified.code && verified.weth && verified.allowed === true;

  return (
    <section className="panel" style={{padding: 12, borderColor: configured ? "rgba(34,197,94,0.45)" : "rgba(34,211,238,0.45)"}}>
      <button onClick={() => setOpen((value) => !value)} style={headerButton} aria-expanded={open}>
        <span>🛡️ SniperVault setup wizard</span><span className="muted">{configured ? "configured ✓" : `step ${step} of 4`} {open ? "▴" : "▾"}</span>
      </button>
      {open && <div style={{display: "grid", gap: 9, marginTop: 10}}>
        <div style={stepStyle(step >= 1, 1)}><strong>1. Connect wallet</strong><span className="muted">{wallet.address ? `${shortHash(wallet.address, 6)} · chain ${wallet.chainId}` : "operator owner wallet required"}</span>{!wallet.address && <button style={buttonStyle} onClick={() => void wallet.connect()}>connect wallet</button>}{wallet.address && wallet.chainId !== expectedChainId && <button style={buttonStyle} onClick={() => void wallet.switchChain(expectedChainId)}>switch network</button>}</div>
        <div style={stepStyle(step >= 2, 2)}><strong>2. Deploy SniperVault</strong><input value={vault} onChange={(event) => setVault(event.target.value)} placeholder="paste deployed vault or deploy" style={inputStyle} /><button style={buttonStyle} disabled={busy !== "" || !wallet.address || wallet.chainId !== expectedChainId} onClick={() => void deploy()}>{busy === "deploy" ? "deploying…" : "deploy vault"}</button><span className="muted">{verified.code ? `${verified.weth ? "WETH binding ✓" : "wrong WETH"}` : "bytecode not verified"}</span></div>
        <div style={stepStyle(step >= 3, 3)}><strong>3. Allowlist searcher</strong><input value={searcher} onChange={(event) => setSearcher(event.target.value)} placeholder="SNIPER_SEARCHER_ADDRESS" style={inputStyle} /><button style={buttonStyle} disabled={busy !== "" || !verified.code || !isAddress(searcher)} onClick={() => void allowlist()}>{busy === "allowlist" ? "confirming…" : "setSearcher(searcher, true)"}</button><span className={verified.allowed ? "good" : "warn"}>{verified.allowed === null ? "not checked" : verified.allowed ? "allowlisted ✓" : "not allowlisted"}</span></div>
        <div style={stepStyle(step >= 4, 4)}><strong>4. Fund & bind</strong><input value={fundAmount} onChange={(event) => setFundAmount(event.target.value)} style={smallInput} /><span className="muted">ETH → WETH</span><button style={buttonStyle} disabled={busy !== "" || !configured} onClick={() => void fundAndBind()}>{busy === "fund" ? "funding…" : "wrap, fund + bind"}</button><span className="muted">runtime patch is not a substitute for persisting the env line</span></div>
        {message && <div style={{fontSize: 11, color: message.toLowerCase().includes("failed") ? "var(--red)" : "var(--cyan)"}} role="status">{message}</div>}
        <div className="muted" style={{fontSize: 10}}>Constructor: (WETH {shortHash(weth, 6)}, daily {daily} ETH, lifetime {total || "0"} ETH). Change budgets in the Parameters tab before arming; this wizard never handles private keys.</div>
      </div>}
    </section>
  );
}

const headerButton: CSSProperties = {width: "100%", display: "flex", justifyContent: "space-between", background: "transparent", border: 0, color: "var(--text)", cursor: "pointer", fontFamily: "inherit", fontSize: 12, fontWeight: 700, padding: 0, textAlign: "left"};
const buttonStyle: CSSProperties = {background: "#111a25", border: "1px solid #24334a", borderRadius: 4, color: "#d7e2f0", padding: "4px 9px", cursor: "pointer", fontFamily: "inherit", fontSize: 11};
const inputStyle: CSSProperties = {...buttonStyle, background: "#070b11", minWidth: 210, flex: "1 1 210px"};
const smallInput: CSSProperties = {...inputStyle, minWidth: 70, width: 80, flex: "0 0 auto"};
const stepStyle = (done: boolean, number: number): CSSProperties => ({display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap", padding: "7px 8px", border: "1px solid var(--line)", borderRadius: 4, background: done ? "rgba(34,197,94,0.04)" : "transparent"});
