# JerseyMikes

A simulation-first MEV searcher for **Ethereum mainnet and Base** (one process
per chain): **sandwich, JIT liquidity, atomic arbitrage, liquidations
(Aave V3, Compound V3, Morpho Blue, Maker), oracle-update front-running and
new-token sniping**, wired to live mempool and private-orderflow data (or, on
sequencer chains, the sequencer feed), scored against a forked EVM, and
rendered in a real-time console.

**Broadcasting is disabled by default and fail-closed.** The live transport
exists, but requires separate boot arming, broadcast capability, authenticated
runtime mode, risk and inventory approval, durable nonce recovery, exact-payload
simulation, and an independent strategy-specific qualification `PASS` after at
least seven continuously observed days.

```
bot (Rust)                contracts (Foundry)          frontend (Next.js)
──────────                ───────────────────          ──────────────────
mempool + MEV-Share  ──▶  MevExecutor.execute()   ──▶  P/L + equity curve
relay + block feeds       profit-or-revert guard       simulated tx history
10 strategy rows          Balancer flash loans         live mempool tape
liq ×4 + oracle backruns  V3 JIT mint callback         contract control panel
anvil fork simulation     coinbase bribe               (viem, injected wallet)
SQLite + REST/SSE
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
| **Rust** | 1.90+ | searcher, simulator and API (`bot/`) |
| **[Foundry](https://getfoundry.sh)** | latest | contracts build/tests; `anvil` is the simulation engine |
| **Node.js** | 22+ | the console (`frontend/`) |
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
| **bloXroute Max Profit relay** delivered blocks | the winning block's transactions are fetched, stored and scored for extractable value |
| Third-party mempool streams (bloXroute / Blocknative) | optional, comma-separated in `EXTRA_MEMPOOL_WS` |
| L2 sequencer / preconfirmation feed | wired; on **Base** the chain's own built blocks are ingested as the delivered blocks (`CHAIN_BLOCK_INGEST`, on by default there) |

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
6. Everything is written to SQLite and streamed to the dashboard. Canonical
   delivered blocks are matched to decision-time opportunities and competitor
   economics are confidence/completeness scored.
7. Only a qualified live strategy enters the serialized durable nonce lane;
   that exact payload is rechecked and sent to configured private relays.
8. Own submitted hashes are reconciled after finality into exact gross,
   builder payment, retained profit, gas, and net profit. Partial/incoherent
   inclusion remains an explicit incident state.

### Why a losing bundle costs nothing

`MevExecutor` enforces retained profit after builder payment and reverts with
`Unprofitable(realised, required)` below `minProfit`. A correctly simulated
atomic private bundle that reverts should be dropped without gas cost. Relay or
builder defects and partial inclusion are still operational risks, so the bot
uses non-reverting payload policy, finality-aware receipt reconciliation,
explicit incident states, and a drawdown stop.

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

Runtime size 11,497 bytes (deterministic across checkouts — `compile-check.js`
disables solc's IPFS metadata hash, which otherwise embeds each source file's
absolute path). Tests cover the profit invariant, every guard, a
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
  strategies/    sandwich · jit · arb · sniper
                 liquidation ×4 (aave · compound · morpho · maker)
                 oracle_frontrun (price-update back-runs)
  risk.rs        gates, caps, drawdown kill switch
  sim/           anvil fork backend + relay eth_callBundle backend
  bundle.rs      Opportunity → calldata → signed bundle → relay payloads
  signer.rs      separate EIP-1559 transaction and relay-reputation signing
  qualification.rs  per-strategy continuity, evidence, and accuracy verdicts
  submission.rs  multi-relay retries/cancellation
  attribution.rs exact own outcomes + confidence-scored competitor evidence
  store.rs       SQLite P/L, qualification, nonce, relay, and finality state
  api.rs         REST + SSE
  engine.rs      the loop that ties it together
```

The bloXroute Max Profit relay integration (`RELAY_TX_INGEST=true`, on by
default) polls `proposer_payload_delivered`, fetches each delivered block's
transactions via `eth_getBlockByHash`, persists them to SQLite
(`relay_blocks` / `relay_block_txs`), and routes every transaction through the
same strategy → risk → simulation funnel as a mempool transaction so the bot
records whether value was extractable. See
[`docs/BLOXROUTE_RELAY.md`](docs/BLOXROUTE_RELAY.md).

