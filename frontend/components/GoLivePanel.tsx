"use client";

/**
 * Go-live readiness: the six-step MevExecutor deployment checklist.
 *
 * This is `docs/GO_LIVE.md` "Path A" as a panel:
 *   1. connect a wallet on mainnet        (EIP-6963, one-click chain switch)
 *   2. confirm the wallet holds gas money (≥ 0.05 ETH recommended)
 *   3. deploy MevExecutor                 (creation bytecode + constructor
 *                                          args encoded in the browser; the
 *                                          hex is a CI-checked copy of the
 *                                          bot artifact)
 *   4. fund the executor                  (0.02–0.05 ETH for its gas — not
 *                                          trading capital; swaps are
 *                                          Balancer flash-loan funded)
 *   5. allowlist the bot's searcher EOA   (setSearcher(searcher, true))
 *   6. verify + point the bot at it       (owner / searcher checks, then
 *                                          EXECUTOR_ADDRESS in .env)
 *
 * Reads and the free deployment estimate ride the server's read-only
 * `/api/eth` proxy. Only steps 3–5 open the wallet, and every step unlocks
 * only when the ones before it make sense. Deploying changes nothing about
 * the bot's behaviour: it stays simulation-only until the two-key arming.
 */

import {useCallback, useEffect, useMemo, useState} from "react";
import {
  createPublicClient,
  createWalletClient,
  custom,
  http,
  formatEther,
  isAddress,
  parseEther,
  type Address,
} from "viem";
import {mainnet} from "viem/chains";
import EXECUTOR_ABI from "@/lib/MevExecutor.abi.json";
import creationHex from "@/lib/MevExecutor.creation.hex";
import {addressUrl, explorerName, txUrl} from "@/lib/explorer";
import {useWallet} from "@/lib/wallet";

/** Constructor: `MevExecutor(address balancerVault, address weth)` — mainnet values. */
const BALANCER_VAULT = "0xBA12222222228d8Ba445958a75a0704d566BF2C8" as const;
const WETH9 = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2" as const;

const STORAGE_KEY = "jerseymikes.executor";

function padAddress(a: string): string {
  return a.toLowerCase().replace(/^0x/, "").padStart(64, "0");
}

/** Creation bytecode + ABI-encoded constructor args (two static address words). */
function deployData(): `0x${string}` {
  const hex = creationHex.trim().replace(/^0x/, "");
  return `0x${hex}${padAddress(BALANCER_VAULT)}${padAddress(WETH9)}` as `0x${string}`;
}

interface Step {
  key: string;
  title: string;
  state: "done" | "todo" | "locked";
  node: React.ReactNode;
}

