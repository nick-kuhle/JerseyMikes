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

- [ ] Replay harness: re-simulate historical blocks from the database and
      compare against what actually landed on chain (from relay bid traces)
- [ ] Model competition: rank our bundle against the block's realised builder
      payment to estimate true inclusion probability
- [ ] Latency budget: per-stage timing histograms; the mempool→bundle path
      needs to be under ~150 ms to matter
- [ ] Nonce/inventory manager (currently the bundle nonce is a placeholder)
- [ ] Re-org handling and per-block reconciliation

## Phase 2 — coverage

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
