# Phase 2 — Engineering Work Order

**Audience:** the dev teams picking up Phase 2 of JerseyMikes.
**What this is:** the open work order for Phase 2. It consolidates the former
`PHASE_2_DESIGN.md` and `PHASE_2_REVIEW.md` (recoverable from git history at
commit `e62bbc2`). Completed workstreams have been **moved to the log** in
[`BUILD_NOTES.md`](BUILD_NOTES.md) ("Phase 2 work log"); this file keeps the
rules, the open tickets, the standing conditions on shipped work, and the
Definition of Done. The pre-cleanup document (full W0–W5 ticket specs and
history) is recoverable from git history at the commit preceding that cleanup.
**Lifecycle:** this document is **temporary**. Delete it when the Definition of
Done in §5 is met — see §6, *Retiring this document*.
**Companion:** [`MAINTAINING.md`](MAINTAINING.md) is the permanent guide to how
this codebase thinks. It is not superseded by this document and does not get
deleted. Read its §1 (Mindset), §3 (Common Change Patterns) and §5 (Footguns)
before writing code; this work order references specific sections rather than
restating them.
**Scope:** everything below is simulation-only. Nothing in Phase 2 touches the
live-execution path.

**Status (2026-08-21):** W0–W6 implementations are shipped and CI-green
(log in [`BUILD_NOTES.md`](BUILD_NOTES.md)), and W0 is fully closed —
the `require-ci-on-main` ruleset makes all four checks required on `main`.
What is open: the W6 go/no-go decision, the W4 ceiling note, and the W5
watch — all data-gated on live funnel runs, not on code. Phase 2 is
**not** closed.

---

## 0. The three rules for this phase

Longer versions of the first two are in `MAINTAINING.md` §1.

**Measure before you expand.** Phase 2 has one hard ordering rule: the funnel
must be *correct* and read for a full week before any new strategy surface
is added. Every workstream below is instrumented so its effect on the funnel
is observable. "The simulation tape looks busier" is a feeling; "multi-leg
arb added 12 candidates/block that pass risk" is data.

**Don't loosen gates to chase opportunities.** Few opportunities is the
designed steady state. If the funnel reads zero, the cause is almost never
`MIN_NET_PROFIT_WEI` — it is a quiet mempool feed, an empty pool cache, or an
RPC that doesn't support `eth_getRawTransactionByHash`.

**Nothing broadcasts.** `live_execution` stays off for all of Phase 2. Phase 1
(replay validation, competition modelling, latency budget) is the prerequisite
for going live and is explicitly *not* in scope here. Do not loosen the
two-key arming, `BRIBE_BPS`, or the profit guard in `MevExecutor.sol`.

---

## 1. What is still open

| Item | What is left | Who / how |
| --- | --- | --- |
| **W6 decision** | Go/no-go on UniversalRouter decoding, gated on a **written** public-mempool gap memo after ≥ 7 days of funnel data | measure with the funnel panel's **W6 go/no-go** card; write the memo in [`W6_MEMO.md`](W6_MEMO.md); the flip itself stays an env change (`DECODE_UNIVERSAL_ROUTER=true`) |
| **W4 ceiling** | Keep `ARB_MAX_CYCLE_LEN=3` until the live `atomic_arb.candidatesEmitted` delta at 3 (vs the 2-leg baseline) is written down; only then consider 4–5 | record the delta in the W6-style memo pattern or a PR description |
| **W5 watch** | Watch live `sandwich_v3` funnel rows and `/api/latency` stage `strategy` p95 (budget: ≤ 25 ms added on the pending path, ≤ 12 QuoterV2 `eth_call`s per candidate). Revert the `STRATEGY_SANDWICH_V3` + `POOL_DISCOVERY_V3` pair if the pending-path p95 blows the 150 ms budget or the provider rate-limits | dashboard: funnel panel (live lane) + latency panel |

