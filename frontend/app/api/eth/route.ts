import {NextRequest} from "next/server";

export const dynamic = "force-dynamic";

/**
 * Server-side JSON-RPC proxy for *read-only* contract calls.
 *
 * The dashboard's `MevExecutor` panel needs an RPC to read `owner`,
 * `searchers` and balances. Routing those reads through the server means:
 *   - no `NEXT_PUBLIC_RPC_URL` required in the browser — the bot's own
 *     endpoint (`ETH_HTTP_URL`, server-side only) works and stays private;
 *   - the preview/sandbox works without any browser-side network access to
 *     an Ethereum node.
 *
 * Only read methods are forwarded. Anything that could sign, send or unlock
 * is rejected before it ever reaches a node.
 */

const READ_METHODS = new Set([
  "eth_call",
  // Dry-run only: executes against a simulated block, publishes nothing.
  // The go-live panel uses it to price the MevExecutor deployment.
  "eth_estimateGas",
  "eth_chainId",
  "eth_blockNumber",
  "eth_getBalance",
  "eth_getCode",
  "eth_getStorageAt",
  "eth_getLogs",
  "eth_getTransactionByHash",
  "eth_getTransactionReceipt",
  "eth_getTransactionCount",
  "eth_gasPrice",
  "eth_getBlockByNumber",
  "eth_getBlockByHash",
  "net_version",
  "web3_clientVersion",
]);

const UPSTREAM =
  process.env.ETH_PROXY_URL ||
  process.env.ETH_HTTP_URL ||
  process.env.NEXT_PUBLIC_RPC_URL ||
  // Keyless public endpoint, used only when no other RPC is configured.
  // Set ETH_PROXY_URL to use your own node/provider instead.
  "https://ethereum-rpc.publicnode.com";

function err(message: string, status = 400) {
  return new Response(JSON.stringify({error: {message}}), {
    status,
    headers: {"content-type": "application/json"},
  });
}

export async function POST(req: NextRequest) {
  let body: {method?: string; params?: unknown[]; jsonrpc?: string; id?: unknown};
  try {
    body = await req.json();
  } catch {
    return err("invalid JSON body");
  }
  const method = String(body.method ?? "");
  if (!READ_METHODS.has(method)) {
    return err(`method ${method || "(none)"} is not on the read-only allowlist`, 403);
  }

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 8_000);
  try {
    const upstream = await fetch(UPSTREAM, {
      method: "POST",
      signal: controller.signal,
      headers: {"content-type": "application/json"},
      body: JSON.stringify({...body, jsonrpc: "2.0"}),
      cache: "no-store",
    });
    const text = await upstream.text();
    return new Response(text, {
      status: upstream.status,
      headers: {"content-type": "application/json", "x-rpc-upstream": "jerseymikes"},
    });
  } catch (e) {
    return err(`rpc proxy failed: ${(e as Error).message.split("\n")[0]}`, 502);
  } finally {
    clearTimeout(timer);
  }
}
