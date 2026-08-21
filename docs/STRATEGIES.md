# Strategies

All five run concurrently and all five are gated by the same rule: the bundle
must be net-positive in a forked simulation, and on chain `MevExecutor` reverts
anything that does not clear `minProfit`.

## 1. Sandwich — `strategies/sandwich.rs`

**Trigger.** A UniswapV2-style router swap in the mempool
(`swapExactTokensForTokens`, `swapExactETHForTokens`, `swapExactTokensForETH`),
single hop, WETH on the input side.

**Sizing.** Profit as a function of front-run size `x` is unimodal on a
constant-product curve, so `dex::optimal_sandwich_in` ternary-searches the exact
integer pricing function (the same arithmetic the pool executes) rather than
using a floating-point closed form. Three legs are modelled: our buy, the
victim's swap against the worsened price, our sell against the twice-moved
price.

**The victim-revert trap.** If our front-run pushes the price past the victim's
`amountOutMin`, their transaction reverts, the price never moves further, and we
are left holding inventory we bought at a bad price. The sizing function scores
any such `x` as zero profit, so the search never selects one. Covered by
`sandwich_respects_victim_slippage_bound`.

**Legs.** We trade against the pair directly (`transfer` + `swap`) rather than
through the router: ~40k gas cheaper, and the router's slippage checks are
redundant when the executor enforces profit atomically.

**V3 victims** live in a separate strategy (`sandwich_v3`, see below) so the
funnel can tell the two surfaces apart.

**Not yet.** Multi-hop paths. Aggregator calldata other than UniversalRouter
(1inch v6, 0x v2) — UniversalRouter decoding is implemented behind
`DECODE_UNIVERSAL_ROUTER` (default off); see §7.

## 1b. V3 sandwich — `strategies/sandwich_v3.rs`

**Trigger.** An `exactInputSingle` on SwapRouter (`0x414bf389`) or
SwapRouter02 (`0x04e45aaf`) in the mempool, WETH on the input side, whose
`(token, fee)` pool is already in the V3 cache.

**Toggle.** `STRATEGY_SANDWICH_V3` (default **on**, paired with
`POOL_DISCOVERY_V3`). The strategy is not constructed when the toggle is
off, so it adds zero RPC to the pending path. It also no-ops if
`POOL_DISCOVERY_V3` is off: the cache is empty, the pre-filter returns
before any quote, and the engine logs a warning at boot.

**Sizing.** QuoterV2, not hand-rolled Q64.96. A coarse grid of four front-run
sizes, then a one-step refine, each size costing two `eth_call`s
(`quote(x)` and `quote(x + victim)`). Hard cap of 12 quotes per candidate
and 4 candidates per pending tx. A naive ternary search over the quoter
would be ~120 calls and would get the bot rate-limited off its provider.

**The victim-revert trap.** Same rule as V2: any size whose implied victim
output (`quote(x + victim) − quote(x)`) falls below `amountOutMinimum`
scores zero. Covered by `a_zero_slippage_victim_produces_nothing`.

**Legs.** Routed through SwapRouter02 (`approve` + `exactInputSingle`),
`sqrtPriceLimitX96 = 0`. The back-run is issued with `amountOutMinimum = 0`
— the quoter prices current state, not post-front-run state, so we refuse
to invent a min-out and let the executor's profit guard catch a misprice.
The two-quote post-state approximation is a follow-up.

**Not yet.** Pool-direct swaps (saves ~20k gas, needs a callback), quoting
the back-run against post-victim state, multi-hop V3 victims.

## 2. JIT liquidity — `strategies/jit.rs`

**Trigger.** A large `exactInputSingle` on the V3 router (≥ 20 WETH by default).

**Shape.** `armV3Callback` → `pool.mint(...)` in front of the victim,
`pool.burn(...)` + `pool.collect(...)` behind it. We use pool-level positions
(keyed by owner + tick range) instead of the NFT position manager, because a
call batch cannot thread a returned `tokenId` from one call into the next.
`MevExecutor.uniswapV3MintCallback` pays the pool, and is armed for exactly one
pool address, for exactly one call, via transient storage.

**Sizing.** One tick-spacing either side of the current tick; liquidity derived
from `L = amount / (√Pb − √Pa)` in f64. Approximate by design — the fork
simulation is the arbiter and the on-chain guard makes a mis-size a no-op.

**Expected value.** `victim volume × pool fee × our share of in-range
liquidity`, minus ~500k gas.

**Not yet.** Multi-range positions, tick-crossing awareness (if the swap pushes
price out of our range we stop earning mid-swap), and the exact TickMath
integer path.

## 3. Atomic arbitrage — `strategies/arb.rs`

**Trigger.** Every block (cyclic scan) and every large mempool swap (back-run
against the state the victim will leave behind).

**Search.** Cycle enumeration over the cached pool graph (`dex/graph.rs`),
anchored on WETH: every simple cycle up to `ARB_MAX_CYCLE_LEN` legs that starts
and ends in WETH, with each pool used at most once. Optimal input is solved by
ternary search over the composed curves.

