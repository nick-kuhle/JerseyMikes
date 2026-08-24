# Strategies

All strategies run concurrently and all are gated by the same rule: the bundle
must be net-positive in a forked simulation, and on chain `MevExecutor` reverts
anything that does not clear `minProfit`.

## Live-eligible vs shadow-only

A strategy is a **live candidate** when it has the engineering properties a
submitted bundle needs: atomic settlement into a profit token that can be
valued, and an executor-enforced retained-profit guard. Eligibility is not
approval — a live candidate still cannot broadcast until it earns its own
`PASS` in `GET /api/qualification` (`SIM_TO_LIVE.md`).

| Strategy | Status | Note |
| --- | --- | --- |
| `sandwich`, `sandwich_v3` | live candidate | Requires victim raw bytes; economics are chain-dependent (see below) |
| `atomic_arb` | live candidate | Back-running; the most portable row across chains |
| `liquidation` (Aave V3) | live candidate | Promoted once collateral could be valued |
| `liquidation_compound` | live candidate | ditto |
| `liquidation_morpho` | live candidate | ditto |
| `liquidation_maker` | live candidate | ditto |
| `jit` | shadow-only | Settlement shape |
| `sniper` (atomic probe) | shadow-only | Settlement shape |
| `sniper` (directional lane) | separate lane | Not an atomic strategy at all — see [`SNIPER.md`](SNIPER.md) |
| `oracle_frontrun` | shadow-only | Ordering assumption |

`Strategy::shadow_only_reason()` carries the specific reason for each
shadow-only row and it is reported on the API row, so a strategy is never
silently excluded.

**Why the liquidations were promoted.** They settle in seized collateral, not
ETH. The simulator accounts by balance delta, so before `valuation.rs` existed
every liquidation netted to zero and could not clear `MIN_NET_PROFIT_WEI` — the
math was correct and tested, but structurally unable to produce a bid. With the
profit token priced at the pinned pre-bundle fork block (V3 QuoterV2 → V2
reserves → fail closed, less `VALUATION_HAIRCUT_BPS`; see `RISK.md`), the
blocker is gone. Pricing does **not** rescue JIT, the sniper, or oracle
front-running: their limitations are settlement- and ordering-shaped, not
valuation-shaped.

**Where the value actually is.** Worth being blunt, because it shapes how to
read the funnel. Sandwiching depends on a transparent mempool and an open
builder market that can place three transactions atomically. On private-mempool
rollups neither condition holds: inclusion is probabilistic rather than atomic,
and measured sandwich attempts there are overwhelmingly unprofitable. The
strategies that survive contact with current market structure — especially on
L2 — are the **back-running** ones: atomic arbitrage and liquidations. The
sandwich rows remain implemented, tested and eligible, but expect the
liquidation and arb rows to carry the funnel on any chain without a public
mempool.

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

## 4. Liquidations (Aave V3) — `strategies/liquidation.rs`

**Discovery.** `Borrow` logs from the Aave V3 pool build a borrower watchlist;
every block their `getUserAccountData` is polled in batches of 100.

**Trigger.** `healthFactor < 1e18`.

**Per-reserve composition.** The old USDC-debt/WETH-collateral/5%-bonus
assumption is gone. For each unhealthy user the strategy reads the position's
*actual* composition — `Pool.getUserConfiguration` (the borrowing/collateral
bitmap over `getReservesList`), batched `DataProvider.getUserReserveData`
for the reserves the bitmap says the user touches (bounded to 8), and
`getReserveConfigurationData` for the real liquidation bonus (cached per
block). All four ABI shapes verified against the live pool implementation
(`0x728a138A…`) and data provider. The bundle pairs the largest debt against
the largest collateral; the seized estimate uses the reserve's actual bonus.

**Shape.** Flash-borrow the *actual* debt asset → `liquidationCall` (never
`receiveAToken`, we want the underlying) → swap the *actual* collateral back
→ repay. Close factor stays the HF-based simplification (50%, or 100% when
HF < 0.95; v3.1 has a per-reserve close factor) — the simulation corrects
any residue. Near-miss leads publish the position's real collateral, so
the oracle front-runner matches on the right feed.

**Near-miss publication.** Positions with HF in `[1.00, 1.05)` are published
into the shared `LiquidationLeads` registry (`strategies/leads.rs`, see §4e)
every block, with the exact builder inputs to rebuild the liquidation later.
This costs nothing extra — the health numbers were computed anyway — and it
is what lets the oracle front-runner act in the same block as the price
change instead of one block later.

## 4b. Compound V3 liquidation — `strategies/liquidation_compound.rs`

**Toggle.** `STRATEGY_LIQUIDATION_COMPOUND` (default **on**).

**Trigger.** `isLiquidatable(account)` — a boolean view Comet exposes exactly
for this. Accounts are harvested from `Supply`/`Withdraw` logs (capped at
`LIQUIDATION_WATCH_CAP`, least-recently-active evicted first), so the poll is
one batched sweep, and only the (rare) liquidatable ones pay the deeper
per-asset reads.

