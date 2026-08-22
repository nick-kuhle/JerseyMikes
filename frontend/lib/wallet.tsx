"use client";

/**
 * Wallet connection store (EIP-6963 + legacy injected fallback).
 *
 * Multi-wallet discovery follows EIP-6963: every injected wallet announces
 * itself with a UUID, name and icon, and the console lets the user pick when
 * several are installed. When nothing announces (older wallets), we fall back
 * to `window.ethereum`.
 *
 * The store keeps: connected address, chain id, ETH balance (read through the
 * server-side `/api/eth` proxy so it works without any browser RPC config),
 * and reacts to `accountsChanged` / `chainChanged` from the wallet.
 */

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";

/* ── EIP-6963 types ───────────────────────────────────────────────────────── */

interface Eip1193Provider {
  request(args: {method: string; params?: unknown[]}): Promise<unknown>;
  on?(event: string, handler: (...args: unknown[]) => void): void;
  removeListener?(event: string, handler: (...args: unknown[]) => void): void;
}

export interface WalletProviderInfo {
  uuid: string;
  name: string;
  icon: string;
  rd: Eip1193Provider;
}

interface AnnounceProviderEvent extends CustomEvent {
  type: "eip6963:announceProvider";
  detail: WalletProviderInfo;
}

/* ── Chain metadata for add/switch ────────────────────────────────────────── */

const CHAIN_PARAMS: Record<number, {hex: string; name: string; rpc: string; symbol: string; explorer: string}> = {
  1: {
    hex: "0x1",
    name: "Ethereum Mainnet",
    rpc: "https://ethereum-rpc.publicnode.com",
    symbol: "ETH",
    explorer: "https://etherscan.io",
  },
  8453: {
    hex: "0x2105",
    name: "Base",
    rpc: "https://base-rpc.publicnode.com",
    symbol: "ETH",
    explorer: "https://basescan.org",
  },
  42161: {
    hex: "0xa4b1",
    name: "Arbitrum One",
    rpc: "https://arbitrum-one-rpc.publicnode.com",
    symbol: "ETH",
    explorer: "https://arbiscan.io",
  },
  10: {
    hex: "0xa",
    name: "OP Mainnet",
    rpc: "https://optimism-rpc.publicnode.com",
    symbol: "ETH",
    explorer: "https://optimistic.etherscan.io",
  },
  137: {
    hex: "0x89",
    name: "Polygon PoS",
    rpc: "https://polygon-bor-rpc.publicnode.com",
    symbol: "POL",
    explorer: "https://polygonscan.com",
  },
};

const STORAGE_KEY = "jm.wallet.uuid";

/* ── Store ────────────────────────────────────────────────────────────────── */

interface WalletState {
  /** Every wallet that announced itself (EIP-6963) + legacy `window.ethereum`. */
  providers: WalletProviderInfo[];
  address: string | null;
  chainId: number | null;
  balanceWei: bigint | null;
  connecting: boolean;
  error: string | null;
  /** The wallet that is (or would be) connected — survives reloads. */
  activeName: string | null;
  /** The EIP-1193 provider bound to the current connection, if any. */
  eip1193: Eip1193Provider | null;
  connect: (provider?: WalletProviderInfo) => Promise<void>;
  disconnect: () => Promise<void>;
  switchChain: (chainId: number) => Promise<void>;
  refreshBalance: () => Promise<void>;
}

const WalletContext = createContext<WalletState | null>(null);

