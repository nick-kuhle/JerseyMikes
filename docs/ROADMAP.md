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

- [ ] W0: CI enabled and required on pull requests. The maintainer reports
      `make bot-check`, `make bot-test`, and `make contracts` passing locally;
      the workflow still needs to be enabled by a GitHub account with workflow
      permission.
- [x] W1: Funnel counters distinguish per-invocation and per-opportunity units,
      with live/replay provenance lanes and dashboard labels.
- [x] W2: V2 discovery has retry-safe cursor/seen handling, bounded overlapping
      scans, shared factory-log decoding, and network-free tests.
- [x] W3: UniswapV3 `PoolCreated` discovery has a separate V3 metadata cache and
      is gated by `POOL_DISCOVERY_V3=false` by default.
- [x] W4 implementation: direct multi-leg V2 cycle enumeration is wired into
      `on_block` with the documented budgets. `ARB_MAX_CYCLE_LEN=2` remains the
      safe default until the funnel baseline is measured.

Remaining Phase 2 coverage:

- [ ] UniswapV3 sandwiches and V3 legs in the arb search (W5)
- [ ] UniversalRouter calldata decoding (W6). 1inch v6, 0x v2, and CoW Swap
      decoders are explicitly out of scope for this phase.
- [ ] Raise W4 to 3–5 legs after the funnel week and report the before/after
      candidate-volume delta.
- [ ] Curve / Balancer / Maverick pool math (out of scope for this phase)
- [ ] Compound V3, Morpho and Maker liquidations; oracle-update front-running
      (out of scope for this phase)
- [ ] MEV-Share backrun bidding (`mev_sendBundle` with privacy hints)
      (out of scope for this phase)

**Status note (2026-08-21):** No new Phase 2 strategy box is ticked in this
session — W5/W6 remain gated on funnel data and raising W4 to 3–5 legs waits
for the same baseline. However, **W0's remote CI is now enabled**: the
maintainer granted GitHub `workflows` permission, the workflow was moved into
`.github/workflows/ci.yml`, and GitHub Actions is running on every push and PR.
CI passes `contracts (foundry)` and `frontend (next.js)`. It surfaced a real
bug the artifact-drift job — `compile-check.js` embedded solc's IPFS metadata
hash (which includes absolute source paths) into `MevExecutor.runtime.hex`, so
the artifact only reproduced in the sandbox where it was generated. Fixed by
disabling that hash (`bytecode_hash = "none"`, matching `foundry.toml`) and
regenerating the artifact; a fresh checkout now reproduces it byte-for-byte.
`bot (rust)` still fails on `cargo test --all` (exit 101) and needs diagnosis on
a Rust-capable environment. Details in [`docs/BUILD_NOTES.md`](BUILD_NOTES.md).

## Phase 3 — going live (opt-in, separate PR)

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
