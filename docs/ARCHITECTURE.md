# Architecture

```
                    ┌──────────────────────────────────────────────┐
 public mempool ───▶│                                              │
 MEV-Share SSE  ───▶│  ingest.rs   normalise → IngestEvent         │
 newHeads       ───▶│                                              │
 relay data API ───▶│                                              │
 sequencer feed ───▶└───────────────────┬──────────────────────────┘
                                        │
                          ┌─────────────▼─────────────┐
                          │ strategies/               │  ← pool cache, router
                          │  sandwich · jit · arb ·   │    calldata decoding,
                          │  sniper · liq ×4 ·        │    AMM math, and the
                          │  oracle_frontrun          │    near-miss leads
                          └─────────────┬─────────────┘    registry
                                        │ Opportunity
                          ┌─────────────▼─────────────┐
                          │ risk.rs                   │  size caps, base-fee cap,
                          │  gate / kill switch       │  inflight caps, drawdown
                          └─────────────┬─────────────┘
                                        │
                    ┌───────────────────▼───────────────────┐
                    │ sim/                                  │
                    │  anvil fork (ground truth)            │
                    │  + relay eth_callBundle (cross-check) │
                    └───────────────────┬───────────────────┘
                                        │ SimulationResult + BundleRecord
                          ┌─────────────▼─────────────┐
                          │ store.rs (SQLite)         │
                          │ api.rs   REST + SSE       │
                          └─────────────┬─────────────┘
                                        │
                          ┌─────────────▼─────────────┐
                          │ Next.js dashboard         │  P/L, history, feeds,
                          │ + viem contract panel     │  on-chain control
                          └───────────────────────────┘
```

## Why this shape

**One normalised event type.** Every source — public mempool, MEV-Share, a
sequencer feed, a third-party stream — is flattened into `IngestEvent` before a
strategy sees it. Adding a source is one function in `ingest.rs`; no strategy
changes.

**Strategies are pure-ish functions.** `StrategyImpl::on_pending` /
`on_block` take a context and return `Vec<Opportunity>`. They do not execute,
do not persist, and do not decide risk. That makes them unit-testable without a
chain, which is how the AMM sizing math is covered today.

**Simulation is the arbiter, not the estimator.** The off-chain estimate exists
only to decide whether a candidate is worth a simulation slot. The number that
lands in P/L always comes from a forked EVM executing the real bundle against
real mainnet state.

**Two independent verdicts.** The local anvil fork is ground truth for
accounting. The relay's `eth_callBundle` is what the *builder* would compute.
When they disagree, that disagreement is itself the signal — it usually means
the victim replay was not faithful.

## Repository layout

| Path | Contents |
| --- | --- |
| `contracts/src/MevExecutor.sol` | Generic atomic executor with a hard profit guard, Balancer flash loans, V3 mint callback for JIT |
| `contracts/test/` | Foundry tests + mock ERC20 / WETH / V2 pair / Balancer vault |
| `contracts/script/compile-check.js` | solc-js compile check; also emits the ABI + runtime bytecode the bot embeds |
| `bot/crates/mev-bot/src/rpc.rs` | JSON-RPC client, WS subscriptions, SSE reader |
| `bot/crates/mev-bot/src/dex.rs` | Constant-product math, optimal sandwich/arb sizing, V3 quoter |
| `bot/crates/mev-bot/src/strategies/` | The strategy rows (sandwich ×2, JIT, arb, liquidations ×4, oracle front-run, sniper) plus the shared near-miss `leads.rs` registry |
| `bot/crates/mev-bot/src/sim/` | anvil fork backend + relay `eth_callBundle` backend |
| `bot/crates/mev-bot/src/signer.rs`, `rlp.rs` | EIP-1559 signing and a 60-line RLP encoder |
| `bot/crates/mev-bot/src/store.rs` | SQLite schema and aggregate queries |
| `bot/crates/mev-bot/src/replay.rs` | Offline harness: stored sims × relay traces → true-positive rate |
| `bot/crates/mev-bot/src/competition.rs` | Inclusion probability from our bribe vs realised builder payment |
| `bot/crates/mev-bot/src/latency.rs` | Per-stage histograms; 150 ms mempool→bundle budget |
| `bot/crates/mev-bot/src/inventory.rs` | Searcher nonce + ETH/WETH balances |
| `bot/crates/mev-bot/src/api.rs` | REST + SSE for the dashboard; `GET/POST /api/mode` is the runtime simulation ⇄ live switch (see `docs/RISK.md`) |
| `frontend/` | Next.js console (`/api/bot/*` proxies the bot; `/api/eth` is a server-side read-only RPC proxy for contract reads) |

## The execution path, end to end

1. A router swap appears in the mempool. `ingest` hydrates it and pushes a
   `PendingTx`.
2. `SandwichStrategy` decodes the calldata, loads the pair's reserves from the
   per-block pool cache, and solves for the optimal front-run size with a
   ternary search over the exact integer AMM curve. It refuses to sandwich a
   victim whose `amountOutMin` would be violated.
3. `RiskEngine::check` applies notional, base-fee, inflight and kill-switch
   limits.
4. `Simulator::run` fetches the victim's raw signed bytes
   (`eth_getRawTransactionByHash`), builds the three-transaction bundle, and
   replays `front → victim → back` inside a mainnet fork with automine off, so
   all three land in one block exactly as a bundle would.
5. The realised balance delta of the executor, minus gas and the builder bribe,
   is the recorded net P/L. In parallel the relay simulates the same bundle.
6. The result and the (unsent) bundle are written to SQLite and pushed to the
   dashboard over SSE.

Step 6 is where a live bot would call `eth_sendBundle`. This build stops there.

## The bloXroute Max Profit relay path

On top of the value benchmark (`relay_bids`), the engine polls the bloXroute
Max Profit relay's `proposer_payload_delivered` bid traces and, for every newly
delivered block, fetches its full transaction list with `eth_getBlockByHash`.
Each delivered block is persisted (`relay_blocks`, `relay_block_txs`) and pushed
to the dashboard, and each of its transactions is routed through `on_pending`'s
shared `evaluate` funnel with `TxSource::RelayDelivered`. The result is a
post-mortem replay of the winning builder's block against the fork: our
strategies propose opportunities against exactly the transactions that actually
landed, and the simulator records whether value was extractable. See
[`BLOXROUTE_RELAY.md`](BLOXROUTE_RELAY.md).

## Adding the second chain

`ChainConfig` already carries chain id, WETH, stable, and block time; the
strategies read addresses from `config::known` or from config. To add, say,
Base: point `ETH_HTTP_URL`/`ETH_WS_URL` at it, set `CHAIN_ID`, override the
factory/router/vault addresses, set `SEQUENCER_FEED_URL`, and disable
`STRATEGY_SANDWICH` (no public mempool to sandwich). The simulation and
accounting layers are chain-agnostic.