**Shape.** Comet is a two-step storefront, not a single `liquidate` call:

```
flash USDC → approve Comet → absorb(executor, [account])
          → buyCollateral(asset, minAmount, baseAmount, executor)  (per asset)
          → swap collateral back to USDC → repay
```

`absorb` moves the account's collateral into protocol reserves (the absorber
receives nothing yet); `buyCollateral` buys it back **at a discount** —
`quoteCollateral` prices it with `storeFrontPriceFactor × (1 −
liquidationFactor)` — and the discount *is* the liquidation reward. Doing the
two calls in one batch matters: `buyCollateral` reverts `NotForSale` while
reserves are healthy, and the absorb is usually what pushes them below
`targetReserves`.

**Sizing.** Per asset, `quoteCollateral(asset, 1e9)` once per block gives the
discounted collateral-per-base rate; `base_needed = ⌈seized · 1e9 / rate⌉`,
seized read from `userCollateral`. The `minAmount` bound is 97% of the
seizure (3% slippage cap).

**Verified against the chain.** Every selector (`absorb` `0xc3cecfd2`,
`buyCollateral` `0xe4e6e779`, `isLiquidatable`, `quoteCollateral`,
`userCollateral`, `getAssetInfo`, `numAssets`) was found in the dispatcher of
the *live implementation* behind the 0xc3d688B6… proxy
(`CometWithExtendedAssetList`), and the `AssetInfo` decode offsets were
checked against real returns (asset[2] reads back as WETH, asset[1] as WBTC,
13 assets listed).

**Not yet.** Multi-account absorbs in one bundle; Comet markets on other
bases; a near-miss band (Comet exposes no continuous health factor, so
oracle front-running cannot use it — see §4e).

## 4c. Morpho Blue liquidation — `strategies/liquidation_morpho.rs`

**Toggle.** `STRATEGY_LIQUIDATION_MORPHO` (default **on**).

