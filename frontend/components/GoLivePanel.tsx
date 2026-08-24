"use client";

/**
 * Production go-live wizard.
 *
 * This panel intentionally separates deployment, funding, configuration and
 * arming. Connecting a wallet or deploying a contract must never implicitly
 * enable a trading lane. The browser can write only through the connected
 * operator wallet; private bot keys remain server-side and are never accepted
 * by this component.
 */

import {useCallback, useEffect, useMemo, useState, type CSSProperties, type ReactNode} from "react";
import {
  createPublicClient,
  createWalletClient,
  custom,
  encodeAbiParameters,
  formatEther,
  http,
  isAddress,
  parseEther,
  type Address,
} from "viem";
import {base, mainnet} from "viem/chains";
import {onChainChange, readActiveChain, withChain} from "@/lib/chain";
import {explorerName} from "@/lib/explorer";
import {useWallet} from "@/lib/wallet";
import EXECUTOR_ABI from "@/lib/MevExecutor.abi.json";
import executorCreationHex from "@/lib/MevExecutor.creation.hex";
import SNIPER_VAULT_ABI from "@/lib/SniperVault.abi.json";
import vaultCreationHex from "@/lib/SniperVault.creation.hex";
import type {ConfigResponse, SniperParamsResponse, SniperVaultStatus, StatusResponse} from "@/lib/types";

const WETH_ABI = [
  {type: "function", name: "deposit", stateMutability: "payable", inputs: [], outputs: []},
  {type: "function", name: "transfer", stateMutability: "nonpayable", inputs: [{name: "to", type: "address"}, {name: "value", type: "uint256"}], outputs: [{type: "bool"}]},
  {type: "function", name: "balanceOf", stateMutability: "view", inputs: [{name: "account", type: "address"}], outputs: [{type: "uint256"}]},
] as const;

const CHAIN_IDS: Record<string, number> = {ethereum: 1, mainnet: 1, base: 8453};
const CHAIN_LABELS: Record<string, string> = {ethereum: "Ethereum mainnet", mainnet: "Ethereum mainnet", base: "Base"};
const WETH_BY_CHAIN: Record<string, Address> = {
  ethereum: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
  mainnet: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
  base: "0x4200000000000000000000000000000000000006",
};
const BALANCER_VAULT = "0xBA12222222228d8Ba445958a75a0704d566BF2C8" as Address;
const GAS_THRESHOLD_WEI = parseEther("0.005");
const STORAGE_EXECUTOR = "jerseymikes.executor";
const STORAGE_VAULT = "jerseymikes.sniper-vault";

function chainKey(slug: string | null): string {
  return (slug || "ethereum").toLowerCase();
}

function keyFor(baseKey: string, slug: string): string {
  return `${baseKey}.${slug}`;
}

function storageGet(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

function storageSet(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // The deployment address is still visible in the current session.
  }
}

function asHex(raw: string): `0x${string}` {
  const clean = raw.trim().replace(/^0x/, "");
  return `0x${clean}` as `0x${string}`;
}

function deployData(kind: "executor" | "vault", slug: string, daily: string, total: string): `0x${string}` {
  const creation = kind === "executor" ? asHex(executorCreationHex) : asHex(vaultCreationHex);
  const weth = WETH_BY_CHAIN[slug] ?? WETH_BY_CHAIN.ethereum;
  const args = kind === "executor"
    ? encodeAbiParameters([{type: "address"}, {type: "address"}], [BALANCER_VAULT, weth])
    : encodeAbiParameters(
        [{type: "address"}, {type: "uint256"}, {type: "uint256"}],
        [weth, parseEther(daily || "0"), parseEther(total || "0")],
      );
  return `${creation}${args.slice(2)}` as `0x${string}`;
}

function safeWei(eth: string): bigint | null {
  try {
    if (!eth.trim() || Number(eth) < 0 || !Number.isFinite(Number(eth))) return null;
    return parseEther(eth.trim());
  } catch {
    return null;
  }
}

function errText(error: unknown): string {
  return (error instanceof Error ? error.message : String(error)).split("\n")[0];
}

function shorten(value: string | null | undefined): string {
  return value && value.length > 14 ? `${value.slice(0, 10)}…${value.slice(-4)}` : value || "—";
}

interface BotConfig extends ConfigResponse {
  sniperSearcher?: string;
  sniperSearcherKeyConfigured?: boolean;
}

interface PreflightResponse {
  rpc: boolean;
  relay: boolean;
  qualification: unknown;
  demo?: boolean;
}

interface ContractCheck {
  codeBytes: number | null;
  owner: string | null;
  weth: string | null;
  searcherAllowed: boolean | null;
  error: string | null;
}

const emptyCheck: ContractCheck = {codeBytes: null, owner: null, weth: null, searcherAllowed: null, error: null};

