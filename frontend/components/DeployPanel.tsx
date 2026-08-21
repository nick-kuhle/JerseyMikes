"use client";

/**
 * Go-live checklist: deploy `MevExecutor` to mainnet from the browser.
 *
 * Written for a first-time operator. The steps enforce the order that
 * `docs/GO_LIVE.md` explains in full:
 *
 *   1. connect a wallet (on the bot's chain)
 *   2. confirm it holds gas money
 *   3. deploy the executor (constructor: balancerVault, weth)
 *   4. fund the executor a little (bundle gas at live time)
 *   5. allowlist the bot's searcher EOA
 *   6. point the bot at the new address (env + restart)
 *
 * Deploying is independent of — and comes *before* — arming the bot for live
 * execution. Even armed, this build never broadcasts (Phase 3); simulation
 * needs no deployment at all (the fork injects the bytecode).
 */

import {useCallback, useEffect, useState} from "react";
import {
  createPublicClient,
  createWalletClient,
  custom,
  encodeDeployData,
  formatEther,
  formatUnits,
  http,
  isAddress,
  parseEther,
  type Address,
} from "viem";
import {mainnet} from "viem/chains";
import EXECUTOR_ABI from "@/lib/MevExecutor.abi.json";
import {
  MAINNET_BALANCER_VAULT,
  MAINNET_WETH,
  MEV_EXECUTOR_CREATION_BYTECODE,
} from "@/lib/MevExecutor.creation";
import {useWallet} from "@/lib/wallet";
import {addressUrl, explorerName} from "@/lib/explorer";

const STORAGE_ADDR = "jm.executor.deployed";
const RECOMMENDED_GAS_ETH = 0.05;
const MIN_GAS_ETH = 0.02;

