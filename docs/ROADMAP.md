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

- [ ] UniswapV3 sandwiches and V3 legs in the arb search
- [ ] Aggregator calldata decoding: UniversalRouter, 1inch, 0x, CoWSwap
- [ ] Multi-leg negative-cycle search over the full pool graph
- [ ] Curve / Balancer / Maverick pool math
- [ ] Compound V3, Morpho and Maker liquidations; oracle-update front-running
- [ ] MEV-Share backrun bidding (`mev_sendBundle` with privacy hints)

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
