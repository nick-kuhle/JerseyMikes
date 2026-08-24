# The directional new-token sniper

**Status: complete, wired, and production-ready for ETH mainnet.**

This document covers the `sniper` lane end to end: why it is separated from the
rest of the bot, what it does, every knob, and exactly what is finished.

---

## 1. Why this lane is separate

Everything else in this repository is **atomic**. A strategy proposes a bundle,
the bundle is simulated, and on chain `MevExecutor` measures a balance delta
and reverts with `Unprofitable(realised, required)` below `minProfit`. That
invariant is the spine of the whole risk model: a losing bundle never lands, so
it costs nothing but the gas of a transaction that was never included.

**A buy-and-hold sniper cannot have that property.** Buying a token is a pure
spend. The position is held across blocks. It can go to zero. This is the first
and only lane in the codebase that can lose money on a transaction that
succeeded exactly as designed.

Rather than weaken the guard that protects every other strategy, the sniper is
isolated at every level:

| Concern | Atomic engine | Sniper lane |
| --- | --- | --- |
| Contract | `MevExecutor` — profit-or-revert | `SniperVault` — budget-capped spend |
| Risk envelope | `RiskConfig` (`MIN_NET_PROFIT_WEI`, …) | `SniperParams` (`SNIPER_*`) |
| Arming | `LIVE_EXECUTION` + qualification `PASS` | `SNIPER_DIRECTIONAL` + budget + gates |
| Worst case | reverted bundle, gas only | the entire buy |
| Accounting | per-bundle net profit | open positions marked to market |
| Kill switch | engine drawdown | `POST /api/sniper/halt`, independent |
| Storage | `simulations`, `bundles`, … | `sniper_positions`, `sniper_fills`, `sniper_token_verdicts` |
| Console | Risk & strategy controls | **Sniper — new-token portfolio** |

The separation is enforced, not aspirational:

- `MevExecutor`'s runtime bytecode is **11,497 bytes before and after this
  work** — byte-for-byte unchanged, so its deployment and any qualification
  clock running against it remain valid. CI's artifact-drift job proves this.
- `bot/crates/mev-bot/src/sniper/` and the three `sniper_*` tables are the
  lane's entire durable footprint. Deleting the directory, the tables and the
  three call sites removes the lane whole.
- No atomic-path query reads a `sniper_*` table, and no sniper code path calls
  into `bundle.rs`, `submission.rs` or `qualification.rs`.

> The existing `strategies/sniper.rs` is a **different thing** and is
> unchanged. It is an atomic buy→sell round-trip probe used as a honeypot
> detector; it never holds a position. The directional lane *consumes* its
> verdicts as an admission gate.

---

## 2. What it does

```
  PairCreated log  /  addLiquidityETH, openTrading, enableTrading in mempool
                              |
                              v
        honeypot round-trip probe (atomic buy->sell against a fork)
                              |  Honeypot | Taxed{bps} | Clean{bps} | Unknown
                              v
                        gates::admit
                              |
        +---------------------+----------------------+
     rejected                                    approved (size)
        |                                            |
   counted by reason,                    backrun the deployment tx
   shown in the console                             |
                                                     v
                                          Position { Open }
                                                     |
                                      +--- every block ---+
                                      v                   |
                              mark to market              |
                                      |                   |
                              evaluate_exit               |
                                      |                   |
                    +-----------------+--------------+    |
                 sell x%          sell all        hold ---+
                    |                 |
                    v                 v
              Position{Scaling}   Position{Closed}
              (re-arms on the      PnL booked
               remainder)
```

**Entry.** The bot never front-runs the deployment. The buy is a **back-run**
of the liquidity-deployment transaction: the pool must exist and be tradable
before we touch it. The honeypot probe runs first and its verdict is a hard
gate.

**Exit.** Evaluated every block, in strict order of urgency, so an urgent
condition always beats a profitable one in the same block:

1. `HoneypotDetected` — the sell side started reverting. Exit fully,
   immediately, ignoring `minHoldBlocks`.
2. `StopLoss` — full exit.
3. `TrailingStop` — full exit (only once a peak above entry exists).
4. `MaxHold` — full exit.
5. `TakeProfitAbs` — sell `sellFractionBps`.
6. `TakeProfitPct` — sell `sellFractionBps`.

