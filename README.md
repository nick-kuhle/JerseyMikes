# JerseyMikes

A simulation-first MEV searcher for Ethereum mainnet: **sandwich, JIT liquidity,
atomic arbitrage, liquidations and new-token sniping**, wired to live mempool
and private-orderflow data, scored against a forked EVM, and rendered in a
real-time console.

**Nothing is broadcast.** The bot reads real mainnet data, builds real bundles,
and simulates them against real state — then records the result instead of
sending it. Turning that off takes two independent environment variables that
are not set by default.

```
bot (Rust)                contracts (Foundry)          frontend (Next.js)
──────────                ───────────────────          ──────────────────
mempool + MEV-Share  ──▶  MevExecutor.execute()   ──▶  P/L + equity curve
relay + block feeds       profit-or-revert guard       simulated tx history
5 strategies              Balancer flash loans         live mempool tape
anvil fork simulation     V3 JIT mint callback         contract control panel
SQLite + REST/SSE         coinbase bribe               (viem, injected wallet)
```

---

## Quick start

```bash
git clone --recurse-submodules <this repo> && cd JerseyMikes
make setup                      # submodules + npm deps + .env
$EDITOR .env                    # set ETH_HTTP_URL and ETH_WS_URL

make doctor                     # verify every endpoint answers
make bot-run                    # searcher + API on :8080
make front-dev                  # console on :3000
```

The dashboard works before the bot does: if the API is unreachable it renders
generated data behind a **DEMO DATA** badge so you can see the shape of
everything first.

Requirements (install walkthrough + troubleshooting in
[`docs/SETUP.md`](docs/SETUP.md)):