Everything else in Phase 2 — funnel semantics (W1), hardened V2 discovery
(W2), V3 discovery (W3), multi-leg arb (W4 implementation), V3 sandwich (W5
implementation) and the UniversalRouter decoder (W6 implementation, shipped
off) — is done, verified, and logged in
[`BUILD_NOTES.md`](BUILD_NOTES.md).

---

## 2. Workstream summary

| ID | Workstream | Depends on | Size | Gate to start | Status |
| --- | --- | --- | --- | --- | --- |
| **W0** | Enable CI; green `cargo check` / `cargo test` / `forge test` | — | S | none | ✅ complete — all four jobs green, fmt required in-workflow, clippy at zero, checks required on `main` via the `require-ci-on-main` ruleset |
| **W1** | Fix funnel counter semantics + labels | W0 | S | none | ✅ shipped |
| **W2** | Harden V2 discovery; extract shared log decoding | W0 | M | none | ✅ shipped (incl. sniper retry fix) |
| **W3** | V3 pool discovery (`PoolCreated`) + separate V3 cache | W2 | M | none | ✅ shipped, on (`POOL_DISCOVERY_V3=true`) |
| **W4** | Multi-leg V2 atomic arb (3–5 legs) | W1, W2 | L | 1 week of funnel data | ✅ shipped; default 3 legs; ceiling open (§1) |
| **W5** | V3 sandwich sizing via QuoterV2 | W1, W3 | L | 1 week of funnel data | ✅ shipped, on; watch conditions open (§1) |
| **W6** | UniversalRouter calldata decoding | W1 | M | funnel shows a public-mempool gap | ✅ implemented, **off** — decision open (§4) |

Sizes: S ≈ 1–2 days, M ≈ 3–5 days, L ≈ 1.5–2 weeks including tests and review.

**Explicitly out of scope for Phase 2.** 1inch v6 and 0x v2 decoders (revisit
after W6 produces data); Curve/Balancer/Maverick pool math;
Compound/Morpho/Maker liquidations; MEV-Share backrun bidding; any new chain;
anything in Phase 1 or Phase 3 of `ROADMAP.md`. If a ticket starts growing
toward one of these, stop and re-scope.

**Footguns for the remaining work.** Rather than restate `MAINTAINING.md` §5:

| Ticket | Read first |
| --- | --- |
| W6 | §5 (`sol!` selector verification) |
| any new strategy variant | §3.3 (the full six-step checklist — skipping step 6, funnel integration, is how a strategy becomes unobservable) |

---

## 3. Standing conditions on the shipped workstreams

These stay binding until Phase 2 closes; they are acceptance criteria that
survived their PRs.

### W4 budgets (all enforced in `dex/graph.rs` and covered by tests)

| Budget | Value |
| --- | --- |
| Max cycle length | 5 legs (`MAX_CYCLE_LEN` const + DFS depth test) |
| Max candidates simulated per block | 32 (sort by expected profit, truncate) |
| Max enumeration wall-clock per block | 25 ms (hard `Instant` cancellation in the DFS) |
| Max pools in graph | 200 (truncate by WETH reserve, largest first) |
| Gas model | 320k base + 120k/leg beyond 2 (pre-filter only; the fork is the arbiter) |
| Extra RPC calls | **zero** (enumeration reads the cache only) |

A 5-leg cycle is ~680k gas — real money at 50+ gwei. With `BRIBE_BPS=9000` it
needs roughly 10× the gross profit of a 2-leg cycle to clear the net gate.
That is intended.

### W5 budgets

| Budget | Value |
| --- | --- |
| `eth_call` per V3 candidate | ≤ 12 (coarse grid, then refine) |
| V3 candidates evaluated per pending tx | ≤ 4, largest notional first |
| Added latency on the pending path | ≤ 25 ms p95 |

The pending-tx path is the bot's hot path: a 5 ms strategy × 200 pending
txs/block = 1 s/block on a serial executor, against a design assumption of
~50 ms/block (`MAINTAINING.md` §3.3).

---

## 4. W6 — UniversalRouter decoding (open decision)