export default function GoLivePanel({executor: runtimeExecutor, armed: runtimeArmed, chainId: botChainId}: {
  executor?: string;
  armed?: boolean;
  chainId?: number;
}) {
  const wallet = useWallet();
  const [slug, setSlug] = useState(() => chainKey(readActiveChain()));
  const expectedChainId = botChainId ?? CHAIN_IDS[slug] ?? 1;
  const label = CHAIN_LABELS[slug] ?? slug;
  const weth = WETH_BY_CHAIN[slug] ?? WETH_BY_CHAIN.ethereum;
  const publicClient = useMemo(
    () => createPublicClient({chain: slug === "base" ? base : mainnet, transport: http(withChain("/api/eth", slug))}),
    [slug],
  );

  const [status, setStatus] = useState<StatusResponse | null>(null);
  const [botConfig, setBotConfig] = useState<BotConfig | null>(null);
  const [sniperParams, setSniperParams] = useState<SniperParamsResponse | null>(null);
  const [vaultStatus, setVaultStatus] = useState<SniperVaultStatus | null>(null);
  const [executorAddress, setExecutorAddress] = useState("");
  const [vaultAddress, setVaultAddress] = useState("");
  const [executorCheck, setExecutorCheck] = useState<ContractCheck>(emptyCheck);
  const [vaultCheck, setVaultCheck] = useState<ContractCheck>(emptyCheck);
  const [searcherInput, setSearcherInput] = useState("");
  const [sniperSearcherInput, setSniperSearcherInput] = useState("");
  const [dailyBudget, setDailyBudget] = useState("0.25");
  const [totalBudget, setTotalBudget] = useState("0");
  const [fundAmount, setFundAmount] = useState("0.10");
  const [fundTarget, setFundTarget] = useState<"vault" | "executor">("vault");
  const [soakHours, setSoakHours] = useState("168");
  const [deploying, setDeploying] = useState<"executor" | "vault" | null>(null);
  const [preflight, setPreflight] = useState({rpc: false, bot: false, relay: false, qualification: false});
  const [message, setMessage] = useState<{tone: "info" | "good" | "bad" | "warn"; text: string} | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => onChainChange((next) => setSlug(chainKey(next))), []);

  const load = useCallback(async () => {
    const get = async <T,>(path: string): Promise<T | null> => {
      try {
        const response = await fetch(withChain(`/api/bot/${path}`, slug), {cache: "no-store"});
        if (!response.ok) return null;
        return (await response.json()) as T;
      } catch {
        return null;
      }
    };
    const [s, c, p, v, pf] = await Promise.all([
      get<StatusResponse>("status"),
      get<BotConfig>("config"),
      get<SniperParamsResponse>("sniper/params"),
      get<SniperVaultStatus>("sniper/vault"),
      get<PreflightResponse>("preflight"),
    ]);
    if (s) {
      setStatus(s);
      if (s.qualification) setSoakHours(String(s.qualification.requiredHours));
    }
    if (pf) {
      setPreflight({rpc: Boolean(pf.rpc), bot: !pf.demo, relay: Boolean(pf.relay), qualification: Boolean(pf.qualification)});
    } else if (s) {
      setPreflight((old) => ({...old, bot: !s.demo, qualification: Boolean(s.qualification)}));
    }
    if (c) {
      setBotConfig(c);
      if (c.searcher) {
        setSearcherInput(c.searcher);
      }
      if (c.sniperSearcher) setSniperSearcherInput(c.sniperSearcher);
    }
    if (p) {
      setSniperParams(p);
      if (p.params.vaultAddress && isAddress(p.params.vaultAddress)) setVaultAddress((old) => old || p.params.vaultAddress || "");
    }
    if (v) setVaultStatus(v);
  }, [slug]);

  useEffect(() => {
    void load();
    const timer = setInterval(() => void load(), 10_000);
    return () => clearInterval(timer);
  }, [load]);

  useEffect(() => {
    const savedExecutor = storageGet(keyFor(STORAGE_EXECUTOR, slug));
    const savedVault = storageGet(keyFor(STORAGE_VAULT, slug));
    setExecutorAddress(savedExecutor && isAddress(savedExecutor) ? savedExecutor : "");
    setVaultAddress(savedVault && isAddress(savedVault) ? savedVault : "");
    setExecutorCheck(emptyCheck);
    setVaultCheck(emptyCheck);
  }, [slug]);

  const executorTarget = executorAddress.trim();
  const vaultTarget = vaultAddress.trim();
  const gasReady = Boolean(wallet.address && wallet.chainId === expectedChainId && BigInt(wallet.balanceWei ?? 0n) >= GAS_THRESHOLD_WEI);
  const walletReady = Boolean(wallet.address && wallet.chainId === expectedChainId);
  const configuredSearcher = searcherInput.trim();
  const configuredSniperSearcher = sniperSearcherInput.trim();
  const searcherSeparated = Boolean(
    wallet.address && configuredSearcher && isAddress(configuredSearcher) && configuredSearcher.toLowerCase() !== wallet.address.toLowerCase(),
  );
  const sniperSeparated = Boolean(
    configuredSearcher && configuredSniperSearcher && isAddress(configuredSearcher) && isAddress(configuredSniperSearcher)
      && configuredSearcher.toLowerCase() !== configuredSniperSearcher.toLowerCase(),
  );
  const executorReady = executorCheck.codeBytes !== null && executorCheck.codeBytes > 0 && !executorCheck.error;
  const vaultReady = vaultCheck.codeBytes !== null && vaultCheck.codeBytes > 0 && !vaultCheck.error;
  const preflightReady = preflight.rpc && preflight.bot && preflight.relay && preflight.qualification;

  const verifyContract = useCallback(async (kind: "executor" | "vault", target: string) => {
    if (!isAddress(target)) return;
    const searcher = kind === "executor" ? configuredSearcher : configuredSniperSearcher;
    try {
      const code = await publicClient.getCode({address: target as Address});
      if (!code || code === "0x") throw new Error("no contract bytecode at this address");
      const owner = await publicClient.readContract({address: target as Address, abi: kind === "executor" ? EXECUTOR_ABI : SNIPER_VAULT_ABI, functionName: "owner"});
      const wethOnChain = await publicClient.readContract({address: target as Address, abi: SNIPER_VAULT_ABI, functionName: "WETH"}).catch(() => null);
      const allowed = searcher && isAddress(searcher)
        ? await publicClient.readContract({address: target as Address, abi: kind === "executor" ? EXECUTOR_ABI : SNIPER_VAULT_ABI, functionName: "searchers", args: [searcher as Address]}).catch(() => null)
        : null;
      if (wethOnChain && String(wethOnChain).toLowerCase() !== weth.toLowerCase()) {
        throw new Error(`WETH mismatch: contract uses ${String(wethOnChain)}, expected ${weth}`);
      }
      const result: ContractCheck = {
        codeBytes: Math.max(0, Math.floor((code.length - 2) / 2)),
        owner: String(owner),
        weth: wethOnChain ? String(wethOnChain) : null,
        searcherAllowed: allowed === null ? null : Boolean(allowed),
        error: null,
      };
      if (kind === "executor") setExecutorCheck(result);
      else setVaultCheck(result);
    } catch (error) {
      const result = {...emptyCheck, error: errText(error)};
      if (kind === "executor") setExecutorCheck(result);
      else setVaultCheck(result);
    }
  }, [configuredSearcher, configuredSniperSearcher, publicClient, weth]);

  useEffect(() => {
    if (isAddress(executorTarget)) void verifyContract("executor", executorTarget);
    if (isAddress(vaultTarget)) void verifyContract("vault", vaultTarget);
  }, [executorTarget, vaultTarget, verifyContract]);

  const updatePreflight = useCallback(async () => {
    try {
      const chain = await publicClient.getChainId();
      await publicClient.getBlockNumber();
      setPreflight((old) => ({...old, rpc: chain === expectedChainId}));
    } catch {
      setPreflight((old) => ({...old, rpc: false}));
    }
    try {
      const response = await fetch(withChain("/api/bot/preflight", slug), {cache: "no-store"});
      const check = (await response.json()) as PreflightResponse & {demo?: boolean};
      if (response.ok && !check.demo) {
        setPreflight((old) => ({...old, bot: true, relay: Boolean(check.relay), qualification: Boolean(check.qualification)}));
      }
    } catch {
      setPreflight((old) => ({...old, bot: false}));
    }
  }, [expectedChainId, publicClient, slug]);

  useEffect(() => {
    void updatePreflight();
  }, [updatePreflight]);

  const txWrite = useCallback(async (kind: "executor" | "vault") => {
    if (!wallet.address || !wallet.eip1193 || wallet.chainId !== expectedChainId) {
      setMessage({tone: "bad", text: `Connect the operator wallet to ${label} first.`});
      return;
    }
    setDeploying(kind);
    setMessage({tone: "info", text: `Confirm ${kind} deployment in your wallet…`});
    try {
      const daily = safeWei(dailyBudget);
      const total = safeWei(totalBudget);
      if (kind === "vault" && (daily === null || total === null)) throw new Error("enter valid non-negative ETH budgets");
      const data = deployData(kind, slug, dailyBudget, totalBudget);
      const client = createWalletClient({transport: custom(wallet.eip1193)});
      const hash = await client.sendTransaction({account: wallet.address as Address, data, chain: null});
      setMessage({tone: "info", text: `${kind} deployment ${shorten(hash)} is mining…`});
      const receipt = await publicClient.waitForTransactionReceipt({hash});
      const address = receipt.contractAddress;
      if (!address) throw new Error("receipt did not contain a contract address");
      if (kind === "executor") {
        setExecutorAddress(address);
        storageSet(keyFor(STORAGE_EXECUTOR, slug), address);
      } else {
        setVaultAddress(address);
        storageSet(keyFor(STORAGE_VAULT, slug), address);
        setMessage({tone: "good", text: `SniperVault deployed at ${address}. Verify it before funding.`});
      }
      setMessage({tone: "good", text: `${kind} deployed at ${address}.`});
    } catch (error) {
      setMessage({tone: "bad", text: `${kind} deployment failed: ${errText(error)}`});
    } finally {
      setDeploying(null);
    }
  }, [dailyBudget, expectedChainId, label, publicClient, slug, totalBudget, wallet.address, wallet.chainId, wallet.eip1193]);

  const sendNative = useCallback(async () => {
    const amount = safeWei(fundAmount);
    const target = fundTarget === "vault" ? vaultTarget : executorTarget;
    if (!amount || !isAddress(target) || !wallet.address || !wallet.eip1193) {
      setMessage({tone: "bad", text: "Choose a deployed target and a valid ETH amount."});
      return;
    }
    setBusy(true);
    try {
      const client = createWalletClient({transport: custom(wallet.eip1193)});
      const hash = await client.sendTransaction({account: wallet.address as Address, to: target as Address, value: amount, chain: null});
      await publicClient.waitForTransactionReceipt({hash});
      setMessage({tone: "good", text: `Native ETH funding confirmed: ${shorten(hash)}.`});
    } catch (error) {
      setMessage({tone: "bad", text: `Funding failed: ${errText(error)}`});
    } finally {
      setBusy(false);
    }
  }, [executorTarget, fundAmount, fundTarget, publicClient, vaultTarget, wallet.address, wallet.eip1193]);

  const wrapAndTransfer = useCallback(async () => {
    const amount = safeWei(fundAmount);
    const target = fundTarget === "vault" ? vaultTarget : executorTarget;
    if (!amount || !isAddress(target) || !wallet.address || !wallet.eip1193) {
      setMessage({tone: "bad", text: "Choose a deployed target and a valid ETH amount."});
      return;
    }
    setBusy(true);
    try {
      const client = createWalletClient({transport: custom(wallet.eip1193)});
      const wrapHash = await client.writeContract({account: wallet.address as Address, address: weth, abi: WETH_ABI, functionName: "deposit", value: amount, chain: null});
      await publicClient.waitForTransactionReceipt({hash: wrapHash});
      const transferHash = await client.writeContract({account: wallet.address as Address, address: weth, abi: WETH_ABI, functionName: "transfer", args: [target as Address, amount], chain: null});
      await publicClient.waitForTransactionReceipt({hash: transferHash});
      setMessage({tone: "good", text: `Wrapped and transferred ${fundAmount} WETH to ${shorten(target)}.`});
      void load();
    } catch (error) {
      setMessage({tone: "bad", text: `WETH funding failed: ${errText(error)}`});
    } finally {
      setBusy(false);
    }
  }, [executorTarget, fundAmount, fundTarget, load, publicClient, vaultTarget, wallet.address, wallet.eip1193, weth]);

  const allowVaultSearcher = useCallback(async () => {
    if (!isAddress(vaultTarget) || !isAddress(configuredSniperSearcher) || !wallet.address || !wallet.eip1193) {
      setMessage({tone: "bad", text: "A verified vault, valid dedicated sniper address and owner wallet are required."});
      return;
    }
    setBusy(true);
    try {
      const client = createWalletClient({transport: custom(wallet.eip1193)});
      const hash = await client.writeContract({account: wallet.address as Address, address: vaultTarget as Address, abi: SNIPER_VAULT_ABI, functionName: "setSearcher", args: [configuredSniperSearcher as Address, true], chain: null});
      await publicClient.waitForTransactionReceipt({hash});
      setMessage({tone: "good", text: `Dedicated sniper searcher allowlisted: ${shorten(hash)}.`});
      void verifyContract("vault", vaultTarget);
    } catch (error) {
      setMessage({tone: "bad", text: `Sniper searcher allowlist failed: ${errText(error)}`});
    } finally {
      setBusy(false);
    }
  }, [configuredSniperSearcher, publicClient, vaultTarget, verifyContract, wallet.address, wallet.eip1193]);

  const setVaultBudget = useCallback(async () => {
    const daily = safeWei(dailyBudget);
    const total = safeWei(totalBudget);
    if (daily === null || total === null || !isAddress(vaultTarget) || !wallet.address || !wallet.eip1193) {
      setMessage({tone: "bad", text: "A verified SniperVault, owner wallet and valid budgets are required."});
      return;
    }
    setBusy(true);
    try {
      const client = createWalletClient({transport: custom(wallet.eip1193)});
      const hash = await client.writeContract({account: wallet.address as Address, address: vaultTarget as Address, abi: SNIPER_VAULT_ABI, functionName: "setBudget", args: [daily, total], chain: null});
      await publicClient.waitForTransactionReceipt({hash});
      setMessage({tone: "good", text: `SniperVault budget updated: ${shorten(hash)}.`});
      void load();
    } catch (error) {
      setMessage({tone: "bad", text: `Budget update failed: ${errText(error)}`});
    } finally {
      setBusy(false);
    }
  }, [dailyBudget, load, publicClient, totalBudget, vaultTarget, wallet.address, wallet.eip1193]);

  const applyVaultRuntime = useCallback(async () => {
    if (!isAddress(vaultTarget)) return;
    setBusy(true);
    try {
      const response = await fetch(withChain("/api/bot/sniper/params", slug), {
        method: "POST",
        headers: {"content-type": "application/json"},
        body: JSON.stringify({vaultAddress: vaultTarget}),
      });
      const data = (await response.json()) as {ok?: boolean; error?: string; errors?: string[]; demo?: boolean};
      if (!response.ok || !data.ok || data.demo) throw new Error(data.errors?.join("; ") || data.error || "bot rejected the runtime vault binding");
      setMessage({tone: "good", text: "SniperVault address applied to the running bot. Persist it in the bot .env before restart."});
      void load();
    } catch (error) {
      setMessage({tone: "bad", text: `Runtime vault binding failed: ${errText(error)}`});
    } finally {
      setBusy(false);
    }
  }, [load, slug, vaultTarget]);

  const setSoak = useCallback(async () => {
    const hours = Number(soakHours);
    if (!Number.isInteger(hours) || hours < 1 || hours > 8760) {
      setMessage({tone: "bad", text: "Soak threshold must be an integer from 1 to 8760 hours."});
      return;
    }
    setBusy(true);
    try {
      const response = await fetch(withChain("/api/bot/qualification", slug), {
        method: "POST",
        headers: {"content-type": "application/json"},
        body: JSON.stringify({requiredHours: hours}),
      });
      const data = (await response.json()) as {ok?: boolean; error?: string; demo?: boolean};
      if (!response.ok || !data.ok || data.demo) throw new Error(data.error || "bot rejected the soak threshold");
      setMessage({tone: "good", text: `Dynamic soak threshold set to ${hours} hour${hours === 1 ? "" : "s"}. Evidence gates remain active.`});
      void load();
    } catch (error) {
      setMessage({tone: "bad", text: `Soak update failed: ${errText(error)}`});
    } finally {
      setBusy(false);
    }
  }, [load, slug, soakHours]);

  const armAtomic = useCallback(async () => {
    if (!preflightReady || !status?.liveArmed) {
      setMessage({tone: "bad", text: "Atomic live mode is not boot-armed or preflight is incomplete. Set LIVE_EXECUTION=true and I_UNDERSTAND_LIVE_RISK=yes, then restart."});
      return;
    }
    if (!window.confirm("Enable the atomic engine runtime LIVE mode? This permits real bundle submission when every risk and qualification gate passes.")) return;
    setBusy(true);
    try {
      const response = await fetch(withChain("/api/bot/mode", slug), {method: "POST", headers: {"content-type": "application/json"}, body: JSON.stringify({live: true})});
      const data = (await response.json()) as {ok?: boolean; error?: string; demo?: boolean};
      if (!response.ok || !data.ok || data.demo) throw new Error(data.error || "bot refused live mode");
      setMessage({tone: "warn", text: "Atomic runtime mode is LIVE. Submission is still gated per strategy, risk, nonce, inventory and qualification."});
      void load();
    } catch (error) {
      setMessage({tone: "bad", text: `Atomic live switch failed: ${errText(error)}`});
    } finally {
      setBusy(false);
    }
  }, [load, preflightReady, slug, status?.liveArmed]);

  const armSniper = useCallback(async () => {
    if (!preflightReady || !vaultReady || !isAddress(vaultTarget) || !botConfig?.sniperSearcherKeyConfigured) {
      setMessage({tone: "bad", text: "Sniper preflight requires a verified vault, reachable bot and dedicated SNIPER_SEARCHER_PRIVATE_KEY."});
      return;
    }
    if (!window.confirm("Arm the directional SniperVault lane? It can lose the full buy amount; the atomic engine may remain in simulation.")) return;
    setBusy(true);
    try {
      const response = await fetch(withChain("/api/bot/sniper/params", slug), {
        method: "POST",
        headers: {"content-type": "application/json"},
        body: JSON.stringify({enabled: true, vaultAddress: vaultTarget}),
      });
      const data = (await response.json()) as {ok?: boolean; error?: string; errors?: string[]; demo?: boolean};
      if (!response.ok || !data.ok || data.demo) throw new Error(data.errors?.join("; ") || data.error || "bot refused sniper arming");
      setMessage({tone: "warn", text: "Directional sniper enabled. Confirm the parameter card shows non-zero size/budget before expecting entries."});
      void load();
    } catch (error) {
      setMessage({tone: "bad", text: `Sniper arm failed: ${errText(error)}`});
    } finally {
      setBusy(false);
    }
  }, [botConfig?.sniperSearcherKeyConfigured, load, preflightReady, slug, vaultReady, vaultTarget]);

  const copy = async (text: string, labelText: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setMessage({tone: "good", text: `${labelText} copied.`});
    } catch {
      setMessage({tone: "bad", text: "Clipboard access was denied; copy the text manually."});
    }
  };

  const dynamicBotStatus = status?.qualification;
  const cliRpc = expectedChainId === 8453 ? "$BASE_HTTP_URL" : "$ETH_HTTP_URL";
  const executorCli = `cd contracts\nforge script script/Deploy.s.sol --rpc-url ${cliRpc} --broadcast --verify`;
  const vaultCli = `cd contracts\nforge script script/DeploySniperVault.s.sol --rpc-url ${cliRpc} --broadcast`;
  const envSnippet = `EXECUTOR_ADDRESS=${executorTarget || "<executor>"}\nSEARCHER_ADDRESS=${configuredSearcher || "<atomic-searcher>"}\nSNIPER_VAULT_ADDRESS=${vaultTarget || "<sniper-vault>"}\nSNIPER_SEARCHER_ADDRESS=${configuredSniperSearcher || "<dedicated-searcher>"}`;
  const doneCount = [walletReady, gasReady, executorReady && vaultReady, Boolean(vaultStatus?.configured && vaultStatus.address), preflightReady].filter(Boolean).length;

  return (
    <div style={{display: "grid", gap: 10}}>
      <div style={{display: "flex", justifyContent: "space-between", gap: 10, alignItems: "center", flexWrap: "wrap"}}>
        <div>
          <strong>Production Go-Live Wizard · {label}</strong>
          <span className="muted" style={{fontSize: 11, marginLeft: 8}}>{doneCount}/5 cards ready</span>
        </div>
        <span className="badge" style={{color: "var(--amber)", borderColor: "var(--amber)"}}>deploy ≠ armed</span>
      </div>
      {message && <div style={{...noticeStyle, color: toneColor(message.tone), borderColor: toneColor(message.tone)}} role="status">{message.text}<button onClick={() => setMessage(null)} style={dismiss}>×</button></div>}

      <WizardCard number="1" title={`Network & wallet · ${label}`} state={walletReady ? "done" : "todo"}>
        <div style={rowStyle}>
          {wallet.address ? <code>{shorten(wallet.address)}</code> : <span className="muted">No operator wallet connected</span>}
          {wallet.address && <span className={wallet.chainId === expectedChainId ? "good" : "warn"}>{wallet.chainId === expectedChainId ? `chain ${expectedChainId} ✓` : `wallet chain ${wallet.chainId} · need ${expectedChainId}`}</span>}
          {!wallet.address && <button style={buttonStyle} onClick={() => void wallet.connect()}>connect wallet</button>}
          {wallet.address && wallet.chainId !== expectedChainId && <button style={buttonStyle} onClick={() => void wallet.switchChain(expectedChainId)}>switch to {label}</button>}
          {wallet.address && <button style={buttonStyle} onClick={() => void wallet.disconnect()}>disconnect</button>}
        </div>
        <div className="muted" style={{fontSize: 10}}>The connected wallet is the contract owner for browser deployments. It is not accepted as a bot signer unless you explicitly allowlist it.</div>
      </WizardCard>

      <WizardCard number="2" title="EOA & searcher-key verification" state={gasReady && Boolean(configuredSearcher) ? "done" : walletReady ? "todo" : "locked"}>
        <div style={{display: "grid", gap: 6}}>
          <div style={rowStyle}><span className="muted">owner/deployer</span><code>{shorten(wallet.address)}</code><span className={gasReady ? "good" : "warn"}>{formatEther(BigInt(wallet.balanceWei ?? 0n)).slice(0, 8)} ETH · {gasReady ? "gas buffer ok" : "≥ 0.005 ETH required"}</span></div>
          <div style={rowStyle}><span className="muted">atomic searcher</span><input value={searcherInput} onChange={(e) => setSearcherInput(e.target.value)} placeholder="SEARCHER_ADDRESS" style={inputStyle} /><span className={searcherSeparated ? "good" : "warn"}>{searcherSeparated ? "separate from owner ✓" : "verify separation"}</span></div>
          <div style={rowStyle}><span className="muted">sniper searcher</span><input value={sniperSearcherInput} onChange={(e) => setSniperSearcherInput(e.target.value)} placeholder="SNIPER_SEARCHER_ADDRESS" style={inputStyle} /><span className={sniperSeparated ? "good" : "warn"}>{botConfig?.sniperSearcherKeyConfigured ? "private key configured ✓" : "dedicated key not configured"}</span></div>
          <div className="muted" style={{fontSize: 10}}>Addresses are public. Private keys are never returned by the bot API. Live directional execution is refused at boot without SNIPER_SEARCHER_PRIVATE_KEY.</div>
        </div>
      </WizardCard>

      <WizardCard number="3" title="Deploy & verify MevExecutor + SniperVault" state={executorReady && vaultReady ? "done" : gasReady ? "todo" : "locked"}>
        <div style={{display: "grid", gap: 8}}>
          <div style={rowStyle}><strong>MevExecutor</strong><input value={executorAddress} onChange={(e) => setExecutorAddress(e.target.value)} placeholder="0x executor address" style={inputStyle} /><button style={buttonStyle} disabled={!gasReady || deploying !== null} onClick={() => void txWrite("executor")}>{deploying === "executor" ? "deploying…" : "deploy / verify"}</button><span className={executorReady ? "good" : "muted"}>{executorCheck.error || (executorCheck.codeBytes !== null ? `${executorCheck.codeBytes.toLocaleString()} bytes` : "not checked")}</span></div>
          <div style={rowStyle}><strong>SniperVault</strong><input value={vaultAddress} onChange={(e) => setVaultAddress(e.target.value)} placeholder="0x sniper vault address" style={inputStyle} /><button style={buttonStyle} disabled={!gasReady || deploying !== null} onClick={() => void txWrite("vault")}>{deploying === "vault" ? "deploying…" : "deploy / verify"}</button><button style={buttonStyle} disabled={!vaultReady || busy || !isAddress(configuredSniperSearcher)} onClick={() => void allowVaultSearcher()}>allow sniper searcher</button><span className={vaultReady ? "good" : "muted"}>{vaultCheck.error || (vaultCheck.codeBytes !== null ? `${vaultCheck.codeBytes.toLocaleString()} bytes` : "not checked")}</span></div>
          <div style={rowStyle}><button style={buttonStyle} onClick={() => void copy(executorCli, "MevExecutor forge command")}>copy executor CLI</button><button style={buttonStyle} onClick={() => void copy(vaultCli, "SniperVault forge command")}>copy vault CLI</button><button style={buttonStyle} onClick={() => void copy(envSnippet, "contract env lines")}>copy env lines</button></div>
          <pre style={preStyle}>{executorCli}\n\n{vaultCli}</pre>
          <div className="muted" style={{fontSize: 10}}>Expected constructor bindings: Balancer V2 {shorten(BALANCER_VAULT)} · WETH {shorten(weth)}. Verify owner, WETH and searcher allowlisting before funding.</div>
          <div style={rowStyle}><span className="muted">executor owner {shorten(executorCheck.owner)} · searcher</span><span className={executorCheck.searcherAllowed ? "good" : "warn"}>{executorCheck.searcherAllowed === null ? "not checked" : executorCheck.searcherAllowed ? "allowlisted ✓" : "not allowlisted"}</span><span className="muted">vault owner {shorten(vaultCheck.owner)} · searcher</span><span className={vaultCheck.searcherAllowed ? "good" : "warn"}>{vaultCheck.searcherAllowed === null ? "not checked" : vaultCheck.searcherAllowed ? "allowlisted ✓" : "not allowlisted"}</span></div>
        </div>
      </WizardCard>

      <WizardCard number="4" title="Funding, WETH deposit and budget ceilings" state={vaultStatus?.configured && vaultStatus.address ? "done" : vaultReady ? "todo" : "locked"}>
        <div style={{display: "grid", gap: 8}}>
          <div style={rowStyle}><span className="muted">daily budget ETH</span><input value={dailyBudget} onChange={(e) => setDailyBudget(e.target.value)} style={smallInput} /><span className="muted">lifetime (0 = unlimited)</span><input value={totalBudget} onChange={(e) => setTotalBudget(e.target.value)} style={smallInput} /><button style={buttonStyle} disabled={!vaultReady || busy} onClick={() => void setVaultBudget()}>set vault budget</button></div>
          <div style={rowStyle}><span className="muted">fund target</span><select value={fundTarget} onChange={(e) => setFundTarget(e.target.value as "vault" | "executor")} style={smallInput}><option value="vault">SniperVault</option><option value="executor">MevExecutor</option></select><input value={fundAmount} onChange={(e) => setFundAmount(e.target.value)} style={smallInput} /><span className="muted">ETH</span><button style={buttonStyle} disabled={busy || !walletReady} onClick={() => void wrapAndTransfer()}>wrap + transfer WETH</button><button style={buttonStyle} disabled={busy || !walletReady} onClick={() => void sendNative()}>send native ETH</button></div>
          <div style={rowStyle}><span className="muted">vault spendable</span><strong>{vaultStatus?.spendableRemainingWei ? `${formatEther(BigInt(vaultStatus.spendableRemainingWei))} WETH` : "—"}</strong><span className="muted">reset {vaultStatus?.windowResetTimeSecs ? new Date(vaultStatus.windowResetTimeSecs * 1000).toLocaleString() : "—"}</span><button style={buttonStyle} disabled={busy || !vaultReady} onClick={() => void applyVaultRuntime()}>apply vault to running bot</button></div>
          <div className="muted" style={{fontSize: 10}}>WETH is transferred to the contract, not the connected wallet. Daily/total ceilings are enforced on-chain; drawdown is configured separately in the Sniper Parameters panel and remains off-chain lane risk.</div>
        </div>
      </WizardCard>

      <WizardCard number="5" title="Pre-flight, dynamic soak and independent live switches" state={preflightReady ? "done" : "todo"}>
        <div style={{display: "grid", gap: 8}}>
          <div style={rowStyle}><Check label="RPC" ok={preflight.rpc} /><Check label="bot API" ok={preflight.bot} /><Check label={expectedChainId === 8453 ? "Base feed / raw path" : "relay data"} ok={preflight.relay} /><Check label="qualification loaded" ok={preflight.qualification} /><button style={buttonStyle} onClick={() => {void updatePreflight(); void load();}}>refresh preflight</button></div>
          <div style={rowStyle}><span className="muted">operator soak threshold</span><input type="number" min="1" max="8760" value={soakHours} onChange={(e) => setSoakHours(e.target.value)} style={smallInput} /><span className="muted">hours · current evidence {dynamicBotStatus?.elapsedHours ?? 0}h / {dynamicBotStatus?.requiredHours ?? soakHours}h</span><button style={buttonStyle} disabled={busy || !preflight.bot} onClick={() => void setSoak()}>apply threshold</button></div>
          <div style={rowStyle}><button style={{...buttonStyle, borderColor: "var(--amber)", color: "var(--amber)"}} disabled={busy || !preflightReady} onClick={() => void armAtomic()}>confirm & enable atomic runtime LIVE</button><button style={{...buttonStyle, borderColor: "var(--amber)", color: "var(--amber)"}} disabled={busy || !preflightReady} onClick={() => void armSniper()}>confirm & enable sniper lane</button><span className="muted">atomic: {status?.mode ?? "—"} · boot armed: {status?.liveArmed ? "yes" : "no"} · sniper: {sniperParams?.armed ? "armed" : "shadow"}</span></div>
          <div className="muted" style={{fontSize: 10}}>These buttons only change runtime state. Boot capability remains explicit: LIVE_EXECUTION=true plus I_UNDERSTAND_LIVE_RISK=yes for atomic, and SNIPER_DIRECTIONAL=true plus dedicated key/budget/vault for the sniper. A qualification PASS is still checked before atomic submission.</div>
        </div>
      </WizardCard>

      <div className="muted" style={{fontSize: 10}}>Runtime executor: <code>{shorten(runtimeExecutor)}</code> · runtime atomic mode: <code>{runtimeArmed ? "armed" : "simulation"}</code> · {explorerName(expectedChainId)} links are shown after verified addresses and receipts.</div>
    </div>
  );
}