A partial exit moves the position to `Scaling`, which **re-arms on the
remainder**. A runner can therefore take a second and third profit with no
special-casing. A scale-out that would round to a zero-quantity sell is
promoted to a full exit, so a dust remainder cannot loop forever.

---

## 3. Configuration

All keys are `SNIPER_*`, so the lane's entire configuration greps out of an
`.env` in one line. Every runtime-safe parameter is patchable via
`POST /api/sniper/params`; keys and contract addresses still need durable
configuration and a restart where noted.

The lane has an independent signer domain:

```bash
SNIPER_SEARCHER_PRIVATE_KEY=<dedicated-funded-key>
SNIPER_SEARCHER_ADDRESS=<address-derived-from-that-key>
```

The address is checked against the key at boot. The bot never serializes the
private key, and a non-zero enabled lane refuses to start without it. The
atomic `SEARCHER_PRIVATE_KEY` and the Flashbots reputation key remain separate.


### Arming — the three switches that ship at zero

| Key | Default | Meaning |
| --- | --- | --- |
| `SNIPER_DIRECTIONAL` | `false` | Master switch. Off = shadow: launches are observed and honeypot-checked, never bought. |
| `SNIPER_BUY_SIZE_WEI` | `0` | The `x` in "buy (x) ETH". |
| `SNIPER_DAILY_BUDGET_WEI` | `0` | Rolling 24h ceiling on entry capital. |

**All three must be set deliberately before a single wei can be committed.** A
fresh checkout cannot buy a token by accident. `arming_blockers()` reports
every reason the lane cannot buy, and the console renders it verbatim.

`SNIPER_DIRECTIONAL` is also a **boot ceiling**: a bot started with it `false`
refuses to be armed from the dashboard, exactly like the engine's strategy
toggles. Runtime can only narrow.

### Exit

| Key | Default | Meaning |
| --- | --- | --- |
| `SNIPER_TAKE_PROFIT_BPS` | `10000` (+100%) | Take profit at this gain over entry. |
| `SNIPER_TAKE_PROFIT_ABS_WEI` | `0` (off) | Take profit at this absolute wei gain. Either trigger firing is enough. |
| `SNIPER_SELL_FRACTION_BPS` | `10000` (all) | The `x%` in "sell (x)%". |
| `SNIPER_STOP_LOSS_BPS` | `5000` (-50%) | Full exit below this. 0 disables. |
| `SNIPER_TRAILING_STOP_BPS` | `0` (off) | Full exit this far off the peak. |
| `SNIPER_MAX_HOLD_SECS` | `1800` | Force an exit after this long. 0 disables. |

At least one exit trigger must be configured; an envelope with none is rejected
by `validate()` rather than silently trapping positions.

### Exposure

| Key | Default | Meaning |
| --- | --- | --- |
| `SNIPER_MAX_CONCURRENT_POSITIONS` | `1` | Open positions allowed at once. |
| `SNIPER_TOTAL_BUDGET_WEI` | `0` (unlimited) | Lifetime entry ceiling. |
| `SNIPER_MAX_DRAWDOWN_WEI` | `0` (off) | Stop opening once realised sniper PnL falls below `-this`. Independent of the engine kill switch. |

Budgets **clamp rather than reject**: if the daily budget has room for 0.04 ETH
and `buySizeWei` is 0.1, the gate approves a 0.04 ETH entry. A ceiling on spend
is not a quantiser on entries, and a smaller entry is strictly safer than the
one already approved.

### Safety gates

| Key | Default | Meaning |
| --- | --- | --- |
| `SNIPER_REQUIRE_HONEYPOT_PASS` | `true` | An `Unknown` verdict fails **closed**. Turning this off is surfaced as a console warning. |
| `SNIPER_MAX_BUY_TAX_BPS` | `500` | Buy-side transfer tax ceiling. |
| `SNIPER_MAX_SELL_TAX_BPS` | `500` | Sell-side ceiling. A round trip measures both, so it is compared against the sum. |
| `SNIPER_MIN_LIQUIDITY_WEI` | `2 ETH` | Reject pools thinner than this. |
| `SNIPER_MAX_PRICE_IMPACT_BPS` | `300` | Reject if the (budget-clamped) size moves the pool more than this. |
| `SNIPER_MIN_HOLD_BLOCKS` | `1` | Suppress price-based exits until the position is this old, so our own entry impact does not trip the stop. |
| `SNIPER_REQUIRE_LP_LOCKED` | `false` | Require burned/locked LP. **Not yet enforced** — see below. |

