# Roadmap

Chains are added one at a time; each one has to survive a week of simulation
before the next is started.

## Where the project is

**Development finishes before the soak begins.** These are two different
activities owned by two different groups, and they do not overlap:

| | Owner | Question it answers | Exit condition |
| --- | --- | --- | --- |
| **Build** | engineering | Is the system correct, complete and production-grade? | Every CI gate green and blocking; no unimplemented path on the live route; all four operator controls implemented; docs current |
| **Soak** | operators + testers | Does this correct system actually make money on this chain, safely, over time? | `GET /api/qualification` reports `PASS` per strategy from 7 days of continuous canonical evidence |

The soak is **not** a development phase with a longer feedback loop, and it is
not where remaining engineering work gets discovered. It is a measurement
period run by operators against a finished binary. If a soak turns up a code
defect, that is a build-phase escape — the soak stops, the fix ships through
CI, and the soak clock **restarts from zero** (`DAY0_RUNBOOK.md` Phase 4).
Qualification evidence is only meaningful about the exact build that produced
it.

Practically, that means engineering hands operators: a tagged release that is
green on all four CI jobs, `mev-bot doctor` passing on the target host, the
deployment units in `deploy/`, and a runbook they can execute without an
engineer in the room. Operators hand engineering back: qualification verdicts,
funnel readings, and alert history.

**Current state:** the build phase is complete for Ethereum mainnet and for the
Base safety foundation. Phase 0–3 are shipped; the remaining unchecked boxes
below are either explicitly out of scope for this phase or are *decisions to be
written down* rather than code to be authored. The two open Base items are
tracked in [`BASE_REVENUE_PATH_WORK_ORDER.md`](BASE_REVENUE_PATH_WORK_ORDER.md).
Nothing on the live path is stubbed, and no strategy can broadcast without
independently earning its own `PASS`.

## Phase 0 — this PR

- [x] `MevExecutor`: atomic batches, profit guard, Balancer flash loans, V3 mint
      callback, coinbase bribe, searcher allowlist
- [x] Rust engine: ingest → strategies → risk → simulation → SQLite → REST/SSE
- [x] Five strategies: sandwich, JIT, atomic arb, Aave V3 liquidation, sniper
- [x] Dual simulation: local anvil fork + relay `eth_callBundle`
- [x] Next.js console: P/L, equity curve, transaction history, live feeds,
      contract control panel
- [x] Simulation-only enforcement with a two-key live switch

## Phase 1 — make the numbers trustworthy (Ethereum)

- [x] Replay harness: re-simulate historical blocks from the database and
      compare against what actually landed on chain (from relay bid traces)
      — `mev-bot replay` plus online reconciliation on every new head /
      delivered block (`bot/crates/mev-bot/src/replay.rs`)
- [x] Model competition: rank our bundle against the block's realised builder
      payment to estimate true inclusion probability
      (`competition.rs`; logistic of bribe / winning bid, persisted on
      `reconciliations`)
- [x] Latency budget: per-stage timing histograms; the mempool→bundle path
      needs to be under ~150 ms to matter (`latency.rs`, `/api/latency`)
- [x] Nonce/inventory manager (currently the bundle nonce is a placeholder)
      — searcher nonce is read from chain each block and used to sign both
      legs; ETH/WETH balances tracked; gating opt-in via `INVENTORY_GATE`
      (`inventory.rs`)
- [x] Re-org handling and per-block reconciliation
      — parent-hash mismatch / rewind marks simulations non-canonical and
      drops them from P/L; each confirmed block is ranked against relay
      traces

## Phase 2 — coverage

Scoped and ticketed in [`PHASE_2_HANDOFF.md`](PHASE_2_HANDOFF.md), which is the
source of truth for this phase (workstreams, budgets, acceptance criteria) and
is deleted once the phase ships.

The boxes below track implementation progress, not completion of the full Phase
2 Definition of Done. A checked item has code in the branch and local build/test
verification where available; the funnel-week and remote-CI gates remain called
out in the handoff.

- [x] W0: CI enabled and required on pull requests. All four jobs
      (`bot (rust)`, `contracts (foundry)`, `frontend (next.js)`,
      `embedded bytecode is current`) are green and marked required on
      PRs to `main`.
- [x] W1: Funnel counters distinguish per-invocation and per-opportunity units,
      with live/replay provenance lanes and dashboard labels.
- [x] W2: V2 discovery has retry-safe cursor/seen handling, bounded overlapping
      scans, shared factory-log decoding, and network-free tests.
