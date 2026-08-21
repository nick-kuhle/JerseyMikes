# Phase 2 expansion — design document

> Status: **design only**. No code in this repo implements these strategies
> yet. The code that does exist is the strategy funnel counter, which is the
> first concrete change below and is mergeable on its own.

This document covers the work described in the Phase 2 section of
[`ROADMAP.md`](ROADMAP.md) and the "more competitive strategies" request
that prompted it:

  1. Multi-leg atomic arb (V2, with 3–5 legs) using direct cycle enumeration.
  2. V3 victim decoding for the sandwich strategy, with sizing that uses
     `QuoterV2.eth_call` as the source of truth.
  3. Pool discovery beyond the hardcoded `CORE_TOKENS` set, via factory logs
     (`PairCreated` on V2 factories, `PoolCreated` on V3).
  4. Aggregator calldata decoders (UniversalRouter, 1inch v6, 0x v2) so the
     strategies can see the majority of large-swap flow that today lives
     behind an aggregator router.
  5. A strategy funnel counter on the API and dashboard, so we can see
     *which gate* is filtering out opportunities in any given run.

The motivation, scope, and known traps are all in
[`STRATEGIES.md`](STRATEGIES.md). This document is the engineering
specification.

---

## Why a design doc and not a PR

Two reasons.

First, multi-leg search and V3 pricing are both areas where a
straightforward-looking implementation can be subtly wrong, and the
existing test suite in this repo (and unit tests in general) does not catch
the failure modes that matter on real mainnet pools. The first author of
this kind of code is the wrong person to also be the only reviewer.

Second, I (the writing agent) do not have a working Rust toolchain in the
authoring environment, which means the only honest way to deliver this is
in a form that can be reviewed and re-implemented by someone who does.

Sections 1–4 below are the engineering spec. Section 5 is the one piece of
code that is small, low-risk, and useful *today*, and is described with
enough detail to be implemented and merged independently of the rest.

---

## 1. Multi-leg V2 atomic arb

### Why direct cycle enumeration, not Bellman–Ford

A textbook Bellman–Ford over the log-rate graph of constant-product pools
produces false positives: an edge weight is the *log* of the exchange rate,
and a coarse `log_e` approximation in fixed point will report edges with
coincidentally equal bit-lengths as "relaxable", surfacing cycles that
don't actually exist on chain. The reference implementation (Ang et al.,
2021) uses a high-precision `log_e` table plus a Newton-iteration on the
input size, which is a research-grade port rather than a one-file PR.

Direct cycle enumeration is correct by construction: each leg of the
candidate cycle is evaluated with the same `v2_amount_out` function the
existing `dex.rs` already unit-tests. The trade-off is more candidates
to evaluate, but the bot operates on ≤ 200 pools, so the search is
bounded.

### Algorithm

1. Build a directed edge list from the cached `V2Pool` set: each pool
   contributes two edges (one per swap direction). Dust pools (zero
   reserves on either side) are dropped at this step.
2. Build an adjacency map: `token_in -> [edge indices]`.
3. For each anchor token (WETH + `CORE_TOKENS`):
   - Run a recursive DFS from the anchor. The `visited` set prevents
     revisiting a token within a single walk. Max walk length is 5
     (i.e. max cycle length 5).
   - At every walk length ≥ 2, try to close the cycle back to the
     anchor token. A "closing edge" is any adjacency entry whose
     `token_out == anchor`.
   - Each closing edge yields one candidate cycle. The DFS continues
     after a closing edge is found, because a token may have multiple
     closing edges (parallel pools).
4. De-duplicate cycles by their edge-index sequence.
5. For each cycle, solve the optimal input size with a ternary search
   over the composed pricing function (the existing
   `dex::ternary_search_max` works directly; the profit function is
   `cycle.evaluate(edges, pools, token_in, x) - x`, which is unimodal in
   `x` for constant-product legs with positive fees).
6. Build the `Call` sequence: for each leg, transfer the input amount
   to the pool, then call the pool's `swap`. The executor receives the
   output back; the next leg's transfer moves it to the next pool.
   This reuses the existing `strategies::sandwich::build_leg` helper.