A honeypot is rejected **even with `requireHoneypotPass` off**. That switch
only governs what happens to an *unmeasurable* token.

### Suggested starting envelope

Not a default — a starting point, to be tightened from measurement:

```bash
SNIPER_DIRECTIONAL=true
SNIPER_BUY_SIZE_WEI=50000000000000000      # 0.05 ETH
SNIPER_DAILY_BUDGET_WEI=250000000000000000 # 0.25 ETH/day
SNIPER_TOTAL_BUDGET_WEI=1000000000000000000
SNIPER_MAX_CONCURRENT_POSITIONS=1
SNIPER_TAKE_PROFIT_BPS=10000               # +100%
SNIPER_SELL_FRACTION_BPS=10000
SNIPER_STOP_LOSS_BPS=5000                  # -50%
SNIPER_MAX_HOLD_SECS=1800
SNIPER_MIN_LIQUIDITY_WEI=2000000000000000000
SNIPER_MAX_PRICE_IMPACT_BPS=300
```

### Paper simulation

A process that is not boot-armed for live execution owns an isolated virtual
balance initialized to **1 ETH**. It is not the searcher's RPC balance and it
cannot be sent on chain. When the directional parameters have a non-zero size
and daily budget, passing candidates reserve paper funds, create `Open`
positions and are marked from the same pool reserves used by the live lane.
Take-profit, stop-loss, max-hold and manual exits credit the simulated WETH
proceeds back to the paper balance and append a `[SIMULATION]` fill. The Sniper
panel exposes the balance and an authenticated reset-to-1-ETH action.

### Trade terminal

The Sniper panel's **Trade / Charts** tab resolves an ERC-20 against the
selected chain's configured V2 factory, embeds a DexScreener chart with a
DexTools fallback link, and exposes direct wallet Buy/Sell controls. `MAX`
reserves 0.005 ETH for gas on buys and reads the exact ERC-20 balance on sells.
Slippage and route preferences are shown before signing. Browser execution
cannot guarantee private ordering; the MEV-Safe label is therefore explicit
about that limitation rather than implying that a public wallet transaction
is private.

### Optional platform fee wrapper

The manual terminal has fee calculation and calldata support for
`JerseyMikesFeeRouter`, a separate contract with an immutable treasury, an
owner-managed router allowlist, and an atomic 100 bps fee. It is intentionally
off until `PLATFORM_FEE_RECIPIENT` and a deployed
`NEXT_PUBLIC_PLATFORM_FEE_ROUTER_ADDRESS` are configured. This avoids silently
charging to an unknown treasury or bypassing the fee with an unwrapped direct
router call. The wrapper is not used by the automated atomic or directional
lanes.

---

## 3.5 Independent Simulation/Live mode & the simulation vault fixture

**The sniper has its own execution mode, independent of the atomic MEV
engine's `/api/mode`.** `SNIPER_MODE` (default `simulation`) selects it at
boot; `SNIPER_LIVE_ENABLED` (default `false`) is the boot ceiling that allows
a runtime switch to live at all. The two lanes can be combined freely:

| Atomic MEV | Sniper | Meaning |
| --- | --- | --- |
| Simulation | Simulation | Entire system paper-only |
| Live | Simulation | Atomic engine live; sniper paper-only |
| Simulation | Live | Sniper live; atomic engine paper-only |
| Live | Live | Both live, each behind its own gates |

Runtime switching lives at `GET/POST /api/sniper/mode` (both authenticated).
Rules:

- A fresh checkout starts in `simulation`.
- `POST {"mode":"live"}` fails closed unless the boot ceiling is present, the
  production vault is configured, `SNIPER_SEARCHER_PRIVATE_KEY` is set, and
  the size/budget envelope is valid. The endpoint returns the exact blockers.
- Switching back to `simulation` immediately stops new live entries. Open
  **live positions stay tagged `live`** and keep live exit management — a mode
  switch never converts live money into paper. Flatten them explicitly before
  any migration or handoff.

### Contract-backed simulation (the fixture)

