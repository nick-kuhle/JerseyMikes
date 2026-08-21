# Roadmap

Chains are added one at a time; each one has to survive a week of simulation
before the next is started.

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

- [x] W0: CI enabled and **required on pull requests to `main`**. All four
      jobs (`bot (rust)`, `contracts (foundry)`, `frontend (next.js)`,
      `embedded bytecode is current`) green; since 2026-08-21 the fmt steps
      are required inside the workflow, the tree is fmt-clean and
      clippy-clean, the Rust side is locally verified in the authoring
      sandbox, and the `require-ci-on-main` branch ruleset enforces all
      four checks on `main` (plus no deletes / force-pushes). W0 is
      complete.
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
- [ ] Turn W6 on only if the funnel shows a public-mempool gap, and report
      what it did after a week. If the answer is "nothing", 1inch/0x stay
      closed.
- [ ] Curve / Balancer / Maverick pool math (out of scope for this phase)
- [ ] Compound V3, Morpho and Maker liquidations; oracle-update front-running
      (out of scope for this phase)
- [ ] MEV-Share backrun bidding (`mev_sendBundle` with privacy hints)
      (out of scope for this phase)

**Status note (2026-08-21):** Funnel-week gates for W4 and W5 are flipped.
`ARB_MAX_CYCLE_LEN` defaults to 3; `STRATEGY_SANDWICH_V3` and
`POOL_DISCOVERY_V3` default on as a pair. W6 (`DECODE_UNIVERSAL_ROUTER`)
stays off until a written public-mempool gap memo exists — if
`pendingSeen` is thin the flow is already in Flashbots Protect /
MEV-Blocker / CoW / UniswapX and a public decoder will not help. The
funnel panel renders that exact go/no-go reading from live data and
[`W6_MEMO.md`](W6_MEMO.md) is the decision record to fill. Since the last
note, CI's fmt steps became required (the tree is fmt-clean and
clippy-clean, with the Rust side now compiling and passing 146/146 tests
inside the authoring sandbox — see
[`docs/BUILD_NOTES.md`](BUILD_NOTES.md)), and the `require-ci-on-main`
ruleset makes the four checks required on PRs to `main` — **W0 is
closed**. Phase 2 is **not** closed: the W6 decision is open, W4 stays at
3 until its delta is written down, and the handoff stays until those
reports exist.

## Phase 3 — going live (opt-in, separate PR)

- [x] Console control surface: runtime simulation ⇄ live switch
      (`GET/POST /api/mode`, `engine.rs::LiveMode`) layered strictly on top of
      the boot-time two-key arming — an unarmed process refuses the switch
      with the restart instructions; an armed one can pause/resume without a
      restart. See `docs/RISK.md`.
- [ ] Multi-relay submission with per-relay inclusion stats
- [ ] Bundle merging and cancellation
- [ ] Hot-wallet inventory management, WETH top-ups, profit sweeping
- [ ] Alerting: kill-switch trips, endpoint failures, inclusion-rate collapse
- [ ] Deployment: systemd/docker units, metrics endpoint, log shipping

## Phase 4 — more chains

Order chosen by how much of the existing code carries over:

1. **Base** — no public mempool; sequencer feed + backrun-only strategies.
2. **Arbitrum** — sequencer feed, express-lane auction awareness.
3. **BNB Chain** — public mempool, very sandwich-heavy, needs its own DEX set.
4. **Solana** — different execution model entirely; separate engine, shared
   dashboard.