function WizardCard({number, title, state, children}: {number: string; title: string; state: "done" | "todo" | "locked"; children: ReactNode}) {
  const color = state === "done" ? "var(--green)" : state === "locked" ? "var(--muted)" : "var(--cyan)";
  return <section style={{border: "1px solid var(--line)", borderRadius: 5, padding: "10px 12px", background: state === "locked" ? "transparent" : "var(--panel-2)", opacity: state === "locked" ? 0.58 : 1}}><div style={{display: "flex", gap: 8, alignItems: "center", marginBottom: 8}}><span style={{color, fontWeight: 800}}>{state === "done" ? "✓" : state === "locked" ? "🔒" : number}</span><strong style={{fontSize: 12}}>{title}</strong><span className="muted" style={{marginLeft: "auto", fontSize: 10}}>{state}</span></div>{children}</section>;
}

function Check({label, ok}: {label: string; ok: boolean}) { return <span className={ok ? "good" : "warn"} style={{fontSize: 11}}>{label}: {ok ? "ok" : "pending"}</span>; }

const toneColor = (tone: "info" | "good" | "bad" | "warn") => tone === "good" ? "var(--green)" : tone === "bad" ? "var(--red)" : tone === "warn" ? "var(--amber)" : "var(--cyan)";
const rowStyle: CSSProperties = {display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap"};
const buttonStyle: CSSProperties = {background: "#111a25", border: "1px solid #24334a", borderRadius: 4, color: "#d7e2f0", padding: "4px 9px", cursor: "pointer", fontFamily: "inherit", fontSize: 11};
const inputStyle: CSSProperties = {...buttonStyle, background: "#070b11", minWidth: 220, flex: "1 1 220px"};
const smallInput: CSSProperties = {...inputStyle, minWidth: 72, width: 100, flex: "0 0 auto"};
const preStyle: CSSProperties = {background: "#070b11", color: "var(--cyan)", border: "1px solid var(--line)", borderRadius: 4, padding: 8, margin: 0, overflowX: "auto", fontSize: 10};
const noticeStyle: CSSProperties = {padding: "7px 10px", border: "1px solid", borderRadius: 4, fontSize: 11};
const dismiss: CSSProperties = {float: "right", marginLeft: 12, background: "transparent", border: 0, color: "inherit", cursor: "pointer"};
