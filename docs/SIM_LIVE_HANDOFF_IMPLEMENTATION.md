# Implementation Notes — Sniper Simulation Vault, Independent Mode Switch, and Live Portfolio

Work order: `WORK_ORDER_SNIPER_SIM_LIVE_HANDOFF.md`
Branch: `work/sniper-sim-live-handoff`

---

## What was built

### Work Package A — Simulation Vault Fixture ✅

**`bot/crates/mev-bot/src/sniper/sim_vault.rs` (new)**

- Deploys the **real `SniperVault.sol` bytecode** (constructor and all) into the
  bot's existing local anvil fork — no second anvil process, no per-click
  forks. Creation/ABI artifacts are emitted by `contracts/script/compile-check.js`
  so the fixture is byte-identical to a production deployment.
- Deterministic simulation identities derived from the built-in simulation
  signer (`Signer::SIMULATION_KEY` + domain tag) — **never**
  `SNIPER_SEARCHER_PRIVATE_KEY`. Owner and searcher are distinct; the searcher
  is allowlisted via `setSearcher`.
- Chain-specific WETH binding verified after deploy (`WETH()` read-back). On a
  bare local anvil (CI integration tests) the fixture deploys its own MockWETH;
  on a fork it wraps/transfer funds the vault through the chain's real WETH.
- Per simulated launch: deterministic MockERC20 + `SimV2Pair` (a faithful
  UniswapV2 flash-swap mock — optimistic outputs + K invariant + 0.3% fee)
  seeded with the launch's observed reserves, so the **exact** live calldata
  shape executes.
- Self-healing across `anvil_reset`: fixture state is re-checked lazily,
  CREATE-collision retries bump the deployer nonce, and lost pairs are rebuilt
  from a seed cache.
- Revert decoding names the contract guard (`DailyBudgetExceeded(...)`,
  `Deadline()`, `CallFailed: Error("SimV2Pair: K")`, ...) for the ledger notes
  and console.

**Contract-backed semantics (work order A.2)** — entries execute
`openPosition` calldata and book fills only from the mined `EntryExecuted`
event (realised spend, not the quote); exits execute `closePosition` and book
from `ExitExecuted`. Guards exercised end-to-end (covered by integration tests
against a real anvil): `maxSpend`, `minTokensOut`, `dailyBudget`/`totalBudget`,
block deadline, max base fee, balance deltas, partial exits, honeypot/failed-
sell reverts. A reverted simulated trade moves nothing and is persisted with
its revert reason.

**API shape (A.3)** — `GET /api/sniper/mode` returns the work-order payload:
`atomicMode`, `sniperMode`, `sniperLiveBootEnabled`, `canSwitchLive`,
`blockers[]`, `simulationVaultAddress`, `productionVaultAddress`,
`simulationBalanceWei`, `simulationChainId`, `activeVault{kind,address}`,
`fixture{...}`. `GET /api/sniper/sim-fixture` reports status;
`POST /api/sniper/sim-fixture/init` deploys/verifies (authenticated).

### Work Package B — Independent Sniper Mode ✅

**`bot/crates/mev-bot/src/sniper/mode.rs` (new)** — `SniperMode`
(`Simulation | Live`) + `SniperModeBoot` (`SNIPER_MODE`, default
`simulation`; `SNIPER_LIVE_ENABLED`, default `false`). The mode is never
derived from `/api/mode` or `LIVE_EXECUTION`; fresh checkouts boot in
simulation.

- `GET/POST /api/sniper/mode` (POST behind the bearer gate).
- `POST {"mode":"live"}` fails closed (409 + exact blockers) unless: boot
  ceiling, production vault configured, `SNIPER_SEARCHER_PRIVATE_KEY` set,
  non-zero size/budget, lane not halted.
- `POST {"mode":"simulation"}` always succeeds and immediately stops new live
  entries. Open **live positions stay tagged `live`** and keep live exit
  management (routing is per-position by provenance, not by lane mode).
- Boot validation fails closed on bad combinations
  (`SNIPER_MODE=live` without the ceiling/key).

**Frontend** — `SniperPanel` renders a real `SNIPER MODE` segmented control
(SIMULATION/LIVE buttons with `aria-pressed`, disabled states, keyboard focus)
with the atomic engine mode shown only as context. Switching to live opens a
confirmation dialog listing chain, production vault, connected owner wallet,
bot searcher address, daily/total budgets, and the full-loss warning; blockers
from the bot render inline and the button is disabled until they clear.
Switching back to simulation explains that live positions remain live data.

### Work Package C — Two Ledgers, One Domain Model ✅