- [x] W3: UniswapV3 `PoolCreated` discovery has a separate V3 metadata cache.
- [x] W4 implementation: direct multi-leg V2 cycle enumeration is wired into
      `on_block` with the documented budgets.

Remaining Phase 2 coverage:

- [x] W5 implementation: V3 sandwich sizing via QuoterV2. Victim-revert
      trap, 12-call / 4-candidate budgets, router-routed legs, separate
      `sandwich_v3` funnel row.
- [x] W6 implementation: UniversalRouter `execute` decoder
      (`V2_SWAP_EXACT_IN` / `V3_SWAP_EXACT_IN`) behind
      `DECODE_UNIVERSAL_ROUTER=false`. 1inch v6, 0x v2, and CoW Swap
      decoders remain out of scope.
- [x] Raise W4 to 3 legs after the funnel week (`ARB_MAX_CYCLE_LEN=3`).
      Leave at 3 until live `atomic_arb.candidatesEmitted` on the same feed
      is compared against the 2-leg baseline; only then consider 4–5.
- [x] Turn W5 on after the funnel week (`STRATEGY_SANDWICH_V3=true` with
      `POOL_DISCOVERY_V3=true`). Watch live `sandwich_v3.candidatesEmitted`
      / `submittable` and `/api/latency` stage `strategy` p95. Revert the
      pair if the pending-path p95 blows the 150 ms budget or the provider
      rate-limits.
- [x] **W6 decision: closed, not flipped.** `DECODE_UNIVERSAL_ROUTER` stays
      `false` and 1inch/0x/CoW stay out of scope. The gate was "flip only if
      the funnel shows a public-mempool gap worth decoding"; the answer is
      that the gap is structural, not a decoding shortfall. Public mempool is
      now roughly a fifth of DeFi flow and falling — private-orderflow
      endpoints (Flashbots Protect, MEV-Blocker, CoW, UniswapX) are standard,
      and a majority of block value arrives privately. A better public
      decoder competes for a shrinking, adversarially-priced residue. The
      decoder stays in the tree behind its flag so the decision is reversible
      if the funnel ever contradicts it. Rationale recorded in
      [`W6_MEMO.md`](W6_MEMO.md).
- [ ] Curve / Balancer / Maverick pool math (out of scope for this phase)
- [x] Compound V3, Morpho and Maker liquidations; oracle-update front-running
      — `liquidation_compound` (absorb + discounted `buyCollateral`),
      `liquidation_morpho` (v1.1 `liquidate`, full close, share-exact debt
      math), `liquidation_maker` (bark + atomic clip `take`, deterministic
      `kicks + 1` auction id), and `oracle_frontrun` (Chainlink
      `transmit` / Maker OSM `poke` back-runs built from the shared
      near-miss leads registry). See `docs/STRATEGIES.md` §4b–4e; selectors
      verified against the live Comet implementation and Morpho bytecode.
- [ ] MEV-Share backrun bidding (`mev_sendBundle` with privacy hints)
      (out of scope for this phase)

**Status note (2026-08-22):** Phase 3 ops landed (alerting rules + webhook,
`/api/metrics`, systemd/Docker units — `DEPLOYMENT.md`), the console tunes
the risk envelope at runtime and walks the go-live checklist, and the
simulator now decodes revert reasons (incl. `CallFailed` inner data) and
funds the fixture executor with WETH — **that fix reset the
sandwich/sniper/JIT funnel baseline**, so read those rows from that merge
forward when judging the W6/W4 gates. What remains of Phase 2 is decisions,
not code: fill `W6_MEMO.md` from the funnel panel's gap card and flip or
close W6; compare 3-leg `candidatesEmitted` against the 2-leg baseline for
W4. `PHASE_2_HANDOFF.md` now carries a closeout note and gets deleted once
those two decisions are written down.

**Status note (2026-08-21):** Liquidation coverage landed: the four new
strategies default on with the rest (the first run's job is still to measure
what is reachable); watch each row's `candidatesEmitted` against the added
per-block `eth_getLogs` + batched sweeps, and cap via
`LIQUIDATION_WATCH_CAP` / `MORPHO_MARKET_CAP` / `MAKER_ILKS` if the provider
pushes back. Earlier: funnels are split by lane; W0 CI is required on PRs to
`main`.