**Discovery.** Markets are self-seeding: activity logs (`Supply`,
`SupplyCollateral`, and `Borrow` — *both* the current six-field signature and
the pre-v1.1 five-field one, OR'd in one `eth_getLogs`) reveal which market
ids are being used *now*, which is the population worth watching; borrowers
come from the same logs. Params are resolved per id via `idToMarketParams`
and markets are filtered to a whitelist the swap leg can actually price
(USDC/DAI/USDT/WETH loans against WETH/WBTC/wstETH collateral). Caps:
`MORPHO_MARKET_CAP` markets, `MORPHO_BORROWER_CAP` borrowers per market.

**Trigger.** The health check mirrors the deployed contract exactly:
`borrowed = ⌈shares · (tba+1) / (tbs+1e6)⌉` (the virtual-share rounding
included) and `maxBorrow = collateral · price/1e36 · lltv`; liquidatable when
`maxBorrow < borrowed`. Two batched reads per position (`position`,
`market`) plus one oracle `price()` per market.

**Shape.** Full close, repay-by-shares:

```
flash loan token → approve Blue → liquidate(params, borrower, 0, borrowShares, "")
                 → swap seized collateral back → repay
```

The reward is `min(1.15e18, 1 / (1 − 0.3 · (1 − lltv)))` — lltv-proportional,
~2.6% at 0.915 lltv, capping at 15% for the deepest-risk markets.

**The v1.1 interface.** The deployed singleton at 0xBBBB…FFCb exposes
`liquidate` (not the 2024 `liquidationCall` — no `id` argument; the id is
re-derived from the market params), `position(bytes32,address)` for reads,
and `MarketParams` ordered `(loan, collateral, oracle, irm, lltv)`. The
contract is immutable, so this ABI is frozen on mainnet; all four selectors
were verified against its bytecode dispatcher before implementation.

**Not yet.** Partial liquidations sized to pool depth; MetaMorpho vaults;
seized-collateral oracles that need historical rounds.

## 4d. Maker liquidation — `strategies/liquidation_maker.rs`

**Toggle.** `STRATEGY_LIQUIDATION_MAKER` (default **on**, `MAKER_ILKS=ETH-A`).

**Discovery.** The Vat emits **nothing** (verified live — two independent
RPCs return zero `frob` notes; the source dropped LibNote), so urns are
harvested from each ilk's **gem join**: the joins emit *anonymous* DSNote
LogNotes — topic0 is the padded `join`/`exit` selector, and topics[2] carries
the `usr` argument (the urn, usually the owner's proxy). One `eth_getLogs`
per ilk per block; layout pinned against live ETH-A join logs. Polled with
batched `vat.urns` + `vat.ilks`: unsafe ⇔ `ink · spot < art · rate`.

**Shape.** Maker liquidates through auctions, and the bundle makes the
auction atomic:

```
flash DAI → daiJoin.join → vat.hope(daiJoin)
          → dog.bark(ilk, urn, executor)          (kick reward mints to the executor)
          → clip.take(kicks+1, MAX, marketPrice, executor, "")
          → gemJoin.exit(slice)                    (ERC20 WETH out)
          → swap WETH → DAI
          → daiJoin.exit(leftover vat.dai) → repay
```

Profit = kick reward (`tip + tab·chip`) plus the spread between the auction's
opening price and the market. Both the reward and the opening price
(`getFeedPrice() × buf`) are public reads, so the whole bundle is sized with
exact integers — including the take's own `slice = min(lot, tab/price)` floor.

**The deterministic-id trick.** A `Call[]` batch cannot thread bark's return
value into take. It does not need to: the auction id is `++kicks`, and
`clip.kicks()` is public — the bundle hardcodes `kicks + 1`. If another
searcher barks in between, the take targets a dead auction and reverts; the
bundle dies, nothing is broadcast. Fail-safe is the correct polarity.

**Price cap.** `max` is a ray price (DAI per 1e18 collateral). We bid exactly
the V2 pool price (`dai_reserve · 1e27 / weth_reserve`): if the auction opens
above market, `take` reverts `too-expensive` (correct — not a discount yet);
if below, the spread is realised on the swap leg. Both directions fail safe,
which is why a pure market cap beats any off-chain "fair value".

**Addresses.** Resolved from the live Maker chainlog — notably `MCD_DOG` is
*not* the pre-2024 address (the Dog was replaced in the Sky-era upgrades);
`dog.ilks(ilk)` supplies the clip at runtime so a clip swap never goes stale.
The built-in ilk table (ETH-A, WBTC-A, WSTETH-A) holds the stable adapter
addresses (gem joins, OSM pips), each entry cross-checked against the
chainlog.

**Not yet.** Auctions needing a reset (`list.max` staleness), multi-ilk
bundles, ETH-C/WSTETH-B beyond the table.

## 4e. Oracle-update front-running — `strategies/oracle_frontrun.rs`

**Toggle.** `STRATEGY_ORACLE_FRONTRUN` (default **on**).

The other liquidation strategies watch *state* and react a block late. This
one watches the *event that changes state*: a downward collateral price
update flips every position priced at the stale higher value, and the first
searcher behind that transaction in the block captures the liquidation.

**Watched update paths.**

| Source | Transaction shape | Notes |
| --- | --- | --- |
| Chainlink OCR2 | `transmit(bytes)` to the **aggregator** | the proxy is what protocols read; the aggregator (what `transmit` actually hits) is resolved via `proxy.aggregator()` at runtime and refreshed every ~50 blocks |
| Chainlink legacy | `submit(uint256,int256,uint256,uint256,address)` | same target set |
| Maker OSM | `poke()` to the ilk's pip; `poke(bytes32)` via OsmMom | the pip reprices the ilk's spot an hour later |
| Maker Spot | `poke(bytes32)` | reprices every ilk at once |

**Trigger.** A pending transaction matching (target, selector) above. The
affected collateral is looked up in the shared `LiquidationLeads` registry
(`strategies/leads.rs`): every block, the Aave / Morpho / Maker strategies
publish positions within 5% above their liquidation threshold, normalised to
bps of the threshold, together with everything needed to rebuild their
liquidation. On a match, up to `ORACLE_FRONTRUN_MAX_LEADS` leads are rebuilt
— each with the owning strategy's own builder, so protocol logic lives in
exactly one place — and emitted as **back-run bundles**: the oracle
transaction is the victim, our calls run directly behind it.

**Why this is honest.** The new price is *not* decoded out of the OCR report
(offchain-consensus bytes, drifting layouts); the fork simulation replays
victim → back and decides. Upward or too-small updates revert the liquidation
(`HEALTHY_POSITION` on Morpho, `not-unsafe` on Maker), the bundle dies, and
private orderflow means nothing is broadcast. The strategy measures how often
the pattern exists before anyone cares about winning it — Chainlink
`transmit`s are only back-runnable when they land in the public mempool.

**Not yet.** Redstone/Api3 oracle families, OCR proposal/aggregation rounds,
the Maker medianizer itself (an hour ahead of the OSM — needs addresses the
operator trusts), decoding the upcoming price pre-simulation.

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

**Not yet.** Simulating the token's `transfer` hooks for blacklist/cooldown
logic, and liquidity-lock checks.

**Holding a position across blocks is no longer out of scope — but it is not
this strategy.** The directional sniper is a *separate lane* with its own
contract, risk envelope, storage and console panel, precisely because holding
inventory breaks the atomic profit-or-revert invariant every strategy on this
page depends on. The probe above feeds it: its verdict is the directional
lane's honeypot admission gate. See [`SNIPER.md`](SNIPER.md).

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

The same rule applies to the liquidation rows (`liquidation` (Aave),
`liquidation_compound`, `liquidation_morpho`, `liquidation_maker`) and
`oracle_frontrun`: different victim populations, different reward shapes
(Aave bonus bps, Comet storefront discount, Morpho lltv-proportional
incentive, Maker kick reward + auction spread) and different revert modes.
They share the leads registry but nothing else; `oracle_frontrun` candidates
are back-runs measured against a victim, the four block-cadence rows are
standalone. Read each row's `candidatesEmitted → submittable` on its own.

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