**Schema (additive migrations + backfill)**
- `sniper_positions`: `execution_mode`, `settlement`, `tx_status`, `exit_tx`
- `sniper_fills`: `execution_mode`
- `sniper_simulation_state(id=1)`: durable paper bankroll
  (`balance_wei`, `reset_at_ms`, `updated_at_ms`)

Pre-provenance rows default live-shaped and are backfilled from their
`SIMULATION%` notes / `reason='simulation'` fills.

**Behavior**
- Paper bankroll starts at exactly 1 ETH, survives restarts (hydrated from
  `sniper_simulation_state`), resets only via the explicit authenticated
  endpoint (refused while the lane is live; history preserved, reset
  timestamped).
- Entries reserve before executing and refund on revert/short-fill; exits
  credit the exact realised proceeds. Failed simulated trades never touch the
  bankroll and persist the revert reason.
- `GET /api/sniper/portfolio` adds `totalsByMode{simulation,live}` and every
  row carries `executionMode`/`settlement`/`txStatus`.
- **Live exit accounting upgraded to receipts** (work order §9.7): exits book
  optimistically on submission, then per-block reconciliation replaces the
  estimate with the receipt's exact `ExitExecuted` values or rolls the fill
  back entirely on a revert (`correct_last_sell_fill` /
  `delete_last_sell_fill`).

**Console** — portfolio ledger switcher `[Simulation] [Live] [All]`,
per-mode totals, source badges (`SIMULATION` / `LIVE VAULT` / `CONNECTED
WALLET`) on every row, and simulation rows never link to a chain explorer.

### Work Package D — Wizard ✅

`SniperVaultWizard` is mode-aware:

- **Simulation**: no injected wallet needed, step 2 shows “Simulation vault
  ready · local Anvil fixture · no deployment required”, an
  “Initialize simulation fixture / Test simulation vault” action calls the
  bot, and the fixture's WETH/budget/paper bankroll are displayed. Production
  `setSearcher`/funding controls are not offered in simulation.
- **Live**: unchanged 4-step flow with hard blockers rendered per the state
  matrix (no vault / wrong chain / wrong WETH / searcher not authorized).
- Step 3 renamed **“Authorize bot searcher EOA”** with the work-order copy and
  tooltip. All three identities are always displayed: connected
  owner/deployer wallet, `SNIPER_SEARCHER_ADDRESS`, `SNIPER_VAULT_ADDRESS`,
  plus “Simulation vault · local Anvil only” vs “Production vault · selected
  chain” labels.

### Work Package E — Trade Terminal ✅

- `SIMULATION`/`LIVE` banner: simulation trades route through the bot's
  fixture path (`/api/sniper/trade`) and report the local fixture tx id — no
  explorer links; live shows the chain and signer source (connected wallet /
  bot signer / fee router).
- Fee copy is honest: “modeled in paper accounting (no treasury transfer)” in
  simulation; the atomic fee router remains mandatory when a treasury is
  configured in live.
- MEV-Safe stays explicitly unavailable until a private relay path exists.

### Bug fixes surfaced by the implementation

1. **Reserve re-ordering bug** (`process_launch`): candidates carry WETH-side
   reserves by construction; the address-sort swap quoted against the wrong
   side of the curve whenever the token sorts below WETH. Fixed.
2. **Exit sizing K-invariant bug** (live + sim): optimistic swap outputs used
   the spot mark instead of the constant-product amount, which a V2 pair's K
   check rejects. Both paths now size exits with `marks::v2_amount_out`
   (fallback to mark when reserves are unreadable; `minWethOut` still guards
   realised proceeds).
3. **Live exits booked on mempool acceptance**: replaced with receipt
   reconciliation (see Package C).

---

## Verification performed

Required gates (all green):

```
cd bot && cargo fmt --all -- --check && cargo test --all      # 430 passed
cd bot && cargo clippy --all-targets -- -D warnings           # 0 findings
cd contracts && forge fmt --check && forge test && forge build --sizes  # 86 passed
node contracts/script/compile-check.js                        # MevExecutor 11,497 bytes (invariant)
cd frontend && npm run typecheck && npm run build             # clean
```

New automated tests:

- **Anvil integration suite** (`sniper/sim_vault.rs`, runs wherever `anvil`
  is on PATH, skips gracefully otherwise): deterministic identities distinct
  from production keys; fixture deploys real vault with chain-WETH binding,
  allowlisted searcher, funded balance; contract-backed open/close books exact
  event values; budget/K-invariant/deadline guards revert with named reasons
  and move nothing; honeypot blocks the exit.
