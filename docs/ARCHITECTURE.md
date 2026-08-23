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
| `bot/crates/mev-bot/src/inventory.rs` | Pending-chain nonce, serialized private reservations, recovery block, and exact ETH/WETH inventory gates |
| `bot/crates/mev-bot/src/qualification.rs` | Per-strategy continuity/sample/accuracy verdicts (`PASS`, `FAIL`, `INSUFFICIENT SAMPLE`) |
| `bot/crates/mev-bot/src/submission.rs` | Signed multi-relay `eth_sendBundle`, bounded same-UUID retries, and cancellation |
| `bot/crates/mev-bot/src/attribution.rs` | Exact finalized own-outcome reconciliation and confidence-scored competitor evidence |
| `bot/crates/mev-bot/src/api.rs` | REST + SSE, including runtime mode, qualification, actual MEV, and execution outcomes |
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
6. Shadow results are written to SQLite and pushed over SSE. For live-lane
   candidates only, the engine independently checks boot arming, broadcast
   capability, runtime mode, strategy qualification, inventory, and nonce
   recovery. A qualified candidate enters the single nonce lane and the exact
   reserved-nonce payload is simulated.
7. The reservation is synchronously persisted before concurrent signed
   `eth_sendBundle` calls. Transport failures receive bounded same-UUID retries.
8. Submitted hashes are reconciled after canonical finality. Executor events
   and receipts produce exact own gross, builder payment, retained profit, gas,
   and net profit; partial/incoherent outcomes stay explicit.

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

## Multi-chain (Ethereum + Base, one process per chain)

The engine is single-chain by design — one `Config`, one fork, one store, one
nonce lane — so a second chain is a **second process**, not a mode. Each
instance gets its own env file, port, database and systemd unit
(`mev-bot@<chain>`, [`DEPLOYMENT.md`](DEPLOYMENT.md#multi-chain-layout-ethereum--base)),
which makes qualification, the smoke budget, the kill switch and nonce
recovery per-chain **by construction**.

What is actually chain-aware in the code (added with Base, all env-overridable):

| Layer | How it varies per chain |
| --- | --- |
| Address registry | `config::known::for_chain(CHAIN_ID)` → `ethereum()` or `base()` profile (verified deployments); env `*_ADDRESS` overrides win field-by-field, so a chain without a built-in profile is fully env-driven |
| Strategy availability | A strategy whose protocol is absent from the profile is not constructed (boot warning); sequencer chains warn when front-run strategies are enabled (they are back-run-only there) |
| Discovery | Factory addresses (V2 `PairCreated`, V3 `PoolCreated`) come from the registry, not constants |
| Delivery | `SUBMISSION_MODE`: `bundle` (relays, mainnet) or `raw` (signed txs straight to the chain RPC with a priority fee; sequencer chains have no relay market). Raw cancellation replaces the nonce with both original EIP-1559 caps percentage-bumped, current base fee covered, and an operator hard cap |
| Qualification | `QUALIFICATION_BACKEND`: `relay` (fork vs `eth_callBundle`) or `sequencer` (fork vs an independently recorded canonical state transition). Route/outcome matches stay in a separate population and cannot satisfy both thresholds |
| Delivered blocks | Mainnet: bloXroute relay data API. Sequencer chains: `CHAIN_BLOCK_INGEST` polls the chain's own heads and scores each built block (the sequencer's block *is* the delivered block) |
| Simulation | Fork URL is the chain's RPC; `REFORK_EVERY_BLOCKS` defaults follow the block cadence (1 on 12 s mainnet, 6 on 2 s Base); signatures carry the chain id |

Base is a **sequencer chain**: no public mempool (front-run strategies are
off in `.env.example.base`), no relay market, and `BRIBE_BPS=0`. The current
Base process is a measurement instrument, not a certified revenue lane:
`atomic_arb` prices only the V2 graph, Base registers one V2 venue, V3 discovery
does not feed that graph, and a pending dependency cannot yet be represented as
a preconfirmed-state raw backrun. It must remain shadow-only until
[`BASE_REVENUE_PATH_WORK_ORDER.md`](BASE_REVENUE_PATH_WORK_ORDER.md) is done.
The console still multiplexes both instances behind `CHAINS`. Lending-protocol
deployments exist on Base but are deliberately unregistered (phase 2,
[`STRATEGIES.md`](STRATEGIES.md)).
