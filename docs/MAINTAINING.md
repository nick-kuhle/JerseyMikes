# Maintaining JerseyMikes

A guide for the team that comes next. This is not a how-to for using the
bot — that's what [`SETUP.md`](SETUP.md) and the README are for. This is
about how the codebase *thinks*, what you should be careful about when
changing it, and how the work ahead is shaped by where DeFi is going.

If you only read three sections, read **§1 Mindset**, **§3 Common
Change Patterns**, and **§6 The Landscape**.

This guide is permanent — it outlives any single phase of work. The
time-boxed Phase 2 work order lives in
[`PHASE_2_HANDOFF.md`](PHASE_2_HANDOFF.md) and is deleted once Phase 2
ships; anything in it that turns out to be a durable rule about the
codebase should be folded back into this document before that happens.

---

## 1. Mindset

The single most important thing to internalise:

> **The bot is a measurement instrument, not a money printer.**

The simulation pipeline is deliberately conservative at every gate. A
bundle that survives all the filters and comes out the other side net
positive is, by construction, a candidate that the on-chain profit
guard would let through. The reason "few opportunities" is the steady
state is that the gates are doing their job, not that the system is
broken. The funnel counter (added in PR #5) is the instrument that
turns this from a feeling into a number.

Three corollaries follow:

- **Don't loosen the gates to chase opportunities.** If the funnel says
  "all zeros", the right answer is almost never "lower
  `MIN_NET_PROFIT_WEI`" — it's "the mempool feed is too quiet, or the
  pool cache is empty, or the RPC doesn't support
  `eth_getRawTransactionByHash`". Lowering the gate just produces
  more simulations that revert.

- **Never broadcast before Phase 1 work is done.** The README and
  `RISK.md` are explicit about this. The simulation-only guard is
  two independent environment variables, both required, by design. A
  bundle that the on-chain guard would revert is *not* a safe bundle
  to send — the builder drops reverted bundles, but the bundle still
  hits the mempool, costs gas, and is observable. Read
  [`ROADMAP.md`](ROADMAP.md) Phase 1 before doing anything that
  touches the `LIVE_EXECUTION` path.

- **The strategies you have are the strategies you should be
  improving.** New chains, new AMMs, new aggregators — these are
  obvious and high-traffic next moves, but the existing strategies
  on mainnet are where the simulation tape's silence lives.
  Expansion without measurement is a guess.

## 2. Reading the codebase

Start with [`ARCHITECTURE.md`](ARCHITECTURE.md) and the actual
`bot/crates/mev-bot/src/lib.rs` module list. The crate is small and
deliberately linear: ingest → strategies → risk → simulator → store →
API. There is one event loop in `engine.rs` and one broadcast channel
that the API consumes.

Things that look like over-engineering but aren't:

- **The `pool_by_addr: HashMap<Address, V2Pool>` you see in some
  strategy files** is a local index built per call to avoid walking
  the cache's internal `RwLock` more than necessary. Don't "simplify"
  it.
- **The repeated pattern of `if let Some(x) = ...; if let Some(y) = ...;`**
  instead of `?` in async strategy code: the `?` operator in a
  non-`try` async block propagates to the function, but we want
  early-return at the per-candidate level, not per-tx. The flat
  pattern is intentional.
- **The `sol!` macro blocks scattered through `dex.rs`, `jit.rs`,
  `v3_quote.rs`** are *the* ABI definitions. The router's exact
  parameter order, the `int24` packing, the `uint160` for
  `sqrtPriceLimitX96` — every byte matters. If you change one, you
  break every call site that uses it.

Things that look simple but are subtle:

- **`v2_amount_out` in `dex.rs`.** The integer division rounds
  *down*, matching the on-chain `getAmountOut`. A floating-point
  port would round consistently differently and produce bundles that
  revert in the fork.
- **`ternary_search_max`.** Assumes the function is unimodal. The
  profit curves for V2 sandwich and cyclic arb *are* unimodal; if
  you add a new strategy whose profit curve is not unimodal, the
  search will silently return a local maximum.
- **`optimal_sandwich_in`'s victim-revert check.** Any `x` that
  would push the victim's output below their `amountOutMin` is
  scored as zero profit. This is the trap that catches front-runners
  who don't model the slippage bound. Don't "optimise" it away.

## 3. Common change patterns

These are the changes you will be asked to make, in roughly the order
of how often they come up.

### 3.1 Add a new pair to the pool cache

This is a one-line change in `config.rs::known` if the pair is
canonical (e.g. `known::WETH` already lives there), or a runtime
discovery change in `strategies/sniper.rs` (the
`PairCreated` log scan) if the pair is new. The cache refresh
in `strategies/mod.rs::PoolCache::refresh_all` picks up new pools on
the next strategy tick.

When in doubt, **read the pool from chain first** rather than adding
constants. The cache invariants are easier to reason about if every
pool in the cache came through the same discovery path.

### 3.2 Add a new event to the live feed

Add a variant to `types::FeedEvent` (in `types.rs`), emit it from the
right place in `engine.rs`, and the dashboard will pick it up
through the existing SSE stream. The dashboard components are
tagged-dispatched; a new variant just needs a renderer.

If the new event has a backend cost (e.g. a per-event RPC call),
**don't add it to the broadcast channel unconditionally.** The
current `broadcast::channel` with `cfg.api.feed_capacity.max(64)`
is sized for the current event mix. If you add a high-rate event,
consider a separate channel or a sampled emit.

### 3.3 Add a new strategy

This is the most common and the most dangerous change. The pattern
is:

1. Add a variant to `types::Strategy` (yes, the `all()` array
   literal must be updated; the compiler will tell you).
2. Add a toggle to `config::StrategyToggles` and to
   `Config::from_env`.
3. Add a `pub mod your_strategy;` in `strategies/mod.rs`.
4. Implement `StrategyImpl` (see `jit.rs` for the most-complete
   example; `liquidation.rs` for the most-complex one).
5. Wire it into `engine.rs::new` (the
   `if cfg.strategies.your_strategy { strategies.push(...) }` block).
6. Add funnel-counter integration. This is not optional. The
   counters are how future maintainers (and you, in two weeks)
   will tell whether your new strategy is emitting anything.
7. Add a "Why no simulations?" entry to the existing diagnostics
   text in `frontend/components/RiskPanel.tsx` so users have a
   starting point when your strategy's counter is zero.

**Do not** add a strategy that runs on every pending transaction
without first profiling it. The pending-tx path is the hot path of
the entire bot; a 5 ms strategy × 200 pending txs/block = 1 s/block
of CPU on a serial executor, which the design assumes stays under
~50 ms/block.

### 3.4 Add a new chain

The contract layer (`MevExecutor.sol`) is chain-agnostic. The hard
work is the data layer. The pattern:

1. Add a `ChainConfig` case to `config.rs`. The `chain_id`,
   `weth` address, `block_time_ms`, and `name` are all the
   executor contract needs.
2. Add the chain's RPC URL(s) to `.env.example`. Different
   providers have different rate limits; document the recommended
   tier in the env example.
3. **Critical:** most L2s (Base, Arbitrum, Optimism) have no
   public mempool. The strategy code that depends on
   `newPendingTransactions` must be skipped or replaced with
   sequencer-feed-driven equivalents. The
   `SEQUENCER_FEED_URL` env var is already wired through the
   `ingest` layer; check `ingest.rs` for the "Sequencer" source
   variant.
4. The `PoolCache` is V2-flavoured by default. If the chain is
   V3-dominant (Base, most of Arbitrum), you'll want to extend
   the pool discovery to also pull V3 `PoolCreated` events.
5. Update `frontend/lib/format.ts::STRATEGY_LABEL` and
   `STRATEGY_COLOR` if you add per-chain labels.

The roadmap's Phase 4 is the right reference for chain-by-chain
scope.

### 3.5 Tune the risk gates

Two failure modes to be aware of:

- **Gates too loose:** the funnel will show
  `simulationsReverted >> simulationsSucceeded`. The right fix is
  almost always "fix the strategy's pre-filter sizing" (most
  candidates that revert are mis-sized in the pre-filter, not the
  fork). Lowering `MIN_NET_PROFIT_WEI` to compensate makes the
  funnel quieter, not louder.
- **Gates too tight:** the funnel will show
  `candidatesEmitted > 0` but `gatedByRisk` dominates. The right
  fix is the converse: raise `MAX_BASE_FEE_WEI` if the base fee
  is the gate, raise `MAX_POSITION_WEI` if notional is the gate,
  raise `MAX_INFLIGHT_PER_STRATEGY` if you're queueing work.

**Do not** change `BRIBE_BPS` below 5000 without understanding the
builder market. The default 9000 (90% to the builder) is the
industry standard for a reason: builders sort bundles by effective
payment per gas, and a 50% bribe outbid by a 90% bribe from a
competitor on the same opportunity will lose the auction. Lower
BRIBE_BPS only makes sense if you are submitting a *batch* of
opportunities and amortising the bribe across them.

## 4. Testing

The repo has two test layers: Rust unit tests in each module's
`#[cfg(test)] mod tests` block, and a Foundry test suite under
`contracts/`. There is no end-to-end integration test — the README
acknowledges this is a known gap, and a real integration test would
require a deterministic mainnet state fixture, which is a research
project on its own.

What you should add when you change something:

- **AMM math changes:** add a unit test in `dex.rs` that compares
  the new math against a known mainnet transaction's input/output
  values. The existing tests in that file use the same pattern
  (e.g. `amount_out_matches_solidity`).
- **New strategy:** add a unit test in the strategy's module that
  exercises the most common path (a "happy path" tx) and at least
  one rejection path (a tx that should be skipped, with the funnel
  counter you expect).
- **Funnel-counter changes:** add a test in `engine.rs` (see the
  four tests added in PR #5) that calls `Stats::record_funnel`
  directly and asserts the snapshot.

**Do not** write tests that depend on a live RPC. The test
infrastructure is supposed to run in CI without network access.
The only exception is the contracts' Forge tests, which run against
a local Anvil — that one is fine.

## 5. Common footguns

These are the bugs the existing code already worked around once
each. Don't reintroduce them.

- **Don't use `f64` for AMM math.** The existing code does, in the
  JIT strategy's `size_position`, and it's fine there because the
  sizing is approximate (the fork simulation is the arbiter). For
  sandwich and arb sizing, **always use `U256`**. A floating-point
  sandwich sizer will produce bundles that round-trip to zero in
  the fork on a 1-in-1000 swap; the unit tests will pass, the
  dashboard will lie, the bundles will revert in production.

- **Don't add a new `sol!` interface without verifying the
  selector.** The `sol!` macro generates a 4-byte selector from
  the function signature. If you reorder parameters, rename
  parameters, or change a type, the selector changes. The
  selector is hardcoded in some `decode_*` functions; check both
  the macro and the decoder match. The existing code's
  `decode_v3_swap` and `IUniswapV3Pool::swapCall::SELECTOR` are
  the canonical example.

- **Don't call `Arc::clone` on `Engine` in a tight loop without
  checking the spawn budget.** Every `tokio::spawn` consumes a
  runtime worker; the bot's per-tx strategy loop spawns one task
  per (tx, strategy) pair, which is 5× the per-block mempool rate
  at peak. A bursty block can produce hundreds of tasks in
  flight. The risk engine's `max_inflight_per_strategy` is the
  backstop; the simulator's `SIM_TIMEOUT_MS` is the second
  backstop. If you add a new strategy, both numbers may need
  tuning.

- **Don't serialise a `U256` into a string without
  `to_string()`.** The JSON serialisation defaults to the
  scientific-notation form for large `U256`s on some platforms,
  which viem doesn't parse. The existing code uses `.to_string()`
  everywhere it puts a `U256` into a JSON payload. Match that.

- **Don't assume `eth_call` returns a `revert` reason.** It
  doesn't, on mainnet — only on a forked node (Anvil) does the
  simulator get a structured revert. The simulator handles this
  asymmetry; the strategies assume `sim.revert_reason` is the
  Anvil one and not mainnet. If you add a new pre-filter that
  uses `sim.revert_reason`, make sure it's only called inside the
  fork path.

## 6. The landscape, and what to watch

This is the part of the guide that ages the fastest, but the
shape of what's coming is reasonably stable. The categories below
are the buckets to keep an eye on, with the implication for the
bot in each.

### 6.1 Private orderflow dominates public mempool

The trajectory is clear: the public mempool on Ethereum mainnet is
losing flow to private channels. Flashbots Protect, MEV-Blocker,
CoW Protocol's batch auctions, and the suite of "intent-based"
protocols (1inch Fusion, UniswapX, 0x v2 with private liquidity)
all take user transactions off the public mempool. The strategies
in this bot that depend on `newPendingTransactions` are running
against a shrinking share of the flow.

**What to do:**

- Watch the funnel's `pendingSeen` rate. If it trends down over
  months, the strategies that only act on public mempool will
  trend down with it. The `MEV_SHARE_SSE_URL` and
  `EXTRA_MEMPOOL_WS` are the partial mitigations already in the
  config.
- The long-term direction is **intent auctions**: protocols where
  the user signs an "intent" and a network of solvers compete to
  fill it, with the searcher competing for the right to be the
  solver. The bot's executor contract is the right primitive for
  this; what's missing is a solver-network integration. This is
  where the highest-value strategy work is over the next 1–2
  years, and it's not in the current roadmap. Worth a roadmap
  item.

### 6.2 L2 sequencer economics are evolving

Base and Arbitrum both run centralised sequencers that order
every transaction. The flow that the searcher can see is whatever
the sequencer publishes post-execution, which is a fraction of
what was visible. The roadmap's Phase 4 covers adding these
chains, but the opportunity profile is genuinely different from
mainnet:

- **Gas costs are 10⁻⁴ × mainnet.** A 5-leg arb that costs $20
  on mainnet costs 0.2¢ on Base. The `BRIBE_BPS` default is
  calibrated for mainnet's economics; an L2 strategy that doesn't
  rethink the bribe fraction will underperform.
- **Sequencer auctions are coming.** Arbitrum has been actively
  exploring an express-lane auction, and OP Labs has been
  publishing preconfirmation research. These are the
  lower-latency "sequencer feed" the README references; when
  they ship, the bot should integrate them, not by polling, but
  by the SDK or websocket protocol the chain itself publishes.
- **Cross-chain MEV is real but narrow.** The genuine opportunities
  (L1↔L2 deposits timed to land during low-fee windows, or
  L1↔L2 arbitrage against a re-org) are 1–2 sigma events, not
  steady-state. Don't build the bot around them; build a single
  chain first, then add the cross-chain paths as a separate
  research effort.

### 6.3 New AMM designs keep arriving

The strategies in the bot are V2 + V3 + Balancer (via flash
loans) + Aave V3 (for liquidations). The DeFi AMM space keeps
shipping new designs: Curve's stableswap, Maverick's
dynamic-position pools, Algebra's concentrated liquidity with
built-in fee tiers, Ekubo on Starknet, etc. Each is a new
calldata shape and a new pricing function.

**What to do:**

- Don't add a new AMM as a strategy. Add it as a *pricing module*
  in `dex/`, the way `v2_amount_out` and `v3_quote::quote_exact_in`
  are. Strategies then compose the pricing modules; they don't
  know which AMM they're trading against.
- The Curve and Balancer pool math is the next obvious addition.
  Both are documented and have Rust reference implementations.
  The `dex.rs` module list is the place to look when scoping it.
- New AMMs that aren't yet widely forked tend to have buggy
  reference math (the V3 `TickMath` had three separate CVEs
  found in the first year of deployment). The fork simulation
  is the safety net, but if a new pool is generating
  simulations with unexpected reverts, the first thing to check
  is the pool's actual on-chain behaviour, not the bot's
  pricing.

### 6.4 The contract-account / smart-wallet wave

ERC-4337 account abstraction, EIP-7702 (the Pectra-era "set code
to a smart contract" feature for EOAs), and the growing share of
flow going through smart wallets all change the shape of what
"the mempool" looks like. Specifically:

- **Bundles inside bundles.** A 4337 user operation can contain
  multiple calls; the bot's "one tx = one swap" pre-filter
  silently drops these. If 4337 flow grows to a meaningful share
  of the mempool, the strategies that don't unwrap user
  operations will see shrinking signal.
- **Sponsored gas.** Many AA flows have a third party paying the
  gas, which means the victim's `tx.value` is not the cost
  basis the searcher needs to model. The executor's
  profit-or-revert guard already handles this for the *searcher's*
  side, but the *victim's* slippage tolerance calculation does
  not — and that's the input to the sandwich sizing.

This is a watch-item, not a build-item. The 4337 share of mainnet
is still small; it is the kind of thing you check on every
quarterly review and act on when the data moves.

### 6.5 The privacy / regulatory environment

The legal and ethical status of MEV extraction is jurisdiction-
dependent and moving. Sandwich attacks in particular are under
active scrutiny. This guide does not give legal advice, but two
operational notes:

- **The bot's simulation-only mode is a feature, not just a
  development convenience.** A simulation-only bot can be
  operated in jurisdictions that would not allow a live bot. Read
  the README's warning carefully and treat it as binding.
- **The relay's `eth_callBundle` cross-check** is a real
  audit-trail: every bundle the bot would have submitted is
  visible in the relay's data, timestamped, with a simulated
  outcome. Treat it as a log you'd be willing to share, not one
  you'd be willing to lose.

## 7. The roadmap, in priority order

The existing [`ROADMAP.md`](ROADMAP.md) is the canonical source.
For the team taking this over, the practical order is:

1. **Read the funnel for a week.** Don't change any code yet.
   The funnel tells you where opportunities are dying, and that
   determines the next move. Note that until the fix in
   `docs/PHASE_2_HANDOFF.md` W1 lands, the first-stage counters
   are per *invocation* and the rest are per *opportunity*, so
   don't read a conversion rate across that boundary. If the
   funnel shows the strategies are doing their job, the next
   move is not "more strategies" but "more pools in the cache"
   or "faster mempool feed".

2. **Add pool discovery.** The `PairCreated` log scan in the
   sniper is the seed of this work. Pull it out into a shared
   module and add `PoolCreated` for V3. This is the single
   highest-leverage change for the existing strategies on
   mainnet.

3. **Add the multi-leg V2 arb.** The spec is in
   `docs/PHASE_2_HANDOFF.md` W4. The algorithm choice (direct
   cycle enumeration, not Bellman–Ford) is documented there with
   the trade-off and the search budgets. This is the
   strategy that has the most simulation signal on a typical
   mainnet day.

4. **Add the V3 sandwich trigger.** The spec is in
   `docs/PHASE_2_HANDOFF.md` W5. The approach (QuoterV2 for
   sizing, not hand-rolled Q64.96 math) is documented there with
   the trade-off and the RPC budget. This is the strategy that
   expands the bot's *visible* surface — it sees the large
   router-routed swaps that the existing V2-only sandwich
   ignores.

5. **Replay validation.** This is the Phase 1 work the roadmap
   calls for and is the prerequisite for going live. It is also
   the most research-intensive: re-simulate historical blocks
   from the bot's database against the actual builder payment
   data from the relay bid traces. The output is a
   "true-positive rate" for the simulation pipeline, which is
   the only number that matters when you flip
   `LIVE_EXECUTION=true`.

6. **New chains.** Only after the above. The roadmap's Phase 4
   order (Base → Arbitrum → BNB → Solana) is correct; Solana
   in particular is a separate engine entirely, not a
   configuration change.

## 8. Final note

The codebase is small on purpose. The total Rust is about 6,400
lines, the contracts are 300 lines of Solidity, the frontend is
~2,400 lines of TypeScript. The constraints of a small codebase
are good ones: every line is a line you have to defend, every
import is a dependency you have to vet, every ABI definition is a
truth claim about mainnet that you have to keep honest.

If you find yourself adding a hundred-line feature, step back and
ask whether the same outcome is achievable in twenty lines by
restructuring something that already exists. The architecture
diagram in [`ARCHITECTURE.md`](ARCHITECTURE.md) is the contract
that the bot's "smallness" is a property worth preserving.

If a maintainer six months from now opens the codebase and the
first thing they say is "where does this start?", something has
gone wrong. The answer should be: `bot/crates/mev-bot/src/main.rs`,
one function, ~150 lines, and from there the engine is in
`engine.rs` and the rest follows. Keep that property.