export default function DeployPanel({chainId}: {chainId?: number}) {
  const {address, chainId: walletChain, balanceWei, eip1193, connect, switchChain, refreshBalance} = useWallet();

  const [vault, setVault] = useState<string>(MAINNET_BALANCER_VAULT);
  const [weth, setWeth] = useState<string>(MAINNET_WETH);
  const [estimate, setEstimate] = useState<{gas: bigint; price: bigint} | null>(null);
  const [deployHash, setDeployHash] = useState<string | null>(null);
  const [deployed, setDeployed] = useState<string | null>(null);
  const [status, setStatus] = useState("");
  const [busy, setBusy] = useState(false);

  // The bot's searcher EOA (prefills step 5).
  const [searcher, setSearcher] = useState("");
  const [searcherSource, setSearcherSource] = useState<"bot" | "manual">("manual");
  const [fundAmount, setFundAmount] = useState("0.05");
  const [copied, setCopied] = useState<string | null>(null);

  const homeChain = chainId ?? 1;

  useEffect(() => {
    const saved = window.localStorage.getItem(STORAGE_ADDR);
    if (saved && isAddress(saved)) setDeployed(saved);
  }, []);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const r = await fetch("/api/bot/config", {cache: "no-store"});
        const j = (await r.json()) as {searcher?: string};
        if (!cancelled && j.searcher && isAddress(j.searcher)) {
          setSearcher(j.searcher);
          setSearcherSource("bot");
        }
      } catch {
        /* bot unreachable: the field stays manual */
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  const publicClient = createPublicClient({chain: mainnet, transport: http("/api/eth")});

  const rightChain = address !== null && walletChain === homeChain;
  const balanceEth = balanceWei !== null ? Number(formatEther(balanceWei)) : null;
  const gasOk = balanceEth !== null && balanceEth >= MIN_GAS_ETH;

  const estimateCost = useCallback(async () => {
    if (!address) return;
    setStatus("estimating…");
    try {
      const data = encodeDeployData({
        abi: EXECUTOR_ABI,
        bytecode: MEV_EXECUTOR_CREATION_BYTECODE as `0x${string}`,
        args: [vault as Address, weth as Address],
      });
      const rpc = async (method: string, params: unknown[]) => {
        const r = await fetch("/api/eth", {
          method: "POST",
          headers: {"content-type": "application/json"},
          body: JSON.stringify({jsonrpc: "2.0", id: 1, method, params}),
        });
        const j = (await r.json()) as {result?: string};
        return BigInt((j.result as string) ?? "0");
      };
      const [gas, price] = await Promise.all([
        rpc("eth_estimateGas", [{from: address, data}]),
        rpc("eth_gasPrice", []),
      ]);
      setEstimate({gas, price});
      setStatus(
        `~${Number(formatUnits(gas, 0)).toLocaleString()} gas · ~${formatUnits(gas * price, 18).slice(0, 6)} ETH at the current base fee`
      );
    } catch (e) {
      setEstimate(null);
      setStatus(`estimate failed: ${(e as Error).message.split("\n")[0]}`);
    }
  }, [address, vault, weth]);

  const deploy = useCallback(async () => {
    if (!address || !eip1193) return;
    setBusy(true);
    setStatus("waiting for you to confirm in the wallet…");
    setDeployHash(null);
    try {
      const wallet = createWalletClient({transport: custom(eip1193)});
      const hash = (await wallet.deployContract({
        account: address as Address,
        abi: EXECUTOR_ABI,
        bytecode: MEV_EXECUTOR_CREATION_BYTECODE as `0x${string}`,
        args: [vault as Address, weth as Address],
        chain: null,
      })) as string;
      setDeployHash(hash);
      setStatus(`deploy tx ${hash.slice(0, 10)}… sent — waiting for the receipt…`);
      const receipt = await publicClient.waitForTransactionReceipt({hash: hash as `0x${string}`});
      if (receipt.status === "success" && receipt.contractAddress) {
        setDeployed(receipt.contractAddress);
        window.localStorage.setItem(STORAGE_ADDR, receipt.contractAddress);
        setStatus(`deployed at ${receipt.contractAddress} (block ${receipt.blockNumber}) — the wallet that sent it is now the owner and an allowed searcher`);
      } else {
        setStatus("the deployment transaction reverted — nothing was deployed (you only paid gas)");
      }
    } catch (e) {
      setStatus(`deploy failed: ${(e as Error).message.split("\n")[0]}`);
    } finally {
      setBusy(false);
    }
  }, [address, eip1193, vault, weth]);

  const fundExecutor = useCallback(async () => {
    if (!address || !eip1193 || !deployed) return;
    setBusy(true);
    try {
      const hash = (await eip1193.request({
        method: "eth_sendTransaction",
        params: [{from: address, to: deployed, value: "0x" + parseEther(fundAmount).toString(16)}],
      })) as string;
      setStatus(`funding tx ${hash.slice(0, 10)}… sent — waiting for the receipt…`);
      const receipt = await publicClient.waitForTransactionReceipt({hash: hash as `0x${string}`});
      setStatus(
        receipt.status === "success"
          ? `executor funded with ${fundAmount} ETH`
          : "the funding transaction reverted"
      );
    } catch (e) {
      setStatus(`funding failed: ${(e as Error).message.split("\n")[0]}`);
    } finally {
      setBusy(false);
    }
  }, [address, eip1193, deployed, fundAmount]);

  const allowSearcher = useCallback(async () => {
    if (!address || !eip1193 || !deployed || !isAddress(searcher)) return;
    setBusy(true);
    try {
      const wallet = createWalletClient({transport: custom(eip1193)});
      const hash = (await wallet.writeContract({
        account: address as Address,
        address: deployed as Address,
        abi: EXECUTOR_ABI,
        functionName: "setSearcher",
        args: [searcher, true],
        chain: null,
      })) as string;
      setStatus(`setSearcher tx ${hash.slice(0, 10)}… sent — waiting for the receipt…`);
      const receipt = await publicClient.waitForTransactionReceipt({hash: hash as `0x${string}`});
      setStatus(receipt.status === "success" ? "searcher allowlisted" : "setSearcher reverted");
    } catch (e) {
      setStatus(`setSearcher failed: ${(e as Error).message.split("\n")[0]}`);
    } finally {
      setBusy(false);
    }
  }, [address, eip1193, deployed, searcher]);

  const copy = (label: string, text: string) => {
    void navigator.clipboard.writeText(text);
    setCopied(label);
    setTimeout(() => setCopied(null), 2000);
  };

  const envSnippet = deployed ? `# bot .env — then restart the bot\nEXECUTOR_ADDRESS=${deployed}${searcher && isAddress(searcher) ? `\nSEARCHER_ADDRESS=${searcher} # must match the searcher you allowlisted in step 5` : ""}` : "";

  return (
    <div style={{padding: 14, display: "grid", gap: 12}}>
      <p className="muted" style={{margin: 0, fontSize: 12, lineHeight: 1.6}}>
        <b style={{color: "var(--amber)"}}>Read this first.</b> Deploying the executor is a separate,
        earlier step than arming the bot — nothing about deploying makes the bot trade. Simulation
        doesn&apos;t need a deployment at all (the fork injects the bytecode); you deploy so the
        <b> live</b> path has an on-chain executor. Even an armed bot in this build records bundles
        rather than broadcasting — sending them is Phase 3. Full walkthrough:{" "}
        <code>docs/GO_LIVE.md</code>.
      </p>

      <Step n={1} title="connect a wallet on the right chain" done={Boolean(address) && rightChain}>
        {!address ? (
          <button onClick={() => void connect()} style={btn}>connect wallet</button>
        ) : (
          <span className="muted" style={{fontSize: 12}}>
            connected {address.slice(0, 10)}… on chain {walletChain}{" "}
            {rightChain ? "✓" : (
              <button onClick={() => void switchChain(homeChain)} style={{...btn, borderColor: "var(--red)", color: "var(--red)"}}>
                switch to chain {homeChain}
              </button>
            )}
          </span>
        )}
      </Step>

      <Step n={2} title="confirm the wallet holds gas money" done={gasOk}>
        <span className="muted" style={{fontSize: 12}}>
          balance: {balanceEth !== null ? `${balanceEth.toFixed(4)} ETH` : "—"} · recommended ≥{" "}
          {RECOMMENDED_GAS_ETH} ETH (deployment + a few admin transactions){" "}
          <button onClick={() => void refreshBalance()} style={btn}>refresh</button>
        </span>
      </Step>

      <Step n={3} title="deploy MevExecutor" done={Boolean(deployed)}>
        <div style={{display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center"}}>
          <label className="muted" style={{fontSize: 10}}>balancer vault</label>
          <input value={vault} onChange={(e) => setVault(e.target.value)} spellCheck={false} style={{...input, width: 340}} />
          <label className="muted" style={{fontSize: 10}}>weth</label>
          <input value={weth} onChange={(e) => setWeth(e.target.value)} spellCheck={false} style={{...input, width: 340}} />
        </div>
        <div style={{display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center"}}>
          <button onClick={() => void estimateCost()} disabled={!address || !rightChain || !isAddress(vault) || !isAddress(weth)} style={btn}>
            estimate cost
          </button>
          <button
            onClick={() => void deploy()}
            disabled={busy || !address || !rightChain || !gasOk || !isAddress(vault) || !isAddress(weth) || Boolean(deployed)}
            style={{...btn, borderColor: "var(--cyan)", color: "var(--cyan)"}}
          >
            {deployed ? "deployed ✓" : busy ? "working…" : "deploy"}
          </button>
          {estimate && <span className="muted" style={{fontSize: 11}}>
            {Number(formatUnits(estimate.gas * estimate.price, 18)).toFixed(4)} ETH at current gas
          </span>}
        </div>
        <div className="muted" style={{fontSize: 11}}>
          The deployer wallet becomes the <b>owner</b> and is automatically an allowed searcher.
          Constructor: <code>MevExecutor(balancerVault, weth)</code> — the defaults are mainnet&apos;s
          Balancer V2 vault and WETH9.
        </div>
      </Step>

      <Step n={4} title="fund the executor (gas for future bundles)" done={false}>
        <div style={{display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center"}}>
          <input value={fundAmount} onChange={(e) => setFundAmount(e.target.value)} spellCheck={false} style={{...input, width: 110}} />
          <button onClick={() => void fundExecutor()} disabled={busy || !deployed || !address || !/^\d*\.?\d+$/.test(fundAmount)} style={btn}>
            send ETH to executor
          </button>
          {deployed && (
            <a href={addressUrl(homeChain, deployed) ?? "#"} target="_blank" rel="noreferrer" style={{color: "var(--cyan)", fontSize: 12}}>
              {deployed.slice(0, 12)}… on {explorerName(homeChain)} ↗
            </a>
          )}
        </div>
        <div className="muted" style={{fontSize: 11}}>
          At live time the executor&apos;s transactions pay gas from its own balance (flash-loan
          capital comes from Balancer, not from this deposit). 0.02–0.05 ETH is plenty to start;
          you can always top up later, and the owner can <code>sweep</code> it back.
        </div>
      </Step>

      <Step n={5} title="allowlist the bot's searcher address" done={false}>
        <div style={{display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center"}}>
          <input
            value={searcher}
            onChange={(e) => {setSearcher(e.target.value); setSearcherSource("manual");}}
            placeholder="searcher EOA"
            spellCheck={false}
            style={{...input, width: 340}}
          />
          <button onClick={() => void allowSearcher()} disabled={busy || !deployed || !isAddress(searcher)} style={btn}>
            setSearcher(true)
          </button>
          <span className="muted" style={{fontSize: 10}}>
            {searcherSource === "bot" ? "prefilled from the bot's SEARCHER_ADDRESS" : "entered manually — must equal SEARCHER_ADDRESS in the bot's .env"}
          </span>
        </div>
      </Step>

      <Step n={6} title="point the bot at the executor and restart it" done={false}>
        {deployed ? (
          <div style={{display: "grid", gap: 6}}>
            <pre style={preStyle}>{envSnippet}</pre>
            <div style={{display: "flex", gap: 8}}>
              <button onClick={() => copy("env", envSnippet)} style={btn}>
                {copied === "env" ? "copied ✓" : "copy env lines"}
              </button>
              <button onClick={() => copy("addr", deployed)} style={btn}>
                {copied === "addr" ? "copied ✓" : "copy address"}
              </button>
            </div>
            <div className="muted" style={{fontSize: 11}}>
              After the restart the console&apos;s MevExecutor panel (below) reads your deployed
              address, and the bot&apos;s simulations use it instead of the injected placeholder.
            </div>
          </div>
        ) : (
          <span className="muted" style={{fontSize: 12}}>deploy first (step 3)</span>
        )}
      </Step>

      {status && <div className="muted" style={{fontSize: 11}}>{status}</div>}
      {deployHash && (
        <div className="muted" style={{fontSize: 11}}>
          deploy tx:{" "}
          <a
            href={`https://etherscan.io/tx/${deployHash}`}
            target="_blank"
            rel="noreferrer"
            style={{color: "var(--cyan)"}}
          >
            {deployHash.slice(0, 18)}… ↗
          </a>
        </div>
      )}
    </div>
  );
}

function Step({n, title, done, children}: {n: number; title: string; done: boolean; children: React.ReactNode}) {
  return (
    <div style={{display: "grid", gap: 6, borderTop: "1px solid var(--line)", paddingTop: 10}}>
      <div style={{display: "flex", gap: 8, alignItems: "baseline"}}>
        <span
          className="badge"
          style={{color: done ? "var(--green)" : "var(--muted)", border: "1px solid var(--line)"}}
        >
          {done ? `step ${n} ✓` : `step ${n}`}
        </span>
        <span style={{fontSize: 13}}>{title}</span>
      </div>
      {children}
    </div>
  );
}

const btn: React.CSSProperties = {
  background: "#111a25",
  border: "1px solid #24334a",
  borderRadius: 4,
  color: "#d7e2f0",
  padding: "4px 10px",
  cursor: "pointer",
  fontFamily: "inherit",
  fontSize: 11,
};

const input: React.CSSProperties = {
  background: "#070b11",
  border: "1px solid #1b2532",
  borderRadius: 4,
  color: "#d7e2f0",
  padding: "5px 8px",
  fontFamily: "inherit",
  fontSize: 12,
};

const preStyle: React.CSSProperties = {
  background: "#040608",
  padding: "8px 10px",
  borderRadius: 4,
  border: "1px solid var(--line)",
  color: "#a5b4fc",
  fontSize: 11,
  margin: 0,
  overflowX: "auto",
};