**Status: implemented, shipped off.** `dex/calldata/universal_router.rs`
decodes `execute(commands, inputs)` and the deadline overload for
`V3_SWAP_EXACT_IN` / `V2_SWAP_EXACT_IN`, including a preceding `WRAP_ETH`.
`decode_router(tx, weth, universal)` is the single entry point sandwich and
arb consume; when `DECODE_UNIVERSAL_ROUTER` is false (the default) it is
exactly `decode_swap`. Fixture tests cover five well-formed shapes plus
malformed inputs that must return `None`.

**Gate: only after a week of funnel data shows a meaningful public-mempool
gap.** Both source documents agree on this, and the review is explicit that
1inch and 0x wait for evidence.

**Measuring the gate (shipped):** the dashboard's funnel panel renders a
**W6 go/no-go** card with exactly this reading — `pendingSeen` vs the live
lane's sandwich/JIT `invocationsEmpty` and `candidatesEmitted`, plus sample
age against the 7-day gate — and copies a pre-filled
[`W6_MEMO.md`](W6_MEMO.md). The memo is the deliverable; the card only
produces its numbers. **No toggle exists in the UI: flipping
`DECODE_UNIVERSAL_ROUTER` stays an operator env change gated on the written
memo.**

Be honest about the payoff. A large share of big-swap flow on mainnet is
deliberately *not* in the public mempool — Flashbots Protect, MEV-Blocker, CoW
batch auctions, 1inch Fusion, UniswapX (`MAINTAINING.md` §6.1). Decoding
public calldata only helps for the portion that is public, which is the
portion established searchers already compete for. Highest effort, lowest
certainty, therefore last and gated on data.

**Scope: UniversalRouter only** (`0x3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD`).
1inch v6 and 0x v2 are **out of scope**.

**Acceptance criteria for the decision**

- One week of funnel data on one feed, recorded in `W6_MEMO.md`.
- If flipped: a written report on what the funnel did after another week. If
  the answer is "nothing", that is the deliverable — revert the toggle, and
  1inch/0x stay closed.

---

## 5. Definition of done for Phase 2

Phase 2 is complete when all of these hold:

1. CI is enabled, green and **required** on PRs to `main`. ✅ (the
   `require-ci-on-main` ruleset enforces all four checks; fmt steps are
   required inside the workflow and the tree is clean).
2. The funnel reports per-opportunity and per-invocation counts distinctly,
   and the dashboard labels both.
3. Pool discovery covers V2 and V3 with disjoint caches, bounded log windows,
   retry-safe `seen` tracking, and ≥ 6 non-network tests each.
4. Multi-leg V2 arb is live in `on_block` within every budget in §3, with the
   before/after funnel delta reported.
5. V3 sandwich sizing is on within its RPC and latency budgets, with the
   victim-revert trap tested.
6. UniversalRouter decoding either shipped with a funnel report, or formally
   deferred with the funnel data that justifies deferring it.
7. `docs/STRATEGIES.md`, `docs/ROADMAP.md`, `docs/RISK.md` and
   `.env.example` reflect the shipped state. Every toggle is documented.
8. `live_execution` is still false and the two-key arming is untouched.

Items 2–5 and 7 are done (log in [`BUILD_NOTES.md`](BUILD_NOTES.md)); item 1
waits on the admin step, item 6 on the W6 decision, item 4's delta report on
the W4 ceiling note. Phase 2 is **not** closed.

---

## 6. Retiring this document

When §5 is satisfied, this file gets deleted. Before deleting it:

1. Tick the Phase 2 boxes in [`ROADMAP.md`](ROADMAP.md) and remove its pointer
   to this file.
2. Fold anything that became a **durable rule about the codebase** into
   [`MAINTAINING.md`](MAINTAINING.md) — the §3 budgets here are the obvious
   candidates. A budget that only lived in this file will be violated by the
   next person.
3. Move operator-facing behaviour into [`STRATEGIES.md`](STRATEGIES.md) and
   any new gates into [`RISK.md`](RISK.md).