**Earlier status note (2026-08-21):** Funnel-week gates for W4 and W5 are flipped.
`ARB_MAX_CYCLE_LEN` defaults to 3; `STRATEGY_SANDWICH_V3` and
`POOL_DISCOVERY_V3` default on as a pair. W6 (`DECODE_UNIVERSAL_ROUTER`)
stays off until a written public-mempool gap memo exists — if
`pendingSeen` is thin the flow is already in Flashbots Protect /
MEV-Blocker / CoW / UniswapX and a public decoder will not help. The
funnel panel now renders that exact go/no-go reading from live data and
[`W6_MEMO.md`](W6_MEMO.md) is the decision record to fill. W0 is
required on PRs to `main`. Phase 2 is **not** closed: W6 is still open,
W4 is not raised past 3, and the handoff stays until those reports exist.
Details in [`docs/BUILD_NOTES.md`](BUILD_NOTES.md).

## Phase 3 — production execution readiness

- [x] Three independent operator controls: boot arming, broadcast capability,
      and authenticated runtime mode.
- [x] Per-strategy `PASS` / `FAIL` / `INSUFFICIENT SAMPLE` qualification from
      continuous canonical coverage, fork/relay samples, corresponding actual
      on-chain outcomes, and explicit accuracy tolerances.
- [x] Separate funded transaction signer and relay reputation signer.
- [x] Concurrent multi-relay `eth_sendBundle`, bounded same-UUID retry, relay
      response persistence, and cancellation.
- [x] Serialized durable nonce reservations, startup cancellation recovery, and
      fail-closed reuse blocking through target expiry.
- [x] Finality-aware exact own execution reconciliation, explicit partial and
      incoherent inclusion states, reorg invalidation, API and console views.
- [x] Confidence/completeness-scored competitor attribution without claiming
      unknowable off-chain or inventory economics.
- [x] Hardened Docker Compose and systemd deployment targets, metrics, health,
      alerts, and a reconciled runbook.
- [x] Non-native profit-token valuation (`valuation.rs`): a bundle whose profit
      arrives as a token rather than ETH is priced in native terms at the
      pinned pre-bundle fork block (V3 QuoterV2 over the four canonical fee
      tiers → V2 reserves → fail closed), with a configurable haircut
      (`VALUATION_HAIRCUT_BPS`). This is what makes a liquidation biddable at
      all; before it, every non-native profit settled as zero.
- [x] The four liquidation rows (Aave, Compound V3, Morpho Blue, Maker)
      promoted from shadow-only to live candidates now that their profit can
      be valued. Promotion is eligibility, not approval — each still has to
      earn its own `PASS`.
- [x] Flashblocks ingest: `eth_subscribe ["newFlashblocks"]` over raw
      WebSocket (`FLASHBLOCKS_WS_URL`), a raw-transaction RLP decoder with
      ECDSA sender recovery, and a diff parser that skips what it cannot
      decode. 200 ms preconfirmations against 2 s blocks, and the only Base
      feed carrying raw signed bytes a bundle can transport.
- [x] Console surfaces strategy eligibility. `GET /api/config` already carried
      `strategyEligibility`; the console now renders it beneath the
      qualification report, so a shadow-only row shows its reason instead of
      sitting at `PENDING` and looking like it merely needs more soak time.
      Uncertified simulation results are also distinguished from ordinary
      misses in the simulations table.
- [x] Set `Opportunity.profit_token` at the liquidation construction sites.
      All four protocols carry their settlement asset (Aave `debt_asset`,
      Compound V3 USDC, Morpho Blue `loanToken`, Maker DAI) into the simulator,
      so the valuation path runs end to end and the liquidation rows can
      produce real qualification evidence. A regression test in
      `strategies/leads.rs` pins the invariant.
- [ ] Value non-native profit in the relay and stub simulation backends.
      `sim/relay.rs:93` and `sim/mod.rs:196` return `net_profit_wei = 0`
      unconditionally. The fork backend — the only one the broadcast gate
      reads — is unaffected, so this limits cross-backend comparison rather
      than live safety.
- [ ] Automated inventory top-up/profit sweeping policy (manual operator action
      remains safer for the first production period).
- [ ] Bundle merging across independent opportunities.

See `SIM_TO_LIVE.md` for the implemented broadcast predicate and operating
procedure. Completion of code does not make a strategy qualified; only its own
live evidence can produce `PASS` — which is precisely why the soak is an
operator exercise against a finished build rather than the tail end of
development.

## Phase 4 — more chains

Order chosen by how much of the existing code carries over:

1. **Base** — no public mempool; sequencer feed + backrun-only strategies.
2. **Arbitrum** — sequencer feed, express-lane auction awareness.
3. **BNB Chain** — public mempool, very sandwich-heavy, needs its own DEX set.
4. **Solana** — different execution model entirely; separate engine, shared
   dashboard.