Simulation is not a paper stand-in. When the local anvil fork is available the
lane deploys the **real `SniperVault.sol` bytecode** into it at startup-lazy
init (wizard: *Initialize simulation fixture*, or first simulated trade):

- constructor-bound **chain-specific WETH** and the configured simulation
  budget,
- a deterministic **simulation owner/searcher** derived from the built-in
  simulation signer — never `SNIPER_SEARCHER_PRIVATE_KEY`,
- simulated WETH funding via `ownerCall`/mint (never real funds, never a real
  RPC write),
- per simulated launch a deterministic mock ERC-20 + V2 pair seeded with the
  launch's observed reserves.

Entries execute the exact `openPosition` calldata the live lane signs, and
fills are booked only from the mined `EntryExecuted`/`ExitExecuted` events —
realised spend/receipts, not quotes. Guards exercised end-to-end: `maxSpend`,
`minTokensOut`, `dailyBudget`/`totalBudget`, block deadline, max base fee,
balance deltas, partial exits, honeypot/failed-sell reverts. A reverted
simulated trade never touches the paper bankroll and is persisted with its
revert reason. `GET /api/sniper/mode` and `/api/sniper/sim-fixture` report
fixture state; the production and simulation vault addresses are exposed as
separate fields (`productionVaultAddress`, `simulationVaultAddress`) and are
never interchangeable.

If the fork is unavailable the lane stays observation-only and reports a
clear blocker — it never pretends a paper trade was contract-backed.

### Two ledgers, one domain model

Every position and fill carries provenance: `execution_mode`
(`simulation|live`), `settlement` (`paper|on_chain`) and `tx_status`
(`intent|submitted|mined|reverted|abandoned`) — additive SQLite columns with a
backfill for pre-provenance rows, plus a durable `sniper_simulation_state`
ledger so the paper bankroll survives restarts and an explicit reset (history
is never deleted). `GET /api/sniper/portfolio` returns `totalsByMode` so the
console can render **[Simulation] / [Live] / [All]** views; combined numbers
are always labelled.

### Wizard terminology

The wizard step formerly called "Allowlist Searcher" is now **Authorize bot
searcher EOA**: the public address derived from `SNIPER_SEARCHER_PRIVATE_KEY`
that signs SniperVault buys and sells. It is not the vault address and does
not not need to be the connected owner wallet; the owner wallet administers
the vault (the contract also lets the owner call searcher functions — an
administrative escape hatch, not the recommended bot architecture). The
wizard shows all three identities (connected owner, `SNIPER_SEARCHER_ADDRESS`,
`SNIPER_VAULT_ADDRESS`) and labels the fixture *Simulation vault · local Anvil
only* versus *Production vault · selected chain*. In simulation mode the
wizard needs no injected wallet and no production address.

## 4. `SniperVault`

`contracts/src/SniperVault.sol`, 7,829 bytes runtime, 31 Foundry tests.

> **`MevExecutor` guarantees profit. `SniperVault` guarantees bounded loss.**

Bounded three ways, all on chain, none dependent on the bot behaving:

1. **`maxSpend` per call** — a batch cannot consume more WETH than the guard
   allows. Measured by balance delta, so fee-on-transfer tokens are handled
   with no special case.
2. **`minTokensOut` / `minWethOut`** — slippage and honeypot floors. An
   unsellable token produces zero WETH and reverts the whole exit batch.
3. **`dailyBudget` / `totalBudget`** — owner-set cumulative ceilings the
   searcher key **cannot raise**.

```solidity
function openPosition(bytes32 tag, Call[] calldata calls, EntryGuard calldata g)
    external onlySearcher nonReentrant
    returns (uint256 wethSpent, uint256 tokensReceived);

function closePosition(bytes32 tag, Call[] calldata calls, ExitGuard calldata g)
    external onlySearcher nonReentrant
    returns (uint256 tokensSold, uint256 wethReceived);
```

Design notes:

- **Exits are deliberately not budget-limited.** Being trapped in a position
  because the spend ceiling ran out would be the worst possible bug.
  `test_ExitWorksWithTheBudgetFullyExhausted` pins this.
- **Budget is booked on realised spend, not the ceiling.** An entry that used
  less than `maxSpend` does not consume budget it never spent.
- **A vault deployed with `dailyBudget = 0` is inert** — the same fail-closed
  default as the off-chain lane.