- **Rust unit tests**: mode independence from the atomic engine, fail-closed
  live switching, live positions survive a live→simulation switch, paper
  ledger exactness/persistence/reset, per-mode totals never bleed, exit
  receipt reconciliation primitives, fixture revert decoding.
- **Foundry**: `SniperVaultSimFixture.t.sol` (constructor/WETH binding per
  chain, `EntryExecuted`/`ExitExecuted` event values, budget/slippage
  reverts, partial exits, honeypot, reentrancy) and fee-router reentrancy /
  failed-router / rounding coverage.

Live end-to-end smoke (local chain + fork, this sandbox):

1. Bot boots, fork spawns, fixture attaches (chain WETH bound).
2. `POST /api/sniper/mode {"mode":"live"}` → 409 with the exact blockers
   (no ceiling, no vault, no key, no budgets).
3. Fixture init deploys the real SniperVault (CREATE-collision retry verified
   against an upstream contract at the same deterministic address).
4. Envelope armed **without a production vault** (simulation needs none).
5. Contract-backed sim BUY 0.05 ETH → mined fixture tx, exact `EntryExecuted`
   values, bankroll 1.0 → 0.95 ETH.
6. Contract-backed sim SELL 100% → mined fixture tx, exact `ExitExecuted`
   proceeds (0.04970193 ETH = two 0.3% fees), position closed, bankroll
   credited; fills persist with `executionMode=simulation` + fixture tx
   hashes.

## Not included / known gaps

- **Frontend component tests**: the repo has no frontend test framework; the
  enforced frontend gates are `typecheck` + `build` (both pass). Decision
  logic that can be pure (source badges, mode payload types) lives in
  `lib/types.ts` for future extraction.
- **Base ingestion** is runtime configuration (`CHAIN_ID=8453` +
  `BASE_HTTP_URL`/`BASE_WS_URL`, `CHAIN_BLOCK_INGEST` defaults on for Base) —
  see the new runbook section in `docs/DAY0_RUNBOOK.md`. No code change was
  needed; the simulation fixture binds Base WETH from the chain profile.
- Base WETH fixture funding uses the wrap/transfer path (portable); the
  MockWETH mint path is reserved for bare-anvil tests.

## Files changed

```
bot/crates/mev-bot/src/sniper/mode.rs            (new)  independent mode model
bot/crates/mev-bot/src/sniper/sim_vault.rs       (new)  fixture + integration tests
bot/crates/mev-bot/src/sniper/mod.rs                    lane mode state + switch gates
bot/crates/mev-bot/src/sniper/execution.rs              contract-backed sim paths,
                                                        live exit receipt reconciliation,
                                                        reserve/exit sizing fixes
bot/crates/mev-bot/src/sniper/position.rs               provenance fields + Exit tx
bot/crates/mev-bot/src/sniper/portfolio.rs              totalsByMode + row provenance
bot/crates/mev-bot/src/sniper/marks.rs                  pair reserves + v2_amount_out
bot/crates/mev-bot/src/sniper/params.rs                 vault gate moved to live-only
bot/crates/mev-bot/src/store.rs                         migrations, sim state ledger,
                                                        fill reconciliation helpers
bot/crates/mev-bot/src/api.rs                           /api/sniper/mode, sim-fixture,
                                                        extended payloads
bot/crates/mev-bot/src/config.rs                        SNIPER_MODE / SNIPER_LIVE_ENABLED
bot/crates/mev-bot/src/engine.rs                        fixture wiring, boot envelope
bot/crates/mev-bot/src/sim/anvil.rs                     shared fork lock for the fixture
bot/crates/mev-bot/artifacts/*                          SniperVault + mock artifacts
contracts/src/                                          (unchanged — 11,497-byte invariant)
contracts/test/mocks/SimV2Pair.sol               (new)  faithful V2 flash-swap mock
contracts/test/mocks/MockERC20.sol                      honeypot blocking toggle
contracts/test/SniperVaultSimFixture.t.sol       (new)  fixture semantics coverage
contracts/test/JerseyMikesFeeRouter.t.sol               reentrancy/failure/rounding
contracts/script/compile-check.js                       artifact emission for fixture
frontend/components/SniperPanel.tsx                     SNIPER MODE control, ledger
                                                        switcher, source badges
frontend/components/SniperVaultWizard.tsx               simulation flow + terminology
frontend/components/TradeTerminal.tsx                   mode-aware trading
frontend/lib/types.ts, lib/demo.ts                      types + demo payloads
frontend/app/api/bot/[...path]/route.ts                 mode/fixture proxying
docs/SNIPER.md, docs/DAY0_RUNBOOK.md, .env.example      operator docs
```
