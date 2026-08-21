"use client";

/**
 * Direct on-chain interaction with the deployed `MevExecutor`.
 *
 * Reads go through the server-side `/api/eth` proxy (works with the bot's own
 * `ETH_HTTP_URL`, no browser RPC configuration needed). Writes —
 * `setSearcher`, `sweep` — need a wallet and are signed through the shared
 * EIP-6963 wallet store (`lib/wallet.tsx`), with the resulting transaction
 * hash linked straight to the block explorer.
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
import {shortHash} from "@/lib/format";
import {addressUrl, explorerName, txUrl} from "@/lib/explorer";
import {useWallet} from "@/lib/wallet";

export default function ContractPanel({
  executor,
  chainId,
}: {
  executor: string;
  /** The bot's chain — used for explorer links and the wallet write chain. */
  chainId?: number;
}) {
  const {address, chainId: walletChain, connect, disconnect, activeName, eip1193} = useWallet();
  const [target, setTarget] = useState(executor);
  const [owner, setOwner] = useState<string | null>(null);
  const [balance, setBalance] = useState<string | null>(null);
  const [isSearcher, setIsSearcher] = useState<boolean | null>(null);
  const [searcherInput, setSearcherInput] = useState("");
  const [sweepAmount, setSweepAmount] = useState("0");
  const [status, setStatus] = useState<string>("");
  const [txHash, setTxHash] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const publicClient = useMemo(
    () => createPublicClient({chain: mainnet, transport: http("/api/eth")}),
    []
  );

  const read = useCallback(async () => {
    if (!isAddress(target)) {
      setStatus("not a valid address");
      return;
    }
    setStatus("reading…");
    try {
      const client = publicClient;
      const [o, b] = await Promise.all([
        client.readContract({address: target as Address, abi: EXECUTOR_ABI, functionName: "owner"}),
        client.getBalance({address: target as Address}),
      ]);
      setOwner(o as string);
      setBalance(formatEther(b));
      if (address) {
        const s = await client.readContract({
          address: target as Address,
          abi: EXECUTOR_ABI,
          functionName: "searchers",
          args: [address],
        });
        setIsSearcher(Boolean(s));
      } else {
        setIsSearcher(null);
      }
      setStatus("");
    } catch (e) {
      setOwner(null);
      setBalance(null);
      setStatus(`read failed: ${(e as Error).message.split("\n")[0]}`);
    }
  }, [target, address, publicClient]);

  useEffect(() => {
    setTarget(executor);
  }, [executor]);

  // Read on mount, on target/address change, and every 20s while idle.
  useEffect(() => {
    void read();
    const t = setInterval(() => void read(), 20_000);
    return () => clearInterval(t);
  }, [read]);

  const write = useCallback(
    async (functionName: string, args: unknown[]) => {
      if (!address) {
        setStatus("connect a wallet first");
        return;
      }
      setBusy(true);
      setStatus(`sending ${functionName}…`);
      setTxHash(null);
      try {
        if (!eip1193) throw new Error("wallet is not connected");
        // Chain-agnostic wallet client: the wallet's active chain decides
        // where the tx goes; the wrong-chain banner above warns if that
        // differs from the executor's chain.
        const wallet = createWalletClient({transport: custom(eip1193)});
        const hash = (await wallet.writeContract({
          account: address as Address,
          address: target as Address,
          abi: EXECUTOR_ABI,
          functionName,
          args,
          // Chain-agnostic client: the wallet's active chain decides. viem
          // requires the explicit null to acknowledge that.
          chain: null,
        })) as string;
        setTxHash(hash);
        setStatus(`sent ${shortHash(hash)} — waiting for receipt…`);
        // Follow the receipt through the same proxied RPC the reads use.
        const receipt = await publicClient.waitForTransactionReceipt({hash: hash as `0x${string}`});
        setStatus(
          receipt.status === "success"
            ? `mined ${shortHash(hash)} in block ${receipt.blockNumber}`
            : `reverted ${shortHash(hash)}`
        );
        void read();
      } catch (e) {
        setStatus(`failed: ${(e as Error).message.split("\n")[0]}`);
      } finally {
        setBusy(false);
      }
    },
    [address, target, eip1193, publicClient, read]
  );

  const wrongChain = address !== null && chainId !== undefined && walletChain !== null && walletChain !== chainId;
  const txLink = txUrl(chainId ?? walletChain ?? undefined, txHash);

  return (
    <div style={{padding: 12, display: "grid", gap: 10}}>
      <div style={{display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap"}}>
        <input
          value={target}
          onChange={(e) => setTarget(e.target.value)}
          spellCheck={false}
          style={inputStyle}
          placeholder="MevExecutor address"
        />
        <button onClick={() => void read()} style={btnStyle}>
          read
        </button>
        {address ? (
          <span style={{display: "flex", gap: 8, alignItems: "center"}}>
            <span className="badge" style={{color: "#22d3ee"}} title={activeName ?? undefined}>
              <span className="dot" style={{background: "#22d3ee"}} /> {shortHash(address, 6)}
            </span>
            <button onClick={() => void disconnect()} style={btnStyle}>
              disconnect
            </button>
          </span>
        ) : (
          <button onClick={() => void connect()} style={btnStyle}>
            connect wallet
          </button>
        )}
      </div>

      {address && wrongChain && (
        <div className="muted" style={{fontSize: 11, color: "#ff5c5c"}}>
          your wallet is on chain {walletChain}; the executor being read is on chain {chainId}. A
          write from the wrong chain will fail — switch your wallet first (header ▾).
        </div>
      )}

      <div style={{display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(150px, 1fr))", gap: 8}}>
        <Field
          label="owner"
          value={
            owner ? (
              <ExplorerLink chainId={chainId} addr={owner}>
                {shortHash(owner)}
              </ExplorerLink>
            ) : (
              "—"
            )
          }
        />
        <Field label="eth balance" value={balance ? `${Number(balance).toFixed(4)} ETH` : "—"} />
        <Field
          label="you are searcher"
          value={isSearcher === null ? "—" : isSearcher ? "yes" : "no"}
          tone={isSearcher ? "pos" : undefined}
        />
        <Field
          label="executor"
          value={
            isAddress(target) ? (
              <ExplorerLink chainId={chainId} addr={target}>
                {shortHash(target)}
              </ExplorerLink>
            ) : (
              "—"
            )
          }
        />
      </div>

      <div style={{display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center"}}>
        <input
          value={searcherInput}
          onChange={(e) => setSearcherInput(e.target.value)}
          placeholder="searcher address"
          spellCheck={false}
          style={{...inputStyle, minWidth: 320}}
        />
        <button
          disabled={busy || !isAddress(searcherInput)}
          onClick={() => void write("setSearcher", [searcherInput, true])}
          style={btnStyle}
        >
          allow searcher
        </button>
        <button
          disabled={busy || !isAddress(searcherInput)}
          onClick={() => void write("setSearcher", [searcherInput, false])}
          style={btnStyle}
        >
          revoke
        </button>
      </div>

      <div style={{display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center"}}>
        <input
          value={sweepAmount}
          onChange={(e) => setSweepAmount(e.target.value)}
          placeholder="ETH amount"
          spellCheck={false}
          style={{...inputStyle, minWidth: 140, width: 140}}
        />
        <button
          disabled={busy || !address || !isAddress(target) || !/^\d*\.?\d*$/.test(sweepAmount) || sweepAmount === ""}
          onClick={() =>
            void write("sweep", [
              "0x0000000000000000000000000000000000000000",
              address,
              parseEther(sweepAmount),
            ])
          }
          style={btnStyle}
          title="sweeps native ETH from the executor to the connected wallet"
        >
          sweep eth
        </button>
        <span className="muted" style={{fontSize: 11}}>
          owner-only · amount in ETH · `sweep(token, to, amount)`
        </span>
      </div>

      {status && (
        <div className="muted" style={{fontSize: 11}}>
          {status}
          {txLink && (
            <>
              {" "}
              <a href={txLink} target="_blank" rel="noreferrer" style={{color: "#22d3ee"}}>
                view on {explorerName(chainId ?? walletChain ?? undefined)} ↗
              </a>
            </>
          )}
        </div>
      )}
      <div className="muted" style={{fontSize: 11, lineHeight: 1.5}}>
        Reads are served by the dashboard&apos;s own RPC proxy (<code>/api/eth</code>); writes are
        signed by your wallet. The executor reverts any batch that does not net at least{" "}
        <code>minProfit</code>. Because bundles go through private orderflow, a reverting bundle is
        dropped by the builder and costs no gas.
      </div>
    </div>
  );
}

function ExplorerLink({
  chainId,
  addr,
  children,
}: {
  chainId?: number;
  addr: string;
  children: React.ReactNode;
}) {
  const url = addressUrl(chainId, addr);
  if (!url)
    return (
      <span title={addr} style={{cursor: "default"}}>
        {children}
      </span>
    );
  return (
    <a href={url} target="_blank" rel="noreferrer" style={{color: "#22d3ee", textDecoration: "none"}} title={addr}>
      {children}
    </a>
  );
}

function Field({label, value, tone}: {label: string; value: React.ReactNode; tone?: string}) {
  return (
    <div style={{background: "#0e141d", border: "1px solid #1b2532", borderRadius: 4, padding: "6px 8px"}}>
      <div className="muted" style={{fontSize: 10, textTransform: "uppercase", letterSpacing: "0.06em"}}>
        {label}
      </div>
      <div className={tone}>{value}</div>
    </div>
  );
}

const inputStyle: React.CSSProperties = {
  background: "#070b11",
  border: "1px solid #1b2532",
  borderRadius: 4,
  color: "#d7e2f0",
  padding: "5px 8px",
  minWidth: 380,
  fontFamily: "inherit",
  fontSize: 12,
};

const btnStyle: React.CSSProperties = {
  background: "#111a25",
  border: "1px solid #24334a",
  borderRadius: 4,
  color: "#d7e2f0",
  padding: "5px 10px",
  cursor: "pointer",
  fontFamily: "inherit",
  fontSize: 12,
};