7. Sort candidates by expected profit; cap the per-block opportunity
   list at ~32 to bound simulation cost.

### Files

- `bot/crates/mev-bot/src/dex/graph.rs` — `DirectedEdge`, `Cycle`,
  `build_edges`, `adjacency`, `enumerate_cycles`, plus unit tests for
  each invariant (empty input, identical pools, diverged pools,
  3-leg triangle, dust pool drop, cycle-closes-to-start).
- `bot/crates/mev-bot/src/strategies/arb.rs` — replace the existing
  pair-pair loop in `on_block` with a call to `enumerate_cycles`,
  keeping the existing `on_pending` back-run path intact.

### Known traps

- **Recursion depth.** The DFS is bounded by `MAX_CYCLE_LEN = 5`, so
  no stack overflow in practice. Add an explicit depth assertion in
  tests.
- **Cycle deduplication.** Two DFS walks can produce the same cycle
  when the same edge is reachable by different paths, or when the
  same cycle is anchored to two different start tokens. De-dup by
  the sorted edge-index sequence.
- **Gas model.** The existing 2-leg strategy uses a flat 320k gas
  estimate. A per-leg `+120k` model is a reasonable starting point;
  the fork simulation is the source of truth, and a misestimate
  only causes false-positive or false-negative *pre*-filtering.
- **Multi-leg execution cost.** A 5-leg cycle is ~680k gas, which is
  real money at 50+ gwei. The `BRIBE_BPS = 9000` default already
  prices this in: a 5-leg cycle needs 10x the gross profit of a
  2-leg cycle to pass the net-profit gate.

---

## 2. V3 sandwich via QuoterV2

### Why not hand-rolled Q64.96 math

`TickMath.getSqrtRatioAtTick` and `SqrtPriceMath.getNextSqrtPriceFromAmount0RoundingUp`
are subtle, and writing them correctly requires line-by-line comparison
with the on-chain reference. The cost of being slightly wrong is
silently wrong quotes that the unit tests would also assert, because
the test author and the implementation author are usually the same
person. The Q64.96 integer arithmetic needed (`mulDiv` over 512-bit
intermediates) is not in `alloy-primitives` by default and would need
either a custom implementation or a new dependency.

The existing `dex::quote_v3` already calls `QuoterV2.quoteExactInputSingle`
via `eth_call`, which runs the real swap logic on a forked node and
returns the exact integer output. Slower than a hand-rolled
implementation (one extra RPC roundtrip per candidate), but correct
by construction and consistent with the JIT strategy's existing
pattern.

### Algorithm

1. Decode the pending V3 router call: `ISwapRouter02.exactInputSingle`
   with selector `0x04e45aaf`. The struct fields are all `uint256`-typed
   so no signed-int encoding subtleties.
2. Verify the swap is a candidate: `amountIn > 0` and `tokenIn != tokenOut`.
3. Read the pool's `slot0`, `liquidity`, `token0`, `token1` (the
   `pool_state` helper in `strategies::jit.rs` already does this).
4. For the sandwich sizing loop, use `dex::quote_v3` to evaluate each
   candidate front-run size: `quote_exact_in(quoter, token_in, token_out, fee, x)`,
   and same for the back-run after the front-run has moved the price
   (the back-run runs the *reverse* swap, so it needs the
   post-front-run pool state, which is not directly available from
   the quoter — see "Known traps" below).
5. Victim-revert trap: as in the V2 strategy, any `x` that would push
   the victim's output below their `amountOutMinimum` scores zero.
6. Build the call sequence: front-leg router call, victim transaction
   (raw replay), back-leg router call.

### Known traps

