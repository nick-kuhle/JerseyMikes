"use client";

import {useCallback, useEffect, useState} from "react";
import {createPublicClient, createWalletClient, custom, http, formatEther, isAddress, type Address} from "viem";
import {mainnet} from "viem/chains";
import EXECUTOR_ABI from "@/lib/MevExecutor.abi.json";
import {shortHash} from "@/lib/format";

/**
 * Direct on-chain interaction with the deployed `MevExecutor`.
 *
 * Reads work with a plain RPC (no wallet needed). Writes — `setSearcher`,
 * `sweep` — require the owner's wallet, so they are gated behind a connection.
 */
export default function ContractPanel({executor, rpcUrl}: {executor: string; rpcUrl?: string}) {
  const [address, setAddress] = useState<Address | null>(null);
  const [target, setTarget] = useState(executor);
  const [owner, setOwner] = useState<string | null>(null);
  const [balance, setBalance] = useState<string | null>(null);
  const [isSearcher, setIsSearcher] = useState<boolean | null>(null);
  const [searcherInput, setSearcherInput] = useState("");
  const [status, setStatus] = useState<string>("");
  const [busy, setBusy] = useState(false);

  const publicClient = useCallback(() => {
    return createPublicClient({chain: mainnet, transport: http(rpcUrl || undefined)});
  }, [rpcUrl]);

  const read = useCallback(async () => {
    if (!isAddress(target)) {
      setStatus("not a valid address");
      return;
    }
    setStatus("reading…");
    try {
      const client = publicClient();
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
      }
      setStatus("");
    } catch (e) {
      setStatus(`read failed: ${(e as Error).message.split("\n")[0]}`);
    }
  }, [target, address, publicClient]);

  const connect = useCallback(async () => {
    const eth = (globalThis as {ethereum?: {request: (a: unknown) => Promise<string[]>}}).ethereum;
    if (!eth) {
      setStatus("no injected wallet found");
      return;
    }
    const accounts = await eth.request({method: "eth_requestAccounts"});
    setAddress(accounts[0] as Address);
  }, []);

  const write = useCallback(
    async (functionName: string, args: unknown[]) => {
      const eth = (globalThis as {ethereum?: never}).ethereum;
      if (!eth || !address) {
        setStatus("connect a wallet first");
        return;
      }
      setBusy(true);
      setStatus(`sending ${functionName}…`);
      try {
        const wallet = createWalletClient({chain: mainnet, transport: custom(eth)});
        const hash = await wallet.writeContract({
          account: address,
          address: target as Address,
          abi: EXECUTOR_ABI,
          functionName,
          args,
        });
        setStatus(`sent ${shortHash(hash)}`);
      } catch (e) {
        setStatus(`failed: ${(e as Error).message.split("\n")[0]}`);
      } finally {
        setBusy(false);
      }
    },
    [address, target]
  );

  useEffect(() => {
    setTarget(executor);
  }, [executor]);

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
        <button onClick={read} style={btnStyle}>
          read
        </button>
        <button onClick={connect} style={btnStyle}>
          {address ? shortHash(address) : "connect wallet"}
        </button>
      </div>

      <div style={{display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(150px, 1fr))", gap: 8}}>
        <Field label="owner" value={owner ? shortHash(owner) : "—"} />
        <Field label="eth balance" value={balance ? `${Number(balance).toFixed(4)} ETH` : "—"} />
        <Field
          label="you are searcher"
          value={isSearcher === null ? "—" : isSearcher ? "yes" : "no"}
          tone={isSearcher ? "pos" : undefined}
        />
      </div>

      <div style={{display: "flex", gap: 8, flexWrap: "wrap"}}>
        <input
          value={searcherInput}
          onChange={(e) => setSearcherInput(e.target.value)}
          placeholder="searcher address"
          spellCheck={false}
          style={{...inputStyle, minWidth: 320}}
        />
        <button
          disabled={busy || !isAddress(searcherInput)}
          onClick={() => write("setSearcher", [searcherInput, true])}
          style={btnStyle}
        >
          allow searcher
        </button>
        <button
          disabled={busy || !isAddress(searcherInput)}
          onClick={() => write("setSearcher", [searcherInput, false])}
          style={btnStyle}
        >
          revoke
        </button>
        <button
          disabled={busy || !address}
          onClick={() => write("sweep", ["0x0000000000000000000000000000000000000000", address, 0n])}
          style={btnStyle}
          title="sweeps 0 wei — edit the amount before using in anger"
        >
          sweep eth
        </button>
      </div>

      {status && <div className="muted">{status}</div>}
      <div className="muted" style={{fontSize: 11, lineHeight: 1.5}}>
        The executor reverts any batch that does not net at least <code>minProfit</code>. Because bundles go through
        private orderflow, a reverting bundle is dropped by the builder and costs no gas.
      </div>
    </div>
  );
}

function Field({label, value, tone}: {label: string; value: string; tone?: string}) {
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