```bash
cd bot
cargo test --all                     # unit tests: AMM math, RLP, signing, risk, store
cargo run --bin mev-bot -- doctor    # endpoint pre-flight
cargo run --release --bin mev-bot    # run
```

## Multi-chain (Ethereum + Base)

One `mev-bot` process per chain — the engine is single-chain by design, and
process isolation means a Base run can never touch the mainnet instance (or
its 7-day qualification clock). `CHAIN_ID` selects a built-in, live-verified
address profile (`1` = Ethereum, `8453` = Base); any other id runs with an
empty profile that the `*_ADDRESS` env vars fill in field-by-field, so a new
chain needs no code.

Sequencer chains (Base) differ in three env rows: `SUBMISSION_MODE=raw`
(signed txs go straight to the chain RPC — no relay market — with
`PRIORITY_FEE_WEI` as the ordering currency), `QUALIFICATION_BACKEND=sequencer`
(the qualification second opinion is the included block, not a relay
`eth_callBundle`), and `BRIBE_BPS=0`. Front-run strategies are off by default
there (no public mempool — it is back-run-only); the v1 lane is flash-loan
`atomic_arb`. See `.env.example.base` and [`docs/SETUP.md`](docs/SETUP.md).

## The console

`frontend/` — Next.js 16, live at `:3000`.

- Cumulative simulated P/L (equity curve) and per-strategy breakdown
- Simulated transaction history with gas, gross, net and revert reasons
- Live tape: mempool transactions, MEV-Share hints, blocks, opportunities,
  simulations, bundles, relay payloads — filterable
- Relay payload-delivered table: what MEV actually sold for, block by block
- `MevExecutor` control panel: read `owner`/`searchers`/balance through the
  dashboard's own read-only RPC proxy (`/api/eth`), and write `setSearcher`/
  `sweep` with a connected wallet (viem) — transaction receipts followed live
- **Wallet**: EIP-6963 multi-wallet discovery (pick between installed
  wallets), eager reconnect, account/chain/balance state, and a one-click
  switch to the bot's chain when the wallet sits elsewhere
- **Execution-mode switch**: the SIMULATION ONLY / LIVE EXECUTION badge is a
  real switch backed by `GET/POST /api/mode`. It can only *narrow* what the
  environment allowed — a bot not started with `LIVE_EXECUTION=true` **and**
  `I_UNDERSTAND_LIVE_RISK=yes` refuses the switch with the arming steps
  (see [`docs/RISK.md`](docs/RISK.md)); an armed bot can pause/resume live
  mode without a restart
- **Block-explorer links** on every transaction, block and address the console
  renders — victim txs in the simulation history, opportunities, the live
  tape, delivered-block transactions, and the executor's addresses
- **Risk envelope, instant**: every control in the risk panel applies at
  runtime (`POST /api/risk`) — the next opportunity is gated, the next
  bundle's guards priced, with the values you just set. No restart; the
  `.env` snippet remains for persisting values as boot defaults. Strategy
  toggles narrow only (boot set is the ceiling), and a tripped drawdown
  kill switch can be re-armed from the panel.
- **Alerts**: rule engine (kill switch, endpoint stalls, conversion
  collapse, reorgs) with fire/resolve lifecycle — `GET /api/alerts`, the
  live tape (`alert` events), and Prometheus metrics at `/api/metrics`
  (see [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md))
- **Go-live checklist**: the six-step MevExecutor deployment panel
  (`docs/GO_LIVE.md` Path A) — connect wallet, gas check, deploy (CI-checked
  bytecode, free cost estimate), fund, `setSearcher`, verify + copy the
  `EXECUTOR_ADDRESS` line. Deployment alone changes no execution mode; all
  broadcast and strategy-specific qualification gates still apply.
- **W6 go/no-go card** in the funnel panel: the public-mempool gap reading
  (`pendingSeen` vs sandwich/JIT `invocationsEmpty`/`candidatesEmitted`,
  7-day sample gate) that decides whether UniversalRouter decoding gets
  flipped — see [`docs/W6_MEMO.md`](docs/W6_MEMO.md) (Ethereum only — a
  sequencer chain has no public mempool to gap)