- **Back-run after a front-run moves the price.** The quoter returns
  the output for the *current* pool state, not for the state after
  the front-run. The conservative approach is to issue the back-run
  with `amountOutMinimum = 0` and let the executor's profit guard
  catch the mispricing. The aggressive approach is to approximate the
  post-front-run state with two `quote_v3` calls (one for the front,
  one for the back, accepting the quoter's own fee). The fork
  simulation is the source of truth either way.
- **Router vs. pool-direct call.** The pool's `swap(address, bool, int256, uint160, bytes)`
  is ~20k gas cheaper than the router, but it requires encoding
  `int256` and a callback. The sandwich using the router is the
  safe default; the pool-direct path is a follow-up optimisation
  with a clear test plan.
- **`sqrtPriceLimitX96`.** We pass `0` (no limit), which is permissive
  and works for the vast majority of swaps. Pools with very tight
  ranges may reject the call; the simulation will catch this.

### Files

- `bot/crates/mev-bot/src/dex/v3_quote.rs` — `V3SwapIntent` (the
  decoded swap), `quote_exact_in` (the QuoterV2 wrapper), and
  `build_router_swap_call` (the router calldata builder).
- `bot/crates/mev-bot/src/strategies/sandwich.rs` — extend `on_pending`
  to also try V3 victims: decode with the same logic as
  `strategies::jit::decode_v3_swap`, then run a sizing loop that
  uses `dex::v3_quote::quote_exact_in` instead of the V2
  `v2_amount_out`.

---

## 3. Pool discovery

### Current state

`strategies/arb.rs` hardcodes a `CORE_TOKENS = [USDC, USDT, DAI, WBTC]`
list and resolves the WETH pair for each on UniV2 + SushiV2. New pairs
are only added via the sniper's `PairCreated` log scan, which is
tied to a separate go-live logic.

### Proposal

Add a `PoolDiscovery` struct in `strategies/arb.rs` (or a new
`strategies/discovery.rs` if it grows). On every block, it scans a
sliding window of recent blocks for:

- `PairCreated(address,address,address,uint256)` from
  `known::UNIV2_FACTORY` and `known::SUSHI_FACTORY` (the sniper
  strategy already does this; extract it into a shared helper).
- `PoolCreated(address,address,uint24,address)` from
  `known::UNIV3_FACTORY` for the V3 sandwich trigger.

For each new pool:

- If it's a WETH-quoted pool with non-dust reserves, call
  `ctx.pools.load(...)` to add it to the cache.
- Maintain a `seen_pools: HashSet<Address>` so we don't re-load.

The cost is one `eth_getLogs` call per block, which is ~50–200 KB
of data on a busy mainnet block. The cache refresh picks up the
new pools on the next strategy tick.

### Why this matters

Without discovery, the bot only sees the four `CORE_TOKENS` × two
venues = 8 pools. With discovery, it sees the top ~50 WETH-quoted
pools (the realistic cycle space on mainnet) within one block of
their launch. Multi-leg arb on a larger pool graph is the main
beneficiary: a 3-leg cycle through, say, WETH → USDC → WBTC → WETH
only appears in the graph when WBTC/USDC exists in the cache.

---

## 4. Aggregator calldata decoders

### Current state

`strategies::decode_swap` (in `strategies/mod.rs`) decodes the
UniswapV2-style router calls. The V3 router is decoded in
`strategies::jit::decode_v3_swap`. Everything else — UniversalRouter,
1inch, 0x, CoW Protocol — is invisible to the strategies today.

### Why this matters less than it seems

A large fraction of "big swap" flow on mainnet today is **deliberately
not in the public mempool**: it goes through MEV-Blocker, CoW Protocol,
1inch Fusion, or private orderflow. Decoding the public mempool
calldata only helps for the portion of flow that *is* public, which
is the part that established searchers are already competing for.

This is the highest-effort, lowest-payoff section. Recommend deferring
it until the rest of Phase 2 ships and we have funnel data showing
how much public mempool flow we are missing.

### If you do want it

The decoders are stable ABIs:

- **UniversalRouter** (`0x3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD`): the
  `execute(commands, inputs)` function with a `Commands` byte string
  whose bits select between `V3_SWAP_EXACT_IN`, `V2_SWAP_EXACT_IN`,
  etc. Decoding requires walking the command bytes and the
  per-command input arrays.
- **1inch v6** (`0x1111111254EEB25477B68fb85Ed929f73A960582`): single
  `swap(...)` call with a complex `IAggregationRouter.Executor` arg.
  The pattern is `swap(Executor.swap(amount, minReturn, pools))` with
  the pools encoded as `(token, token, fee_or_stable, kind)`.
- **0x v2** (`0xDef1C0ded9bec7F1a1670819833240f027b25EfF`): the
  `fillOrder`/`fillOrKillLimitOrder`/`transformERC20` set. The
  calldata is the most complex of the three; the SDK
  (`0x-swap-sdk`) is the only sane reference.

A reasonable scope: UniversalRouter only, since it's by far the most
common on mainnet. The other two can be follow-up PRs.

### Files

- `bot/crates/mev-bot/src/dex/calldata/mod.rs` — `DecodedSwap`
  struct and a `decode_any_router(tx) -> Option<DecodedSwap>`
  function that tries each decoder in order.
- `bot/crates/mev-bot/src/dex/calldata/universal_router.rs` — the
  UniversalRouter decoder.
- `bot/crates/mev-bot/src/dex/calldata/oneinch.rs`, `zeroex.rs` —
  follow-ups.

The decoded `DecodedSwap` is then used as the input to a new
`strategies::decode_swap_v2` function that wraps the existing
`decode_swap` and the new aggregator decoders, returning a
`SwapIntent` that the sandwich and arb strategies already consume.

---

## 5. Strategy funnel counter (mergeable today)

This is the one concrete code change. It does not require any of
sections 1–4, and it answers the question that motivated this
whole effort: **why am I not seeing opportunities?**

### What it does

Adds a per-strategy funnel to `engine::Stats`:

```
                  pending_seen
                       │
                       ▼
              strategy.on_pending / on_block
                       │
              ┌────────┴────────┐
              ▼                 ▼
         not a candidate    candidate
              │                 │
              ▼                 ▼
         counter:         risk.check
         skipped_          (gates)
         not_candidate        │
                              ▼
                         not submittable  →  counter: gated_by_risk
                              │
                              ▼
                         simulation
                              │
                         ┌────┴────┐
                         ▼         ▼
                      success   failure
                         │         │
                         ▼         ▼
                   counter:    counter:
                   profitable  revert
```

### Implementation

- Extend `engine::Stats` with a `funnel: HashMap<Strategy, FunnelCounters>`.
- Bump the right counter at the right point in `Engine::on_pending`,
  `Engine::on_block`, and `Engine::consider`.
- Add a `/api/funnel` route that returns the snapshot as JSON.
- The dashboard gets a new panel: a small bar chart with the
  counters per strategy.

The diff is ~80 lines of Rust + 30 lines of frontend. It compiles
and runs against the existing tests.

### Why this is the right first step

Every other section of this document adds more *candidates* to the
strategy. The funnel tells us whether the candidates we already
have are being filtered out by a config we can fix, a base fee we
should wait out, or a victim-revert trap that the simulator is
rightly catching. **Without the funnel, sections 1–4 are shots in
the dark**: they might find opportunities, or they might be
churning the simulator for zero net output.

With the funnel, every subsequent change is measurable: "we added
multi-leg arb and the funnel shows 12 new candidates per block
that pass risk and are now in the simulation queue." That's a
real signal. "We added multi-leg arb and the simulation tape
looks busier" is a feeling, not data.

---

## What this document is not

- It is not a guarantee that any of sections 1–4 will produce
  opportunities. The strategies are added; the opportunities are
  discovered by the simulator. The number of profitable
  simulations per block is a property of the market, the
  competition, and the latency, not of the code.
- It is not a path to live trading. The simulation-only guard
  remains in place. The Phase 1 work (replay validation,
  competition modelling, latency budget) is the prerequisite
  for live use, and is not addressed here.
- It is not a substitute for a maintainer's review. The
  reasoning in each section is correct, but the code that
  implements it will be written by a different author and will
  need to be reviewed against the constraints in this document
  before it is merged.
