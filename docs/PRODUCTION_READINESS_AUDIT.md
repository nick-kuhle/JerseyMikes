# JerseyMikes token-sniper production audit

**Audit date:** 2026-08-24  
**Branch:** `work/production-readiness`  
**Scope:** bundled Sniper V2 work order, Ethereum Mainnet and Base paths

## Release position

This branch bundles the four requested frontend-oriented work packages with
bot and contract support. It is safe to review and stage, but it is **not a
claim that live trading should be armed without operator evidence**. This
checkout contains no treasury address, deployed addresses, paid RPC
credentials, relay credentials, or signing keys. All live defaults remain
fail-closed.

## Implemented work packages

### WP-1 — SniperVault onboarding

- Added `frontend/components/SniperVaultWizard.tsx` and mounted it in the
  Sniper panel.
- Flow is connect/switch network → deploy or paste vault → verify WETH/code →
  allowlist `SNIPER_SEARCHER_ADDRESS` → wrap and transfer WETH → bind
  `{vaultAddress}` through the authenticated bot control plane.
- The wizard embeds the generated `SniperVault` creation bytecode and waits
  for receipts through the selected-chain RPC proxy.
- Added `contracts/script/DeploySniperVault.s.sol`; it selects Mainnet or Base
  WETH from `block.chainid`, applies the configured daily/lifetime budget and
  allowlists the dedicated sniper searcher.
- The existing platform Go-Live panel also verifies both `MevExecutor` and
  `SniperVault`, and now has native/WETH funding and independent lane controls.

### WP-2 — 1 ETH paper simulator

- `SniperLane` initializes an isolated `1_000_000_000_000_000_000` wei paper
  bankroll.
- Simulation-mode entries reserve paper funds and create real persisted
  positions with `[SIMULATION]`-equivalent notes/fill reasons; reserve-derived
  marks drive exits.
- Paper exits credit simulated WETH proceeds and persist exact paper fills.
- Added receipt reconciliation for pending automatic entries so RPC mempool
  acceptance is not treated as settlement.
- Added `POST /api/sniper/paper/reset`, with a visible **SIMULATION WALLET**
  panel and reset-to-1-ETH control in the UI. Live-armed processes reject the
  reset operation.
- Paper mode can be enabled without a deployed vault or live signer; a live
  vault-bound envelope still obeys the boot ceiling.

### WP-3 — Trade / Charts terminal

- Replaced the old external-link-only swap pane with
  `frontend/components/TradeTerminal.tsx` and renamed the tab to
  **Trade / Charts**.
- Added ERC-20 lookup by address, symbol/decimals resolution, current-chain V2
  factory pair resolution and reserve-based output preview.
- Added embedded DexScreener charting plus a DexTools fallback link.
- Added chain-specific Mainnet/Base router and factory bindings and generated
  contract artifacts from the same compile-check pipeline.
- External links remain available as research fallbacks rather than being
  presented as in-app execution.

### WP-4 — In-app trade execution

- Added Buy/Sell controls, 25/50/75/MAX selectors, 0.005 ETH buy gas reserve,
  exact ERC-20 balance reads for sell MAX, slippage presets and route
  preference controls.
- Direct wallet execution uses only explicit known V2 router ABI calls; no
  arbitrary calldata is accepted by the UI.
- Added strict authenticated `POST /api/sniper/trade` bot-signer endpoint for
  normalized, bounded SniperVault buy/sell intents.
- Browser MEV-Safe preference is intentionally not falsely advertised: the
  terminal blocks that route until a private bot/relay path is configured.

### WP-5 — 1% fee architecture

- Added fee math and configuration helpers in `frontend/lib/swap.ts`:
  `calculatePlatformFee`, net amount calculation and fee-router configuration.
- Added `contracts/src/JerseyMikesFeeRouter.sol`, which has an immutable
  non-zero treasury, 100 bps fee, owner-managed router allowlist, atomic native
  or ERC-20 fee collection, allowance cleanup and reentrancy protection.
- Added `DeployFeeRouter.s.sol` and three Foundry tests covering native fee
  exactness, ERC-20 fee/allowance behavior and router allowlisting.
- Fee-enabled browser execution is blocked unless both
  `NEXT_PUBLIC_PLATFORM_FEE_RECIPIENT` and the deployed
  `NEXT_PUBLIC_PLATFORM_FEE_ROUTER_ADDRESS` are present. This is deliberate:
  no treasury address was supplied in the work order, and a direct swap must
  not silently bypass an advertised fee.

## Cross-cutting hardening bundled from the prior production path

- Dedicated atomic and sniper searcher key domains, with derived-address
  validation and redacted serialization/debug output.
- `BASE_HTTP_URL` / `BASE_WS_URL` selected-chain aliases while retaining
  legacy `ETH_*` compatibility. A selected Base process cannot silently use a
  Mainnet endpoint when its Base binding is missing.
- Dynamic authenticated `POST /api/qualification` for a 1–8760 hour operator
  soak threshold. It changes only the evidence window; it cannot manufacture
  evidence or override per-strategy gates.
- Authenticated bot pre-flight endpoint with RPC chain-id, relay/raw-path,
  feed and qualification checks.
- Canonical same-origin `/api/stream?chain=ethereum|base` route.
- Sniper parameter API contract corrected to camelCase; a Rust regression test
  covers the exact JSON shape.
- Sniper control writes no longer fall back to successful demo mutations when
  the selected bot is unreachable. Paper reset is the only harmless demo
  mutation when explicitly enabled.

## Verification

- `cargo test --all`: **402 passed**.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `forge test -vv`: **65 passed** after adding the three fee-router tests.
- `forge fmt --check`: passed.
- `forge build --sizes`: `MevExecutor` runtime remains **11,497 bytes**;
  `SniperVault` runtime remains **7,829 bytes**; fee router runtime is 4,796
  bytes.
- `node contracts/script/compile-check.js`: passed; ABI/creation artifacts
  are generated from the same Solidity source compilation.
- `npm run typecheck`: passed.
- `npm run build`: passed with `/api/stream` and all dashboard routes present.
- Base simulation smoke against `https://mainnet.base.org`: 4 heads, 3
  delivered chain blocks and **700 real Base transactions** ingested through
  the replay/strategy path; no transaction was broadcast.

## Explicit remaining blockers before real money

1. Supply and verify per-chain paid archive RPC/WebSocket or Flashblocks
   endpoints, builder/relay configuration, API auth tokens, deployed contract
   addresses, and dedicated keys outside Git.
2. Supply the actual treasury address and deploy/verify the fee router on each
   chain before enabling fee-configured terminal trades.
3. Complete the operator soak and review canonical, relay/actual and sniper
   paper evidence. Dynamic threshold selection is not a substitute for a
   sound decision.
4. Add a durable automatic-exit intent column/reconciler before unattended
   directional operation; manual exits now wait for and decode receipts, while
   the legacy automatic exit loop still has a follow-up reconciliation item.
5. Treat Base as real ingestion/replay measurement, not as certified
   competitive revenue. Aerodrome discovery, Flashblocks state ordering and
   the successor Base revenue-path work order remain outside this bundle.
6. Run a formal contract/security review of the fee-router's owner-approved
   router set and ensure every router calldata builder sends output to the
   trader, as required by the wrapper's interface contract.

Recommended sequence: deploy and verify → allowlist → fund → persist chain env
→ run paper/shadow → review evidence → configure treasury/fee router → enable
only the intended lane with explicit operator confirmation.