export function WalletProvider({children}: {children: ReactNode}) {
  const [providers, setProviders] = useState<WalletProviderInfo[]>([]);
  const [address, setAddress] = useState<string | null>(null);
  const [chainId, setChainId] = useState<number | null>(null);
  const [balanceWei, setBalanceWei] = useState<bigint | null>(null);
  const [connecting, setConnecting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [activeName, setActiveName] = useState<string | null>(null);
  /** The EIP-1193 provider we are bound to. Ref, not state: it never renders. */
  const bound = useRef<Eip1193Provider | null>(null);
  const [eip1193, setEip1193] = useState<Eip1193Provider | null>(null);

  const onAnnounce = useCallback((ev: Event) => {
    const detail = (ev as AnnounceProviderEvent).detail;
    if (!detail?.uuid) return;
    setProviders((prev) =>
      prev.some((p) => p.uuid === detail.uuid) ? prev : [...prev, detail]
    );
  }, []);

  /* Discovery: listen for announcements, then re-request them (EIP-6963). */
  useEffect(() => {
    window.addEventListener("eip6963:announceProvider", onAnnounce);
    window.dispatchEvent(new Event("eip6963:requestProvider"));
    const legacy = (window as {ethereum?: Eip1193Provider & {isMetaMask?: boolean}}).ethereum;
    if (legacy) {
      setProviders((prev) =>
        prev.some((p) => p.uuid === "legacy-injected")
          ? prev
          : [
              ...prev,
              {
                uuid: "legacy-injected",
                name: "Injected wallet",
                icon: "",
                rd: legacy,
              },
            ]
      );
    }
    return () => window.removeEventListener("eip6963:announceProvider", onAnnounce);
  }, [onAnnounce]);

  /**
   * Last balance read, so repeat reads for the same account inside the TTL are
   * served from memory.
   *
   * Wallets fire `accountsChanged` / `chainChanged` more often than the
   * underlying state actually changes (some emit on every focus, and a chain
   * switch emits both), and each event used to trigger a fresh `eth_chainId`
   * plus a proxied `eth_getBalance`. The balance is decoration in this
   * console — it does not gate any action — so a short cache is free.
   */
  const balanceCache = useRef<{who: string; at: number; wei: bigint} | null>(null);
  const BALANCE_TTL_MS = 10_000;

  const readChainAndBalance = useCallback(async (who: string, force = false) => {
    const prov = bound.current;
    if (!prov) return;
    try {
      const cid = (await prov.request({method: "eth_chainId"})) as string;
      setChainId(parseInt(cid, 16));
    } catch {
      /* wallet did not answer; chain stays unknown */
    }
    const cached = balanceCache.current;
    if (
      !force &&
      cached &&
      cached.who.toLowerCase() === who.toLowerCase() &&
      Date.now() - cached.at < BALANCE_TTL_MS
    ) {
      setBalanceWei(cached.wei);
      return;
    }
    try {
      const r = await fetch("/api/eth", {
        method: "POST",
        headers: {"content-type": "application/json"},
        body: JSON.stringify({jsonrpc: "2.0", id: 1, method: "eth_getBalance", params: [who, "latest"]}),
      });
      const j = (await r.json()) as {result?: string};
      if (typeof j.result === "string") {
        const wei = BigInt(j.result);
        balanceCache.current = {who, at: Date.now(), wei};
        setBalanceWei(wei);
      }
    } catch {
      setBalanceWei(null);
    }
  }, []);

  /* Stable EIP-1193 event handlers. Declared once so bind/unbind can add and
   * remove the exact same function reference from any provider. */
  const handleAccounts = useCallback(
    (accts: unknown) => {
      const list = accts as string[];
      if (!Array.isArray(list) || list.length === 0) {
        setAddress(null);
        setBalanceWei(null);
        setChainId(null);
        return;
      }
      setAddress(list[0]);
      // The account genuinely changed — do not serve a stale balance.
      void readChainAndBalance(list[0], true);
    },
    [readChainAndBalance]
  );

  const handleChain = useCallback((cid: unknown) => {
    if (typeof cid === "string") setChainId(parseInt(cid, 16));
    else if (typeof cid === "number") setChainId(cid);
  }, []);

  const bind = useCallback(
    (info: WalletProviderInfo) => {
      // Unbind the previous provider's event handlers, if any.
      const prev = bound.current;
      if (prev?.removeListener) {
        prev.removeListener("accountsChanged", handleAccounts);
        prev.removeListener("chainChanged", handleChain);
      }
      bound.current = info.rd;
      setEip1193(info.rd);
      setActiveName(info.name);
      if (info.rd.on) {
        info.rd.on("accountsChanged", handleAccounts);
        info.rd.on("chainChanged", handleChain);
      }
    },
    [handleAccounts, handleChain]
  );

  /* Eager reconnect: if the user connected before, restore silently. */
  useEffect(() => {
    if (bound.current || !providers.length) return;
    const stored = window.localStorage.getItem(STORAGE_KEY);
    if (!stored) return;
    const match = providers.find((p) => p.uuid === stored);
    if (!match) return;
    let cancelled = false;
    (async () => {
      try {
        const accts = (await match.rd.request({method: "eth_accounts"})) as string[];
        if (cancelled || !Array.isArray(accts) || accts.length === 0) return;
        bind(match);
        setAddress(accts[0]);
        void readChainAndBalance(accts[0]);
      } catch {
        /* provider refused; user reconnects manually */
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [providers, bind, readChainAndBalance]);

  const connect = useCallback(
    async (provider?: WalletProviderInfo) => {
      setError(null);
      const target =
        provider ?? providers[0] ?? null;
      if (!target) {
        setError("no injected wallet found — install MetaMask, Rabby or another EIP-1193 wallet");
        return;
      }
      setConnecting(true);
      try {
        const accts = (await target.rd.request({method: "eth_requestAccounts"})) as string[];
        if (!Array.isArray(accts) || accts.length === 0) throw new Error("no accounts returned");
        window.localStorage.setItem(STORAGE_KEY, target.uuid);
        bind(target);
        setAddress(accts[0]);
        await readChainAndBalance(accts[0]);
      } catch (e) {
        setError((e as Error).message?.split("\n")[0] || "connection rejected");
      } finally {
        setConnecting(false);
      }
    },
    [providers, bind, readChainAndBalance]
  );

  const disconnect = useCallback(async () => {
    // Best-effort permission revoke; not all wallets support it.
    const prov = bound.current;
    if (prov) {
      try {
        await prov.request({method: "wallet_revokePermissions", params: [{eth_accounts: {}}]});
      } catch {
        /* fine — clearing local state is enough for the console */
      }
      if (prov.removeListener) {
        prov.removeListener("accountsChanged", handleAccounts);
        prov.removeListener("chainChanged", handleChain);
      }
    }
    window.localStorage.removeItem(STORAGE_KEY);
    balanceCache.current = null;
    bound.current = null;
    setEip1193(null);
    setAddress(null);
    setChainId(null);
    setBalanceWei(null);
    setActiveName(null);
  }, [handleAccounts, handleChain]);

  const switchChain = useCallback(async (target: number) => {
    const prov = bound.current;
    if (!prov) return;
    const params = CHAIN_PARAMS[target];
    try {
      await prov.request({
        method: "wallet_switchEthereumChain",
        params: [{chainId: params?.hex ?? `0x${target.toString(16)}`}],
      });
    } catch (e) {
      const code = (e as {code?: number}).code;
      if (code === 4902 && params) {
        await prov.request({method: "wallet_addEthereumChain", params: [params]});
      } else if (code === 4001) {
        throw new Error("switch rejected in wallet");
      } else {
        throw e;
      }
    }
  }, []);

  /** Explicit user action — always hits the network. */
  const refreshBalance = useCallback(async () => {
    if (address) await readChainAndBalance(address, true);
  }, [address, readChainAndBalance]);

  const value = useMemo<WalletState>(
    () => ({
      providers,
      address,
      chainId,
      balanceWei,
      connecting,
      error,
      activeName,
      eip1193,
      connect,
      disconnect,
      switchChain,
      refreshBalance,
    }),
    [providers, address, chainId, balanceWei, connecting, error, activeName, eip1193, connect, disconnect, switchChain, refreshBalance]
  );

  return <WalletContext.Provider value={value}>{children}</WalletContext.Provider>;
}

export function useWallet(): WalletState {
  const ctx = useContext(WalletContext);
  if (!ctx) throw new Error("useWallet must be used inside <WalletProvider>");
  return ctx;
}