| Tool | Version | Why |
| --- | --- | --- |
| **Rust** | 1.79+ | searcher, simulator and API (`bot/`) |
| **[Foundry](https://getfoundry.sh)** | latest | contracts build/tests; `anvil` is the simulation engine |
| **Node.js** | 20+ | the console (`frontend/`) |
| Ethereum RPC | archive-capable, mainnet | live + historical state |

---

## What it does

### Data it listens to

| Source | Used for |
| --- | --- |
| Public mempool (`newPendingTransactions` + batched hydration) | sandwich, JIT, back-run targets |
| Flashbots **MEV-Share** SSE | private orderflow hints; acted on when calldata is present |
| `newHeads` | block cadence, base fee, pool-cache refresh |
| Relay **data API** (`proposer_payload_delivered`) | what the winning builder actually paid — the market price of each block's MEV, and our benchmark |
| Third-party mempool streams (bloXroute / Blocknative) | optional, comma-separated in `EXTRA_MEMPOOL_WS` |
| L2 sequencer / preconfirmation feed | wired and idle until chain #2 |

### How an opportunity is scored

1. A strategy proposes a bundle (`Opportunity`).
2. `RiskEngine` gates it on notional, base fee, inflight count, kill switch.
3. The victim's **raw signed transaction** is fetched and the bundle is replayed
   `front → victim → back` inside an `anvil` fork of mainnet at the current head,
   with automine off so all of it lands in one block.
4. The executor's realised balance delta, minus gas and the builder bribe, is
   the recorded P/L — not an estimate.
5. In parallel, the relay's `eth_callBundle` scores the same bundle. Divergence
   between the two is itself a signal.
6. Everything is written to SQLite and streamed to the dashboard.

Step 7 — `eth_sendBundle` — is the only thing this build does not do.

### Why a losing bundle costs nothing

`MevExecutor` measures the profit token's balance before and after the batch and
reverts with `Unprofitable(realised, required)` unless the delta clears
`minProfit`. Bundles go through private orderflow, so a reverting bundle is
**dropped by the builder and never included**: no block space, no gas. That is
what makes liberal risk parameters safe to start with.

---

## The contract

`contracts/src/MevExecutor.sol` — one generic executor for all five strategies.
Strategies are encoded off-chain as an ordered `Call[]`, so a strategy change
never needs a redeploy.

- `execute(tag, calls, guard)` — atomic batch with a hard profit requirement
- `flashExecute(tag, tokens, amounts, calls, guard)` — same, funded by a
  zero-fee Balancer V2 flash loan
- `uniswapV3MintCallback` + `armV3Callback` — JIT liquidity without the NFT
  position manager (pool positions are keyed by owner + tick range)
- `quote(calls, profitToken)` — `eth_call`-only dry run for off-chain sizing
- Guards: `minProfit`, `blockDeadline`, `maxBaseFee`, `bribeBps`, searcher
  allowlist, transient-storage reentrancy/callback protection

Runtime size 9,618 bytes. Tests cover the profit invariant, every guard, a
flash-loan arbitrage, a full sandwich round trip, and access control.

```bash
cd contracts && forge test -vvv
# no Foundry? a solc-only type-check + artifact regeneration:
node script/compile-check.js
```

## The bot

Single Rust crate, `bot/crates/mev-bot`. Deliberately thin on dependencies: the
JSON-RPC client, RLP encoder and EIP-1559 signer are ~250 lines of auditable
code rather than a provider stack, because the exact bytes we sign matter.

```
src/
  ingest.rs      every data source → one normalised event
  dex.rs         AMM math, optimal sandwich/arb sizing, V3 quoter
  strategies/    sandwich · jit · arb · liquidation · sniper
  risk.rs        gates, caps, drawdown kill switch
  sim/           anvil fork backend + relay eth_callBundle backend
  bundle.rs      Opportunity → calldata → signed bundle → relay payloads
  signer.rs      secp256k1, EIP-1559, X-Flashbots-Signature
  store.rs       SQLite schema + P/L aggregates
  api.rs         REST + SSE
  engine.rs      the loop that ties it together
```

```bash
cd bot
cargo test --all                     # unit tests: AMM math, RLP, signing, risk, store
cargo run --bin mev-bot -- doctor    # endpoint pre-flight
cargo run --release --bin mev-bot    # run
```

## The console

`frontend/` — Next.js 15, live at `:3000`.

- Cumulative simulated P/L (equity curve) and per-strategy breakdown
- Simulated transaction history with gas, gross, net and revert reasons
- Live tape: mempool transactions, MEV-Share hints, blocks, opportunities,
  simulations, bundles, relay payloads — filterable
- Relay payload-delivered table: what MEV actually sold for, block by block
- `MevExecutor` control panel: read `owner`/`searchers`/balance with a plain
  RPC, and `setSearcher`/`sweep` with an injected wallet (viem)

The browser only ever talks to `/api/bot/*`, which the Next server proxies to
`BOT_API_URL`.

---

## Configuration

Everything is environment-driven; see [`.env.example`](.env.example) for the
annotated list. The only required variable is `ETH_HTTP_URL`.

Risk defaults are intentionally permissive — `MIN_NET_PROFIT_WEI=1`,
`MAX_POSITION_WEI=100 ETH`, every strategy on — because the first run's job is
to measure what is reachable, not to be profitable. See
[`docs/RISK.md`](docs/RISK.md) for the suggested tightening order.

## Documentation

| Document | Contents |
| --- | --- |
| [`docs/SETUP.md`](docs/SETUP.md) | Install walkthrough for all three toolchains, `.env`, `make doctor`, troubleshooting |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | Wiring, data flow, repo layout, how to add a chain |
| [`docs/STRATEGIES.md`](docs/STRATEGIES.md) | Each strategy: trigger, sizing math, traps avoided, what's missing |
| [`docs/RISK.md`](docs/RISK.md) | Why nothing can be broadcast, every guard, known limitations |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | Phased plan through going live and chains 2–5 |
| [`docs/MAINTAINING.md`](docs/MAINTAINING.md) | How the codebase thinks: mindset, change patterns, footguns, the landscape ahead |
| [`docs/PHASE_2_HANDOFF.md`](docs/PHASE_2_HANDOFF.md) | Phase 2 work order: W0–W6 tickets with budgets and acceptance criteria (temporary; deleted when Phase 2 ships) |
| [`docs/BUILD_NOTES.md`](docs/BUILD_NOTES.md) | What CI verifies and what the authoring sandbox could not |

## Status

Phase 0 of [the roadmap](docs/ROADMAP.md): the full pipeline exists end to end
and is simulation-only. Before this is worth trusting with real money it needs
the Phase 1 work — replay validation against blocks that actually landed,
competition modelling, and a latency budget.

⚠️ Sandwich attacks extract value from other users. This repository is a
research tool; deploying it against live users is your decision and your
liability.