- **Chain indicator + switcher**: the header always shows the active chain
  as a green pill (so which chain a screenshot came from is never
  ambiguous). With `CHAINS` set (e.g. `ethereum|…,base|…`), the pill
  becomes a switcher — click Ethereum ↔ Base and the whole panel tree
  re-keys on the slug so no panel can show another chain's data. An
  unreachable bot falls back to the flagged DEMO state for that chain
  only. Single-chain deployments (`CHAINS` unset) get one inert pill
  labelled with the only chain. The wallet's network and the console's
  active chain are independent; a mismatch shows an amber banner under
  the header, and the go-live panel's wallet gate follows the console's
  active chain (so a Base console offers "switch wallet to Base", not
  "switch to mainnet").

The browser only ever talks to `/api/bot/*`, which the Next server proxies to
`BOT_API_URL`; contract reads go through `/api/eth`, a server-side
read-only RPC proxy (uses the bot's `ETH_HTTP_URL` when set).

---

## Configuration

Everything is environment-driven; see [`.env.example`](.env.example) for the
annotated list. The only required variable is `ETH_HTTP_URL`.

The bot's API binds `127.0.0.1:8080` by default. Its three mutating endpoints
(`POST /api/mode`, `/api/risk`, `/api/risk/reset`) can trip the kill switch,
disable strategies, and rewrite the risk envelope, so exposing them off-box
requires a shared secret: set `API_AUTH_TOKEN` whenever `API_BIND` is not
loopback, or the bot refuses to start — in simulation mode too. See
[Securing the API](docs/DEPLOYMENT.md#securing-the-api).

Upgrading an existing `.env` requires adding the signer, broadcast,
qualification, finality, and API-security variables deliberately. Follow the
ordered migration in [`docs/SIM_TO_LIVE.md`](docs/SIM_TO_LIVE.md).

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
| [`docs/RISK.md`](docs/RISK.md) | Fail-closed broadcast predicate, executor guards, nonce/finality and known limitations |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | Phased plan through going live and chains 2–5 |
| [`docs/MAINTAINING.md`](docs/MAINTAINING.md) | How the codebase thinks: mindset, change patterns, footguns, the landscape ahead |
| [`docs/PHASE_2_HANDOFF.md`](docs/PHASE_2_HANDOFF.md) | Phase 2 work order: W0–W6 tickets with budgets and acceptance criteria (temporary; deleted when Phase 2 ships) |
| [`docs/W6_MEMO.md`](docs/W6_MEMO.md) | The public-mempool gap memo that gates UniversalRouter decoding — template + decision record |
| [`docs/SIM_TO_LIVE.md`](docs/SIM_TO_LIVE.md) | Switching a simulating bot over to live: securing the API first, the `.env` upgrade trap, tightening risk, arming last |
| [`docs/PATH_TO_LIVE.md`](docs/PATH_TO_LIVE.md) | The one-page in-the-room runbook for the live-smoke burst + 7-day soak (a per-chain procedure) |
| [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md) | systemd + Docker Compose units, `/api/metrics`, the alert rules and their knobs |
| [`docs/BUILD_NOTES.md`](docs/BUILD_NOTES.md) | What CI verifies and what the authoring sandbox could not |

## Status

The repository now contains the production relay path, but all execution
defaults remain off. Current engineering live candidates are V2 sandwich, V3
sandwich, and atomic arbitrage; each must independently earn a `PASS` from
continuous canonical fork/relay/on-chain accuracy evidence. Other strategy rows
remain visible and explicitly ineligible until their documented settlement or
valuation limitation is removed.

Production operations include separate transaction/reputation signers,
multi-relay same-UUID retries and cancellation, durable serialized nonce
reservations and restart recovery, finality-aware exact own-outcome accounting,
confidence/completeness-scored competitor attribution, hardened Docker/systemd
targets, and qualification/execution reporting in the API and console. Follow
[`docs/SIM_TO_LIVE.md`](docs/SIM_TO_LIVE.md); arming is now a real money switch.

⚠️ Sandwich attacks extract value from other users. This repository is a
research tool; deploying it against live users is your decision and your
liability.