4. Remove this document's row from the README documentation table.
5. `git rm docs/PHASE_2_HANDOFF.md`.

`MAINTAINING.md` is **not** deleted with it. It is the permanent guide and
outlives every phase.

---

## Appendix A — Existing API surface to reuse

Do not reimplement any of these.

| Symbol | Location | Use |
| --- | --- | --- |
| `dex::v2_amount_out` / `v2_amount_in` | `dex.rs` | exact integer V2 pricing |
| `dex::ternary_search_max` | `dex.rs` | unimodal integer optimiser |
| `dex::optimal_two_leg_arb` | `dex.rs` | reference for cycle sizing |
| `dex::optimal_sandwich_in` | `dex.rs` | V2 sandwich sizing incl. victim trap |
| `dex::fetch_v2_pool` | `dex.rs` | batched V2 pool snapshot |
| `dex::get_pair` | `dex.rs` | factory `getPair` |
| `dex::quote_v3` | `dex.rs` | QuoterV2 exact-in |
| `strategies::PoolCache` | `strategies/mod.rs` | V2 cache: `get/insert/all/pair_for/load/refresh_all` |
| `strategies::decode_swap` / `decode_router` | `strategies/mod.rs`, `dex/calldata` | router decoding (single entry point) |
| `strategies::scan_factory_logs` + `decode_pair_created` / `decode_pool_created` | `strategies/mod.rs` | shared factory-log scanning (V2 + V3) |
| `strategies::sandwich::build_leg` | `strategies/sandwich.rs` | pool-direct V2 swap calls |
| `strategies::jit::decode_v3_swap` / `V3SwapIntent` | `strategies/jit.rs` | V3 router decoding |
| `strategies::jit::V3State` | `strategies/jit.rs` | slot0 + liquidity snapshot |
| `engine::Stats::record_funnel` | `engine.rs` | the only funnel mutation path |
| `engine::LiveMode` | `engine.rs` | boot-arming + runtime mode switch (`docs/RISK.md`) |

For the procedures — adding a strategy, adding a feed event, adding a pair,
tuning risk gates — follow `MAINTAINING.md` §3. Adding a strategy in
particular has a six-step checklist there; skipping step 6 (funnel
integration) is how a new strategy becomes unobservable.

---

## Appendix B — Config knobs touched by Phase 2

| Var | Default | Notes |
| --- | --- | --- |
| `POOL_DISCOVERY` | `true` | V2 discovery per block |
| `POOL_DISCOVERY_V3` | `true` | paired with `STRATEGY_SANDWICH_V3` |
| `STRATEGY_SANDWICH_V3` | `true` | V3 sandwich; `sandwich_v3` funnel row |
| `DECODE_UNIVERSAL_ROUTER` | `false` | still off; flip only with a public-mempool gap memo |
| `MIN_NET_PROFIT_WEI` | `1` | do not move to chase funnel numbers |
| `MAX_POSITION_WEI` | `100e18` | notional gate |
| `MAX_BASE_FEE_WEI` | `500 gwei` | |
| `MAX_INFLIGHT_PER_STRATEGY` | `32` | also W4's candidate cap |
| `SIM_TIMEOUT_MS` | `2500` | backstop on spawn storms |
| `BRIBE_BPS` | `9000` | **do not change in Phase 2** — rationale in `MAINTAINING.md` §3.5 |
| `LIVE_EXECUTION` | `false` | two-key switch; out of scope |

Interpreting the funnel when a gate looks wrong: `MAINTAINING.md` §3.5.

---

## Appendix C — What Phase 2 deliberately does not answer

Carried over from the design doc's closing section, because it is still true:

- Phase 2 does not guarantee opportunities. It adds strategies; the simulator
  discovers opportunities. The count per block is a property of the market,
  competition and latency, not of this code.
- Phase 2 is not a path to live trading. The simulation-only guard stays.
  Phase 1 (replay validation, competition modelling, latency budget) is the
  prerequisite and is not addressed here.