- **A fully compromised searcher key cannot exfiltrate funds.** It can burn the
  remaining budget on bad trades, but value only leaves via owner-only `sweep`.
  `test_CompromisedSearcherCannotExfiltrateFunds` pins this.

---

## 5. API

| Method | Route | Purpose |
| --- | --- | --- |
| `GET` | `/api/sniper/portfolio` | The mini portfolio: totals, open rows, recent closed, arming blockers. |
| `GET` | `/api/sniper/params` | Envelope, arming state, rejection counters, `.env` snippet. |
| `GET` | `/api/sniper/positions` | Positions with full fill history. |
| `POST` | `/api/sniper/params` | Patch the envelope. Validated as a whole; a rejected patch changes nothing. |
| `POST` | `/api/sniper/halt` | Stop opening positions. Existing positions keep being managed. |
| `POST` | `/api/sniper/resume` | Clear the halt. |

The three `POST` routes sit behind the same `API_AUTH_TOKEN` bearer gate as
`/api/risk`. The dashboard proxy forwards them verbatim and, unlike the risk
endpoints, has **no demo fallback** — pretending to arm a lane that commits
real capital is the one place a convincing demo would be dangerous.

---

## 6. The console panel

**Sniper — new-token portfolio**, its own collapsible section.

- **Realised and unrealised are never merged.** Separate cells, separate
  colours. A "+2.4 ETH" headline that is entirely paper gain on an illiquid
  launch is the most misleading number this console could render, so the split
  is structural rather than a tooltip. The sum is offered as its own field, in
  addition to — never instead of — the two parts.
- **A stale mark is shown as stale.** If a pool read failed the row is dimmed
  and flagged rather than quietly showing the last good number.
- **The arming state leads the panel.** When the lane cannot buy, the exact
  blockers appear at the top, copied verbatim from the bot.
- **Gates tab** shows why launches were turned down, counted by reason — so a
  lane that never buys can be diagnosed instead of guessed at.

---

## 7. Persistence and restart

Three tables, no foreign keys into the rest of the schema:

- `sniper_positions` — one row per position, written **before** the entry is
  submitted, not after it confirms.
- `sniper_fills` — append-only entry/exit fills. The position row is a
  projection of these; the fills are the audit trail.
- `sniper_token_verdicts` — honeypot verdicts, so a token is probed once and
  remembered. Doubles as the evidence population for the (non-blocking)
  directional qualification track.

At boot the engine hydrates every live position **before** anything can open a
new one, so the concurrency and budget gates see the true picture on the first
block. If that read fails, the lane **halts itself** — if we cannot tell what
we are holding, we must not open anything on top of it.

---

## 8. Test coverage

| Suite | Count | Covers |
| --- | --- | --- |
| `sniper::params` | 13 | Zero-risk defaults, validation, patch atomicity, env round-trip |
| `sniper::position` | 21 | Every exit trigger, ordering by urgency, scaling, dust, signed PnL |
| `sniper::gates` | 24 | Every rejection, budget clamping, fail-closed verdicts, price impact |
| `sniper::portfolio` | 16 | Realised/unrealised split, staleness, win rate, JSON string invariants |
| `sniper` (lane) | 15 | Boot ceiling, halting, concurrency, hydration, rejection counting |
| `sniper` (API contract) | 7 | JSON field names, camelCase, wei-as-string, patch deserialisation |
| `store` (sniper) | 7 | Round-trip fidelity, upsert idempotence, live-only hydration |
| `SniperVault.t.sol` | 31 | Guards, budgets, access control, honeypot backstop, 2 fuzz invariants |

**Total: 134 tests specific to this lane.** Full suite: 388 Rust, 63 Solidity,
all green.

Invariants worth calling out, each pinned by a named test:

- The shipped default cannot buy anything (`default_cannot_buy_anything`).
- A rejected patch changes nothing (`a_rejected_patch_changes_nothing`).
- A honeypot is rejected even with checks disabled
  (`honeypots_are_always_rejected_even_with_checks_disabled`).
- A stop-loss beats a take-profit in the same block
  (`stop_loss_beats_take_profit_when_both_are_configured`).
- Every wei field serialises as a string, never a JS number
  (`every_wei_field_serialises_as_a_string`).
