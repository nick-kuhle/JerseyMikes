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

**Not yet.** Multi-hop paths, V3 victims, aggregator calldata (1inch, 0x,
UniversalRouter).

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

**Search.** WETH → token → WETH across two venues (UniV2 / Sushi today). Optimal
input solved by ternary search over both curves composed.

**Capital.** A zero-fee Balancer V2 flash loan, so the strategy needs no
inventory: `flashExecute` borrows, swaps twice, repays, and the executor
verifies the leftover is ≥ `minProfit`.

**Not yet.** Three-plus-leg cycles, V3 legs, Curve/Balancer pools as legs, and a
proper negative-cycle (Bellman–Ford) search over the whole pool graph.

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