`ARB_MAX_CYCLE_LEN` defaults to **3**, the first post-funnel-week raise.
Set it back to 2 to reproduce the original two-venue WETH → token → WETH
scan. 3-leg cycles such as WETH → USDC → WBTC → WETH only exist once pool
discovery has loaded the cross pairs. Raise to 4–5 only after live
`atomic_arb.candidatesEmitted` on the same feed moves at 3. The search is
bounded on four axes regardless of configuration: 5 legs, 200 pools, 32
candidates and a 25 ms wall-clock budget per block, and it makes no RPC
calls of its own.

Anchoring on WETH alone is not a limitation: any cycle touching WETH can be
rotated to start there. Cycles that never touch WETH are skipped on purpose —
their profit is denominated in a token the gas model cannot price.

**Capital.** A zero-fee Balancer V2 flash loan, so the strategy needs no
inventory: `flashExecute` borrows, swaps twice, repays, and the executor
verifies the leftover is ≥ `minProfit`.

**Not yet.** V3 legs in the cycle search, Curve/Balancer pools as legs. A negative-cycle
(Bellman–Ford) search was considered and rejected: on log-rate weights a
fixed-point `log_e` cheap enough to run per block reports false cycles, and the
precise version is a research port. See `docs/PHASE_2_HANDOFF.md` W4.

## 4. Liquidation — `strategies/liquidation.rs`

**Discovery.** `Borrow` logs from the Aave V3 pool build a borrower watchlist;
every block their `getUserAccountData` is polled in batches of 100.

**Trigger.** `healthFactor < 1e18`.

**Shape.** Flash-borrow the debt asset → `liquidationCall` (never
`receiveAToken`, we want the underlying) → swap the seized collateral back →
repay. Close factor is 50%, or 100% when HF < 0.95.

**Not yet.** Compound V3, Morpho, Maker; oracle-update front-running (the real
edge — being first in the block where the price update lands); per-reserve
liquidation bonus lookup instead of the 5% assumption.

## 5. New-token sniper — `strategies/sniper.rs`

**Trigger.** `PairCreated` logs, plus mempool transactions carrying the
selectors that make a token tradable (`addLiquidityETH`, `openTrading`,
`enableTrading`, `removeLimits`, …).

**Probe.** An atomic **buy → sell round trip** in a single batch. This is both
the entry and the safety check:

| round trip returns | verdict |
| --- | --- |
| nothing / < 50% | honeypot — token is blacklisted |
| 50–98% | transfer tax |
| 98–100% | clean (just the 2 × 30 bps AMM fee) |
| > 100% | genuinely mispriced launch — profitable |

Only the last case is net-positive, which is exactly the case the executor lets
through. Everything else is recorded as a rejected observation, which is the
point: the dashboard shows how much of new-token flow is a trap.

**Not yet.** Holding a position across blocks (this build is atomic-only by
design), simulating the token's `transfer` hooks for blacklist/cooldown logic,
and liquidity-lock checks.

## 6. Reading the funnel

Every strategy reports into the per-strategy funnel on `/api/funnel` and the
dashboard panel. **Two units live in it and must not be divided into each
other:**

| Counter | Unit | Meaning |
| --- | --- | --- |
| `invocationsWithOutput` | strategy calls | calls that produced ≥ 1 opportunity |
| `invocationsEmpty` | strategy calls | calls that produced none |
| `candidatesEmitted` | opportunities | total opportunities built (sum of `opps.len()`) |
| `gatedByRisk`, `missingVictimRaw`, `simulations*`, `submittable` | opportunities | the rest of the per-opportunity funnel |

A single call can emit many candidates — that is exactly what widening a search
(multi-leg arb, V3 victims) does — so `candidatesEmitted / invocationsWithOutput`
is a search-width signal, while `submittable / candidatesEmitted` is the
conversion rate that matters.

V2 and V3 sandwiches are **separate funnel rows** (`sandwich` vs
`sandwich_v3`). Do not add them together and call it "the sandwich conversion
rate" — they have different RPC costs, different victim populations and
different revert modes.

## 7. UniversalRouter decoding — `dex/calldata/universal_router.rs`

**Toggle.** `DECODE_UNIVERSAL_ROUTER` (default **off**). When off,
`decode_router` is exactly the existing V2-router decoder, so a default
checkout is behaviour-identical to the funnel baseline.

**Scope.** The single mainnet UniversalRouter at
`0x3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD`. Walks `execute(commands, inputs)`
(and the deadline overload) and emits a `SwapIntent` for the first
`V3_SWAP_EXACT_IN` or `V2_SWAP_EXACT_IN`. `WRAP_ETH` immediately before a
zero-amount swap is treated as native in. 1inch v6 and 0x v2 are out of
scope.

**Cost.** Pure calldata parsing, well under 1 ms per pending tx. No RPC.

The funnel is also split by **provenance**. `stats.funnel` counts flow the bot
could have acted on; `stats.funnelReplay` counts already-mined transactions
replayed from bloXroute delivered blocks (`docs/BLOXROUTE_RELAY.md`). Never
read a rate across the two: the replay population is roughly 150 transactions
per block that were never winnable in real time, and folding it in would bury
the live signal completely. Before this split the first stage counted calls
and everything after it counted opportunities, which made the implied
conversion rate meaningless.