export default function GoLivePanel({
  executor,
  armed,
}: {
  /** What the bot currently uses (sim fixture until EXECUTOR_ADDRESS is set). */
  executor?: string;
  /** Boot-time two-key arming state, for the final note. */
  armed?: boolean;
}) {
  const {address, chainId, balanceWei, connect, disconnect, activeName, eip1193, switchChain} = useWallet();

  const [searcher, setSearcher] = useState("");
  const [deployed, setDeployed] = useState<string | null>(null);
  const [deploying, setDeploying] = useState(false);
  const [deployHash, setDeployHash] = useState<string | null>(null);
  const [estimate, setEstimate] = useState<string | null>(null);
  const [fundAmount, setFundAmount] = useState("0.03");
  const [searcherInput, setSearcherInput] = useState("");
  const [allowTx, setAllowTx] = useState<string | null>(null);
  const [fundTx, setFundTx] = useState<string | null>(null);
  const [verify, setVerify] = useState<{
    code?: string;
    owner?: string;
    searcherAllowed?: boolean;
    executorBalance?: string;
  }>({});
  const [status, setStatus] = useState("");
  const [known, setKnown] = useState("");

  const publicClient = useMemo(() => createPublicClient({chain: mainnet, transport: http("/api/eth")}), []);

  // Bot's own signer EOA — prefills the allowlist step.
  useEffect(() => {
    fetch("/api/bot/config", {cache: "no-store"})
      .then((r) => r.json())
      .then((c: {searcher?: string}) => {
        if (c.searcher && isAddress(c.searcher)) {
          setSearcher(c.searcher);
          setSearcherInput(c.searcher);
        }
      })
      .catch(() => {});
  }, []);

  // Remember a deployment across reloads.
  useEffect(() => {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (saved && isAddress(saved)) setDeployed(saved);
  }, []);

  const target = known && isAddress(known) ? known : deployed ?? "";

  const refreshVerify = useCallback(async () => {
    if (!isAddress(target)) return;
    try {
      const [code, owner, executorBalance] = await Promise.all([
        publicClient.getCode({address: target as Address}),
        publicClient.readContract({address: target as Address, abi: EXECUTOR_ABI, functionName: "owner"}),
        publicClient.getBalance({address: target as Address}),
      ]);
      let searcherAllowed: boolean | undefined;
      if (searcher && isAddress(searcher)) {
        searcherAllowed = Boolean(
          await publicClient.readContract({
            address: target as Address,
            abi: EXECUTOR_ABI,
            functionName: "searchers",
            args: [searcher as Address],
          }),
        );
      }
      setVerify({code: `${(code?.length ?? 2) / 2 - 1} bytes`, owner: String(owner), searcherAllowed, executorBalance: formatEther(executorBalance)});
    } catch (e) {
      setVerify({code: `read failed: ${(e as Error).message.split("\n")[0]}`});
    }
  }, [target, searcher, publicClient]);

  useEffect(() => {
    void refreshVerify();
    const t = setInterval(() => void refreshVerify(), 20_000);
    return () => clearInterval(t);
  }, [refreshVerify]);

  const onWalletReady = chainId === 1 && Boolean(address);
  const walletFunded = onWalletReady && BigInt(balanceWei ?? 0) >= parseEther("0.005");

  const doEstimate = useCallback(async () => {
    if (!address) return;
    setStatus("estimating deployment cost…");
    try {
      const gas = await publicClient.estimateGas({account: address as Address, data: deployData()});
      const price = await publicClient.getGasPrice();
      setEstimate(`~${formatEther(BigInt(gas) * price).slice(0, 6)} ETH (${gas.toLocaleString()} gas @ ${Number(price) / 1e9} gwei)`);
      setStatus("");
    } catch (e) {
      setEstimate(null);
      setStatus(`estimate failed: ${(e as Error).message.split("\n")[0]}`);
    }
  }, [address, publicClient]);

  const doDeploy = useCallback(async () => {
    if (!address || !eip1193) return;
    setDeploying(true);
    setStatus("confirm the deployment in your wallet…");
    try {
      const wallet = createWalletClient({transport: custom(eip1193)});
      const hash = (await wallet.sendTransaction({
        account: address as Address,
        data: deployData(),
        chain: null,
        // The deployer pays gas; constructor has no payable side.
      })) as `0x${string}`;
      setDeployHash(hash);
      setStatus("mining — waiting for the receipt…");
      const receipt = await publicClient.waitForTransactionReceipt({hash});
      const ca = (receipt as unknown as {contractAddress?: string}).contractAddress;
      if (ca && isAddress(ca)) {
        setDeployed(ca);
        localStorage.setItem(STORAGE_KEY, ca);
        setStatus(`deployed at ${ca}`);
      } else {
        setStatus("receipt has no contract address — was the tx a deployment?");
      }
    } catch (e) {
      setStatus(`deploy failed: ${(e as Error).message.split("\n")[0]}`);
    } finally {
      setDeploying(false);
    }
  }, [address, eip1193, publicClient]);

  const doFund = useCallback(async () => {
    if (!address || !eip1193 || !isAddress(target)) return;
    setStatus("confirm the funding transfer in your wallet…");
    try {
      const wallet = createWalletClient({transport: custom(eip1193)});
      const hash = (await wallet.sendTransaction({
        account: address as Address,
        to: target as Address,
        value: parseEther(fundAmount || "0"),
        chain: null,
      })) as `0x${string}`;
      setFundTx(hash);
      setStatus(`funded ${fundAmount} ETH (tx pending)`);
      setTimeout(() => void refreshVerify(), 8_000);
    } catch (e) {
      setStatus(`fund failed: ${(e as Error).message.split("\n")[0]}`);
    }
  }, [address, eip1193, target, fundAmount, refreshVerify]);

  const doAllowSearcher = useCallback(async () => {
    if (!address || !eip1193 || !isAddress(target)) return;
    const who = searcherInput.trim();
    if (!isAddress(who)) {
      setStatus("searcher address is not valid");
      return;
    }
    setStatus("confirm setSearcher in your wallet…");
    try {
      const wallet = createWalletClient({transport: custom(eip1193)});
      const hash = (await wallet.writeContract({
        account: address as Address,
        address: target as Address,
        abi: EXECUTOR_ABI,
        functionName: "setSearcher",
        args: [who as Address, true],
        chain: null,
      })) as `0x${string}`;
      setAllowTx(hash);
      setStatus(`setSearcher sent (tx pending)`);
      setTimeout(() => {
        setSearcher(who);
        void refreshVerify();
      }, 8_000);
    } catch (e) {
      setStatus(`setSearcher failed: ${(e as Error).message.split("\n")[0]}`);
    }
  }, [address, eip1193, target, searcherInput, refreshVerify]);

  const deployedOk = isAddress(target) && Boolean(verify.code) && !verify.code?.startsWith("read failed") && verify.code !== "0 bytes";
  const fundedOk = deployedOk && Boolean(verify.executorBalance) && Number(verify.executorBalance ?? 0) > 0;
  const allowedOk = deployedOk && verify.searcherAllowed === true;
  const usingIt = !!executor && executor !== "" && executor.toLowerCase() === target.toLowerCase();

  const steps: Step[] = [
    {
      key: "wallet",
      title: "1 · Connect a wallet on Ethereum mainnet",
      state: onWalletReady ? "done" : "todo",
      node: (
        <div style={{display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap"}}>
          {address ? (
            <>
              <span style={{fontSize: 11}}>
                {activeName ?? "wallet"} · <code>{address.slice(0, 10)}…</code>{" "}
                {chainId === 1 ? (
                  <span style={{color: "var(--green)"}}>chain 1 ✓</span>
                ) : (
                  <span style={{color: "var(--amber)"}}>chain {chainId}</span>
                )}
              </span>
              {chainId !== 1 && (
                <button onClick={() => void switchChain(1)} style={btn}>
                  switch to mainnet
                </button>
              )}
              <button onClick={() => disconnect()} style={btn}>
                disconnect
              </button>
            </>
          ) : (
            <button onClick={() => void connect()} style={{...btn, borderColor: "var(--cyan)", color: "var(--cyan)"}}>
              connect wallet
            </button>
          )}
        </div>
      ),
    },
    {
      key: "gas",
      title: "2 · Confirm the wallet holds gas money",
      state: walletFunded ? "done" : onWalletReady ? "todo" : "locked",
      node: (
        <span style={{fontSize: 11}} className="muted">
          {address ? (
            <>
              balance <strong>{formatEther(BigInt(balanceWei ?? 0)).slice(0, 6)} ETH</strong> —{" "}
              {walletFunded ? (
                <span style={{color: "var(--green)"}}>≥ 0.005 ETH ok (0.05 recommended)</span>
              ) : (
                <span style={{color: "var(--amber)"}}>top up: deployment + admin txs + headroom</span>
              )}
            </>
          ) : (
            "connect first"
          )}
        </span>
      ),
    },
    {
      key: "deploy",
      title: "3 · Deploy MevExecutor",
      state: deployedOk ? "done" : walletFunded ? "todo" : "locked",
      node: (
        <div style={{display: "grid", gap: 6}}>
          <div style={{display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center"}}>
            <button onClick={() => void doEstimate()} disabled={!walletFunded} style={btn}>
              estimate cost (free)
            </button>
            <button
              onClick={() => void doDeploy()}
              disabled={!walletFunded || deploying}
              style={{...btn, borderColor: deployedOk ? "var(--line)" : "var(--green)", color: deployedOk ? "var(--muted)" : "var(--green)"}}
            >
              {deploying ? "deploying…" : deployedOk ? "redeploy" : "deploy"}
            </button>
            {estimate && <span style={{fontSize: 11, color: "var(--cyan)"}}>{estimate}</span>}
          </div>
          <p className="muted" style={{fontSize: 10, margin: 0}}>
            constructor <code>MevExecutor(balancerVault, weth)</code> — prefilled with mainnet Balancer V2 + WETH9. The
            creation bytecode is the CI-checked copy of the bot&apos;s artifact, so what you deploy is what the bot
            simulates against.
          </p>
          <div style={{display: "flex", gap: 8, alignItems: "center", fontSize: 11}}>
            <span className="muted">already deployed? paste the address:</span>
            <input value={known} onChange={(e) => setKnown(e.target.value)} placeholder="0x…" style={{...input, width: 320}} />
          </div>
          {deployHash && (
            <a href={txUrl(1, deployHash) ?? undefined} target="_blank" rel="noreferrer" style={{fontSize: 11, color: "var(--cyan)"}}>
              deployment tx ↗
            </a>
          )}
        </div>
      ),
    },
    {
      key: "fund",
      title: "4 · Fund the executor (its gas money)",
      state: fundedOk ? "done" : deployedOk ? "todo" : "locked",
      node: (
        <div style={{display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap"}}>
          <input value={fundAmount} onChange={(e) => setFundAmount(e.target.value)} style={{...input, width: 80}} />
          <span className="muted" style={{fontSize: 11}}>ETH →</span>
          <code style={{fontSize: 11}}>{target || "(deploy first)"}</code>
          <button onClick={() => void doFund()} disabled={!deployedOk} style={btn}>
            send
          </button>
          <span className="muted" style={{fontSize: 10}}>
            0.02–0.05 ETH. Not trading capital — swap capital is flash-loan funded; the owner can always sweep it back.
          </span>
          {verify.executorBalance && (
            <span style={{fontSize: 11, color: fundedOk ? "var(--green)" : "var(--amber)"}}>
              holds {verify.executorBalance.slice(0, 6)} ETH
            </span>
          )}
          {fundTx && (
            <a href={txUrl(1, fundTx) ?? undefined} target="_blank" rel="noreferrer" style={{fontSize: 11, color: "var(--cyan)"}}>
              tx ↗
            </a>
          )}
        </div>
      ),
    },
    {
      key: "allow",
      title: "5 · Allowlist the bot's searcher EOA",
      state: allowedOk ? "done" : deployedOk ? "todo" : "locked",
      node: (
        <div style={{display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap"}}>
          <input value={searcherInput} onChange={(e) => setSearcherInput(e.target.value)} placeholder="searcher address" style={{...input, width: 320}} />
          <button onClick={() => void doAllowSearcher()} disabled={!deployedOk} style={btn}>
            setSearcher(…, true)
          </button>
          <span className="muted" style={{fontSize: 10}}>
            prefilled from the bot&apos;s SEARCHER_ADDRESS. The executor only accepts bundles from allowlisted addresses.
          </span>
          {verify.searcherAllowed === true && <span style={{fontSize: 11, color: "var(--green)"}}>allowlisted ✓</span>}
          {verify.searcherAllowed === false && <span style={{fontSize: 11, color: "var(--amber)"}}>not allowlisted yet</span>}
          {allowTx && (
            <a href={txUrl(1, allowTx) ?? undefined} target="_blank" rel="noreferrer" style={{fontSize: 11, color: "var(--cyan)"}}>
              tx ↗
            </a>
          )}
        </div>
      ),
    },
    {
      key: "verify",
      title: "6 · Verify & point the bot at it",
      state: usingIt ? "done" : allowedOk ? "todo" : "locked",
      node: (
        <div style={{display: "grid", gap: 6, fontSize: 11}}>
          <div className="muted">
            code: <strong>{verify.code ?? "—"}</strong> · owner:{" "}
            {verify.owner ? (
              <a href={addressUrl(1, verify.owner) ?? undefined} target="_blank" rel="noreferrer" style={{color: "var(--cyan)"}}>
                {verify.owner.slice(0, 10)}… ↗
              </a>
            ) : (
              "—"
            )}
            {target && (
              <>
                {" "}· contract:{" "}
                <a href={addressUrl(1, target) ?? undefined} target="_blank" rel="noreferrer" style={{color: "var(--cyan)"}}>
                  {target.slice(0, 10)}… on {explorerName(1)} ↗
                </a>
              </>
            )}
          </div>
          <div className="muted">
            last step: set <code>EXECUTOR_ADDRESS={target || "<the deployed address>"}</code> in <code>.env</code> and
            restart — the bot is currently using{" "}
            <code>{executor ? executor.slice(0, 10) + "…" : "the simulation fixture"}</code>. Deploying changes nothing
            else: the bot stays simulation-only until{" "}
            <code>LIVE_EXECUTION=true</code> + <code>I_UNDERSTAND_LIVE_RISK=yes</code>
            {armed === false && <span style={{color: "var(--green)"}}> (currently unarmed ✓)</span>}.
          </div>
          <button
            onClick={() => {
              void navigator.clipboard.writeText(`EXECUTOR_ADDRESS=${target}`);
              setStatus("copied EXECUTOR_ADDRESS to clipboard");
            }}
            disabled={!isAddress(target)}
            style={{...btn, justifySelf: "start"}}
          >
            copy EXECUTOR_ADDRESS line
          </button>
        </div>
      ),
    },
  ];

  const done = steps.filter((s) => s.state === "done").length;

  return (
    <div style={{display: "grid", gap: 10}}>
      <div style={{display: "flex", justifyContent: "space-between", alignItems: "center"}}>
        <span className="muted" style={{fontSize: 11}}>
          {done}/6 steps complete · full walkthrough in <code>docs/GO_LIVE.md</code>
        </span>
        {status && (
          <span style={{fontSize: 11, color: "var(--amber)"}}>{status}</span>
        )}
      </div>
      {steps.map((s) => (
        <div
          key={s.key}
          style={{
            display: "grid",
            gridTemplateColumns: "20px 1fr",
            gap: 10,
            padding: "8px 10px",
            border: "1px solid var(--line)",
            borderRadius: 4,
            background: s.state === "locked" ? "transparent" : "var(--panel-2)",
            opacity: s.state === "locked" ? 0.5 : 1,
          }}
        >
          <span style={{color: s.state === "done" ? "var(--green)" : "var(--muted)", fontSize: 13, lineHeight: "20px"}}>
            {s.state === "done" ? "✓" : s.state === "locked" ? "🔒" : "○"}
          </span>
          <div style={{display: "grid", gap: 6}}>
            <span style={{fontSize: 12, fontWeight: 600, color: s.state === "done" ? "var(--muted)" : "inherit"}}>
              {s.title}
            </span>
            {s.node}
          </div>
        </div>
      ))}
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
  fontSize: 11,
};