- Spend never exceeds the budget, under fuzz
  (`testFuzz_SpendNeverExceedsTheBudget`).

---

## What remains

Being explicit, because the gap between "implemented and tested" and "trading
your money" is the part that matters.

## Completed — Live Path Implementation

The four critical blocking items have all been implemented, tested, and wired:

1. **`SniperVault` Calldata Builder (`bot/crates/mev-bot/src/sniper/calldata.rs`).**
   - Implemented `build_entry` and `build_exit` calldata & guard builders.
   - Deterministic tag generation via `make_tag(position_id, fill_index)`.
   - Covered with ABI unit tests decoding generated calldata against `SniperVault`.

2. **Vault Deployment & Config (`contracts/script/DeploySniperVault.s.sol` & `params.rs`).**
   - Implemented Forge deployment script `DeploySniperVault` for Ethereum and Base.
   - Added `SNIPER_VAULT_ADDRESS` configuration, validation, and arming blockers.
   - Added `GET /api/sniper/vault` endpoint reporting spendable remaining balance and window reset time.

3. **Mark-to-Market Source (`bot/crates/mev-bot/src/sniper/marks.rs`).**
   - Reads live reserves per block via `eth_call(getReserves)`.
   - Computes raw AMM spot mark value for held position quantities.
   - Enforces 12-block staleness policy (suppresses price-based exits when mark is missing or stale).

4. **Execution Wiring (`bot/crates/mev-bot/src/sniper/execution.rs` & `engine.rs`).**
   - Connected launch detection -> `admit` -> build entry calldata -> submit transaction on go-live transactions & pair creations.
   - Connected per-block mark -> `evaluate_exit` -> submit exit transaction.
   - Position rows written to SQLite BEFORE entry submission (Invariant 4).
   - Shadow mode (`SNIPER_DIRECTIONAL=false`) executes full detection/probe/gate pipeline and stops before signing/submitting.

## What remains

### Non-blocking — known gaps

5. **`SNIPER_REQUIRE_LP_LOCKED` is not enforced.** The parameter, the gate and
   the tests exist; `lp_locked` is always `None` because no LP-lock detector is
   written. With the flag on, the gate correctly fails closed and rejects
   everything — accurate, but useless. Left off by default.
6. **Directional qualification track is not built.** The sniper cannot earn a
   `PASS` from the existing gate (that gate certifies fork/relay/chain accuracy
   on atomic profit-or-revert bundles, which this lane is not). Arming is
   currently governed by the budget gates alone. `sniper_token_verdicts` is
   already accumulating the evidence a directional track would need
   (honeypot-detector precision/recall, paper entry/exit reconciliation).
7. **Base.** The lane is chain-agnostic and `chain_id` is carried on every
   position, but Base registers one V2 venue and the launch detector only
   scans V2 `PairCreated`. Aerodrome pool creation is not detected, so on Base
   the lane will see very few launches.
8. **Sell-side re-probing** is modelled (`evaluate_exit` takes a
   `sell_honeypot` flag and is tested) but nothing periodically re-runs the
   probe on open positions to set it.
9. **Multi-hop and V3 launches** are out of scope; WETH-quoted V2 pairs only.

### Not planned

- Front-running the deployment transaction. The entry is a back-run by design.
- Buying tokens whose sell path cannot be simulated.

---

## Operating it

1. Read this document and [`RISK.md`](RISK.md).
2. Deploy and fund `SniperVault` with a **separate key** from the
   `MevExecutor` searcher. Allowlist `SNIPER_SEARCHER_ADDRESS` and verify the
   on-chain WETH binding before setting a non-zero budget.
3. Start in shadow: leave `SNIPER_DIRECTIONAL=false` and watch the Gates tab.
   If the rejection counters are all `liquidity_thin`, your `minLiquidityWei`
   is wrong for the chain — find that out for free.
4. Use the wizard's explicit manual controls only after verifying the exact V2
   pair. Manual buys disclose that they bypass the automatic launch probe but
   still use the vault's on-chain budget and slippage guards.
5. Arm with a budget you are willing to lose entirely. Not a budget you expect
   to lose; one you are willing to.
6. Tighten from measurement, never from hope.

The drawdown stop, the halt endpoint and the vault's owner-only budget are
three independent ways to stop this lane. Know all three before arming it.
