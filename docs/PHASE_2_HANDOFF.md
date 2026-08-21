# Phase 2 — Engineering Work Order

**Audience:** the dev teams picking up Phase 2 of JerseyMikes.
**Status of this document:** it is the single source of truth for Phase 2. It
replaces and consolidates the previous `PHASE_2_DESIGN.md`, `PHASE_2_REVIEW.md`
and `MAINTAINING.md` (all three deleted; recoverable from git history at commit
`e62bbc2` if you need the originals).
**Scope:** everything below is simulation-only work. Nothing in Phase 2 touches
the live-execution path.

---

## 0. Read this first (10 minutes)

Three things about this codebase that will save you a week each.

**The bot is a measurement instrument, not a money printer.** Every gate in the
pipeline is deliberately conservative. "Few opportunities" is the designed
steady state, not a bug. If the funnel reads all zeros, the fix is almost never
to lower `MIN_NET_PROFIT_WEI` — it is that the mempool feed is quiet, the pool
cache is empty, or the RPC does not support `eth_getRawTransactionByHash`.
Loosening a gate just produces more simulations that revert.

**Measure before you expand.** Phase 2 has one hard ordering rule: the funnel
must be correct and read for a full week before any new strategy surface is
added. Every workstream below is instrumented so its effect on the funnel is
observable. "The simulation tape looks busier" is a feeling; "multi-leg arb
added 12 candidates/block that pass risk" is data.

**Nothing broadcasts.** `live_execution` is a two-key switch and stays off for
all of Phase 2. Phase 1 (replay validation, competition modelling, latency
budget) is the prerequisite for going live and is explicitly *not* in scope
here. Do not touch `LIVE_EXECUTION`, `BRIBE_BPS`, or the profit guard in
`MevExecutor.sol` as part of this work.

---

## 1. Verified current state

This section was checked against the checkout at `e62bbc2`, not against the
prior design docs. Treat it as ground truth.

| Area | State | Evidence |
| --- | --- | --- |
| Funnel counters | **Shipped, semantics wrong** | `engine.rs:40-105` (`Stats::funnel`, `FunnelCounters`, `record_funnel`) |
| `/api/funnel` route | **Shipped** | `api.rs:40,159` |
| Dashboard funnel panel | **Shipped** | `frontend/components/FunnelPanel.tsx` |
| V2 pool discovery | **Shipped, thin test coverage** | `strategies/discovery.rs`, wired at `engine.rs:280-288`, toggle `POOL_DISCOVERY` |
| Shared `PairCreated` scan | **Shipped** | `strategies/mod.rs:283` (`scan_pair_created`) |
| V3 pool discovery (`PoolCreated`) | **Not started** | no `PoolCreated` topic anywhere in the crate |
| Multi-leg arb (3–5 legs) | **Not started** | `strategies/arb.rs:48-62` is still the O(n²) pair-pair loop |
| V3 sandwich sizing | **Not started** | `strategies/sandwich.rs` is V2-only; `dex::quote_v3` exists and is unused by it |
| Aggregator decoding | **Not started** | `strategies::decode_swap` handles V2 routers; `jit::decode_v3_swap` handles `ISwapRouter02` |
| Rust build/test verification | **Never run** | no Rust toolchain in the authoring environment; 51 `#[test]`/`#[tokio::test]` blocks exist but have not been executed |
| CI | **Written, not enabled** | `ci/github-actions-ci.yml` is parked outside `.github/workflows/` |

The single most important line in that table is the second-to-last one: **the
Rust in this repo has never been compiled or tested by CI.** W0 exists because
of it, and it blocks everything else.

---

## 2. Workstream summary

| ID | Workstream | Depends on | Size | Gate to start |
| --- | --- | --- | --- | --- |
| **W0** | Enable CI; get a green `cargo check` / `cargo test` / `forge test` | — | S | none |
| **W1** | Fix funnel counter semantics + labels | W0 | S | none |
| **W2** | Harden V2 discovery; extract shared log decoding | W0 | M | none |
| **W3** | V3 pool discovery (`PoolCreated`) + separate V3 cache | W2 | M | none |
| **W4** | Multi-leg V2 atomic arb (3–5 legs) | W1, W2 | L | **1 week of funnel data** |
| **W5** | V3 sandwich sizing via QuoterV2 | W1, W3 | L | **1 week of funnel data** |
| **W6** | UniversalRouter calldata decoding | W1 | M | **funnel shows a public-mempool gap** |

Sizes: S ≈ 1–2 days, M ≈ 3–5 days, L ≈ 1.5–2 weeks including tests and review.

**Explicitly out of scope for Phase 2.** 1inch v6 and 0x v2 decoders (revisit
after W6 produces data); Curve/Balancer/Maverick pool math; Compound/Morpho/Maker
liquidations; MEV-Share backrun bidding; any new chain; anything in Phase 1 or
Phase 3 of `ROADMAP.md`. If a ticket starts growing toward one of these, stop
and re-scope.

---

## W0 — Make the build verifiable

**Why first.** No one has ever run `cargo check` on this crate. Every other
workstream's "acceptance criteria" is meaningless until a compiler has an
opinion. Budget for the possibility that the existing code does not compile
cleanly on first contact.

**Tasks**

1. Enable CI. From the repo root:
   ```bash
   mkdir -p .github/workflows && git mv ci/github-actions-ci.yml .github/workflows/ci.yml
   ```
   (It is parked in `ci/` because the automation that wrote it lacked the
   GitHub `workflows` permission. See `ci/README.md`.)
2. Run and green the four jobs: `contracts` (forge build + test),
   `artifact-drift` (solc-js recompile, fails on ABI drift),
   `bot` (`cargo clippy --all-targets`, `cargo test --all`),
   `frontend` (`tsc --noEmit`, `next build`).
3. Locally: `make bot-check`, `make bot-test`, `make contracts`, `make front-build`.
4. Fix whatever breaks. Compile errors and clippy findings get fixed in this
   workstream, not deferred. Behaviour changes beyond the minimum needed to
   compile go in a separate PR.
5. Update `docs/BUILD_NOTES.md` to record what CI now verifies.

**Acceptance criteria**

- CI is green on `arena/01a023c1-jerseymikes` and required on PRs to `main`.
- `cargo test --all` passes with all 51 existing tests running (not skipped).
- `cargo clippy --all-targets` has zero warnings, or documented `#[allow]`s with
  a reason comment.
- `docs/BUILD_NOTES.md` no longer claims anything is unverified that CI now
  verifies.

**Non-goals.** Do not add an end-to-end integration test here. It needs a
deterministic mainnet state fixture and is a research project of its own; it is
a known, accepted gap.

---

## W1 — Fix the funnel's semantics

**The defect.** `engine.rs:295-303` and `engine.rs:329-337` bump
`candidates_emitted` / `candidates_skipped` **once per strategy invocation**,
not once per `Opportunity`:

```rust
let opps = strat.on_block(&this.ctx, &head).await;
this.stats.record_funnel(kind, |f| {
    if opps.is_empty() { f.candidates_skipped += 1; } else { f.candidates_emitted += 1; }
});
```

A block that yields 30 candidates and a block that yields 1 are indistinguishable.
Downstream counters (`gated_by_risk`, `simulations_*`, `submittable`) *are*
per-opportunity, so the funnel silently mixes two units and the dashboard's
implied "conversion rate" between the first stage and the rest is wrong. This
matters now because W4 and W5 are judged by their effect on candidate volume,
and the current counter cannot see volume.

**Required change**

- Split the two units explicitly. Keep the invocation counters and add
  opportunity counters — do not just redefine the existing fields, or a week of
  already-collected funnel history becomes uninterpretable:

  | Field | Unit | Meaning |
  | --- | --- | --- |
  | `invocations_with_output` | per call | calls returning ≥ 1 opportunity (today's `candidates_emitted`) |
  | `invocations_empty` | per call | calls returning 0 opportunities (today's `candidates_skipped`) |
  | `candidates_emitted` | per opportunity | `+= opps.len()` |
  | `gated_by_risk`, `missing_victim_raw`, `simulations_*`, `submittable` | per opportunity | unchanged |

- Bump `candidates_emitted` by `opps.len()` in both `on_block` and `on_pending`.
- Keep `Stats::record_funnel` as the only mutation path (`engine.rs:100`).
- `api.rs::funnel` and `/api/status.stats.funnel` serialise the new fields.
- `frontend/lib/types.ts::FunnelCounters` and `FunnelPanel.tsx` gain the new
  rows. The panel's explanatory copy (`FunnelPanel.tsx:120-130`) must state the
  unit of each row — the current text implies everything is per-opportunity.
- The "seen" derivation in `FunnelPanel.tsx:81` (`candidatesEmitted +
  candidatesSkipped`) becomes `invocationsWithOutput + invocationsEmpty`.

**Tests** (extend the four in `engine.rs:505-566`)

- A strategy returning 3 opportunities bumps `candidates_emitted` by 3 and
  `invocations_with_output` by 1.
- A strategy returning 0 bumps `invocations_empty` by 1 and leaves
  `candidates_emitted` at 0.
- Snapshot serialises both units for a strategy that has never been recorded
  (empty map must not panic — existing `funnel_starts_empty` covers this).
- Global atomics (`opportunities`, `simulations`, …) remain independent of the
  funnel (existing regression test).

**Acceptance criteria**

- `/api/funnel` returns both units, with `candidates_emitted ≥
  invocations_with_output` for every strategy under load.
- The dashboard labels each row with its unit.
- Documented in `docs/STRATEGIES.md` so operators reading the funnel know what
  they are looking at.

---

## W2 — Harden V2 discovery

`strategies/discovery.rs` is correct on the point that matters most — it only
inserts into `seen` *after* a successful fetch that passes the filters, so a
transient `fetch_v2_pool` failure or a rate-limited provider does not
permanently blacklist a pool. That invariant is load-bearing during provider
rate limiting and reorg recovery. **Do not "optimise" it by marking seen
early.** It currently has no test.

**Tasks**

1. **Deterministic tests.** Introduce a seam so discovery is testable without a
   live RPC — a trait over the two calls it makes (`scan_pair_created`,
   `dex::fetch_v2_pool`) with a fake in tests is the smallest change. Cover:
   - a transient `fetch_v2_pool` failure leaves the pair *unseen*, and the next
     block's scan retries it and succeeds;
   - the log window advances correctly (`last_log_block == 0` → `head - 50`;
     otherwise `last + 1..=head`; `head <= last` → no-op returning 0);
   - duplicate `PairCreated` logs for the same pair load the pool once;
   - a non-WETH pair and a sub-`MIN_WETH_RESERVE` (0.5 WETH) pair are both
     rejected and, per the invariant above, are *not* inserted into `seen`.
2. **Reorg behaviour.** `last_log_block` moves forward monotonically, so a reorg
   that rewinds the head silently skips the re-org'd range. Either re-scan a
   small overlap window (simplest: `from = last.saturating_sub(REORG_DEPTH) + 1`
   with `REORG_DEPTH = 12`) or document why the gap is acceptable. Duplicate
   logs are already harmless once the dedup test above passes.
3. **Extract shared log decoding.** `scan_pair_created` in `strategies/mod.rs:283`
   hardcodes the two V2 factories and the `PairCreated` topic, and
   `strategies/sniper.rs` does its own scan. Pull out a generic
   `scan_factory_logs(rpc, addresses, topic, from, to) -> Vec<Log>` plus
   per-event decoders, so W3 can add `PoolCreated` without copy-paste. Do this
   **before** W3, not during it.
4. **Bound the window.** A cold start with a stale `last_log_block`, or a long
   RPC outage, can request thousands of blocks in one `eth_getLogs` and get
   rejected by the provider. Cap the span (suggest 500 blocks/scan, catching up
   over successive blocks) and log when the cap is hit.

**Acceptance criteria**

- Discovery has ≥ 6 unit tests, none of which touch the network.
- Retry-after-failure is asserted, not assumed.
- `scan_factory_logs` is used by discovery and the sniper; the duplicated scan
  in `sniper.rs` is gone.
- `eth_getLogs` span is bounded and the cap is observable in logs.

---

## W3 — V3 pool discovery

**Hard constraint: V2 and V3 pools do not share a cache or a type.** `PoolCache`
(`strategies/mod.rs:90`) stores `V2Pool`, whose `reserve0`/`reserve1` model is
meaningless for concentrated liquidity. Inserting a V3 pool into it would make
`v2_amount_out` produce plausible-looking, wrong quotes that every downstream
gate would accept. This is the most dangerous available mistake in Phase 2.

**Tasks**

1. Add `V3Pool { address, token0, token1, fee, tick_spacing, block }` to `dex.rs`
   (state — `slot0`, `liquidity` — is read on demand via the existing
   `jit.rs` pool-state helper; do not cache mutable V3 state).
2. Add a separate `V3PoolCache` alongside `PoolCache`. Same `insert`/`get`/`all`
   shape, no shared storage.
3. Scan `PoolCreated(address,address,uint24,address)` from
   `known::UNIV3_FACTORY` (`0x1F98431c8aD98523631AE4a59f267346ea31F984`) using
   the `scan_factory_logs` helper from W2. Note the ABI difference from
   `PairCreated`: `token0`, `token1` and `fee` are indexed topics; `tickSpacing`
   and `pool` are in `data`. Getting this wrong yields garbage addresses that
   look valid.
4. Filter to WETH-quoted pools and to the fee tiers we can act on (500 / 3000 /
   10000). Skip 100 unless someone shows a reason.
5. Gate behind the existing `POOL_DISCOVERY` toggle plus a new
   `POOL_DISCOVERY_V3` (default **off** until W5 can consume it) in
   `config.rs::Config::from_env` and `.env.example`.

**Tests**

- `PoolCreated` topic/data decoding against a captured mainnet log fixture
  (checked in as a JSON fixture — no network).
- A `PairCreated` log fed to the V3 decoder is rejected, and vice versa.
- V3 pools never appear in `PoolCache::all()`.

**Acceptance criteria**

- V3 discovery can be switched on and the two caches remain disjoint, asserted
  by a test.
- One `eth_getLogs` per block total for both factories combined — not two more
  round-trips per block. Discovery runs on the block task before strategies
  spawn (`engine.rs:280-288`); keep its added latency under ~50 ms/block.

---

## W4 — Multi-leg V2 atomic arb

**Gate: do not start until the funnel (W1) has a week of data and W2 has grown
the pool cache.** A 5-leg cycle search over 8 pools is pointless; the same
search over 50 discovered pools is the point.

### Algorithm — direct cycle enumeration, not Bellman–Ford

This is a decided question; do not relitigate it in review. A textbook
Bellman–Ford over the log-rate graph produces false positives: edge weights are
logs of exchange rates, and a coarse fixed-point `log_e` reports edges with
coincidentally equal bit-lengths as relaxable, surfacing cycles that do not
exist on chain. Doing it correctly needs a high-precision `log_e` table plus a
Newton iteration on input size — a research-grade port, not a PR. Direct
enumeration is correct by construction because every leg is priced with the same
`dex::v2_amount_out` that the existing tests pin against Solidity. The cost is
more candidates, which is why the budgets below are mandatory.

### Implementation

New file `bot/crates/mev-bot/src/dex/graph.rs`:

1. `build_edges(pools) -> Vec<DirectedEdge>` — two edges per pool (one per
   direction). Drop pools with a zero reserve on either side.
2. `adjacency(edges) -> HashMap<Address, Vec<usize>>` keyed by `token_in`.
3. `enumerate_cycles(adj, anchors, MAX_CYCLE_LEN = 5) -> Vec<Cycle>` — recursive
   DFS from each anchor (WETH + `arb::CORE_TOKENS`), `visited` set prevents
   revisiting a token within a walk. At every depth ≥ 2, close back to the
   anchor via any edge whose `token_out == anchor`. Keep walking after closing:
   parallel pools give a token multiple closing edges.
4. De-duplicate by the **sorted** edge-index sequence — the same cycle is
   reachable from different anchors and by different walks.
5. Size each cycle with the existing `dex::ternary_search_max` over
   `cycle.evaluate(...) - x`. The composed constant-product profit curve is
   unimodal with positive fees, which is exactly what that search assumes.
6. Build calls by reusing `strategies::sandwich::build_leg` per leg: transfer in,
   call `swap`, output lands at the next pool. Capital is a zero-fee Balancer
   flash loan, as today.
7. Replace the pair-pair double loop in `arb.rs::on_block` (`arb.rs:48-62`).
   **Leave `on_pending`'s back-run path intact** — it operates on post-victim
   state and is a different search.

### Mandatory budgets

The old design said "3–5 legs" with no numbers. These are the numbers; they are
acceptance criteria, not suggestions.

| Budget | Value | Enforcement |
| --- | --- | --- |
| Max cycle length | 5 legs | `MAX_CYCLE_LEN` const + test asserting DFS depth |
| Max candidates simulated per block | 32 | sort by expected profit, truncate; matches `MAX_INFLIGHT_PER_STRATEGY=32` |
| Max enumeration wall-clock per block | 25 ms | measured in a bench; hard `Instant` cancellation check in the DFS |
| Max pools in graph | 200 | truncate by WETH reserve, largest first |
| Gas model | 320k base + 120k/leg beyond 2 | pre-filter only; the fork simulation remains the arbiter |
| Extra RPC calls | **zero** | enumeration reads the cache only |

A 5-leg cycle is ~680k gas — real money at 50+ gwei. With `BRIBE_BPS=9000` it
needs roughly 10× the gross profit of a 2-leg cycle to clear the net gate. That
is intended.

### Tests

Unit tests in `graph.rs` for: empty input; identical pools; diverged pools
(a real triangle that must be found); 3-leg triangle profit matches a
hand-computed `v2_amount_out` chain; dust pool dropped; cycle closes to the
start token; dedup across anchors; depth never exceeds `MAX_CYCLE_LEN`; the
time budget cancels cleanly and returns the best-so-far.

A criterion bench (or a plain timed test) on a synthetic 200-pool graph proving
the 25 ms budget holds. Ship the number in the PR description.

### Acceptance criteria

- All budgets above enforced in code and covered by a test.
- `arb.rs::on_block` produces a superset of today's 2-leg results on the same
  pool set — add a test with two pools where the multi-leg path must find the
  same cycle the old loop found.
- Funnel shows the change: report `candidates_emitted` per block before and
  after on the same feed. If it does not move, say so — that is a valid result.

---

## W5 — V3 sandwich via QuoterV2

**Gate: after W1 and W3.** Ship behind a config toggle (`STRATEGY_SANDWICH_V3`,
default off).

### Sizing — QuoterV2, not hand-rolled Q64.96

Also a decided question. `TickMath.getSqrtRatioAtTick` and
`SqrtPriceMath.getNextSqrtPriceFromAmount0RoundingUp` need `mulDiv` over 512-bit
intermediates, which `alloy-primitives` does not give us, and a subtly wrong
port yields silently wrong quotes that a same-author unit test will happily
assert. `dex::quote_v3` (`dex.rs:459`) already calls
`QuoterV2.quoteExactInputSingle` via `eth_call` against
`known::UNIV3_QUOTER_V2` and returns the exact integer output. One extra RPC
round-trip per candidate, correct by construction, consistent with how JIT
already works.

### Implementation

1. Decode the victim: reuse `jit::decode_v3_swap` (`jit.rs:190`) — it already
   handles `ISwapRouter02.exactInputSingle` (`0x04e45aaf`) and returns
   `V3SwapIntent`. Verify the selector matches the `sol!` definition; a
   parameter reorder changes the 4-byte selector and the decoder must be
   checked against the macro, both sides.
2. Candidate filter: `amount_in > 0`, `token_in != token_out`, pool present in
   the W3 V3 cache.
3. Read pool state with the existing `jit.rs` `pool_state` helper (`slot0`,
   `liquidity`, `token0`, `token1`).
4. Size the front-run with `dex::quote_v3` inside the search. **Respect the RPC
   budget below** — a naive ternary search over `quote_v3` is ~120 `eth_call`s
   per candidate and will get the bot rate-limited off its provider.
5. Victim-revert trap: any `x` that pushes the victim's output below their
   `amountOutMinimum` scores **zero**. This is the trap that catches
   front-runners who do not model the slippage bound. It is non-negotiable and
   needs an explicit test.
6. Back-run: the quoter prices the *current* pool state, not the post-front-run
   state. Ship the conservative path — issue the back-run with
   `amountOutMinimum = 0` and let the executor's atomic profit guard catch a
   mispricing. The two-quote approximation is a follow-up with its own PR.
7. `sqrtPriceLimitX96 = 0` (no limit). Tight-range pools may reject; the fork
   simulation catches it.
8. Route through the router, not the pool directly. Pool-direct saves ~20k gas
   but needs `int256` encoding and a swap callback — a separate, later PR.

### RPC budget

| Budget | Value |
| --- | --- |
| `eth_call` per V3 candidate | ≤ 12 (coarse grid, then refine) |
| V3 candidates evaluated per pending tx | ≤ 4, largest notional first |
| Added latency on the pending path | ≤ 25 ms p95 |

The pending-tx path is the bot's hot path: a 5 ms strategy × 200 pending
txs/block = 1 s/block on a serial executor, against a design assumption of
~50 ms/block. Profile before merging, and record the p95 in the PR.

### Tests

Selector decoding against a captured mainnet `exactInputSingle` calldata
fixture; a victim whose `amountOutMinimum` makes every size unprofitable
produces zero opportunities; sizing is monotone in the quoter's responses
(fake quoter, no network); toggle off means zero added RPC calls.

### Acceptance criteria

- Toggle-gated, default off, documented in `.env.example` and `docs/STRATEGIES.md`.
- No live-RPC tests.
- Funnel distinguishes V2 from V3 sandwich outcomes (either a new `Strategy`
  variant or a labelled sub-counter — pick one and be consistent across
  `types.rs::Strategy::all()`, `config::StrategyToggles`, `engine.rs::new`,
  `frontend/lib/format.ts::STRATEGY_LABEL` and `STRATEGY_COLOR`).

---

## W6 — UniversalRouter decoding

**Gate: only after a week of funnel data shows a meaningful public-mempool gap.**

Be honest about the payoff. A large share of big-swap flow on mainnet is
deliberately *not* in the public mempool — Flashbots Protect, MEV-Blocker, CoW
batch auctions, 1inch Fusion, UniswapX. Decoding public calldata only helps for
the portion that is public, which is the portion established searchers already
compete for. This is the highest-effort, lowest-certainty workstream in Phase 2,
which is why it is last and gated on evidence.

**Scope: UniversalRouter only** (`0x3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD`).
Decode `execute(commands, inputs)` by walking the command byte string and the
per-command input arrays, handling at minimum `V3_SWAP_EXACT_IN` and
`V2_SWAP_EXACT_IN`. 1inch v6 and 0x v2 are **out of scope** — revisit only if
the funnel shows UniversalRouter decoding moved the numbers.

**Structure**

- `dex/calldata/mod.rs` — `DecodedSwap` + `decode_any_router(tx) -> Option<DecodedSwap>`
  trying each decoder in order.
- `dex/calldata/universal_router.rs` — the decoder.
- Wrap the existing `strategies::decode_swap` (`strategies/mod.rs:230`) and the
  new decoder behind one entry point returning the `SwapIntent` that sandwich
  and arb already consume. Do not fork the consumers.

**Acceptance criteria**

- Fixture-driven tests: ≥ 5 captured mainnet UniversalRouter calldata samples
  decoded to the expected `SwapIntent`, plus malformed-input cases that must
  return `None` rather than panic.
- Decoder adds < 1 ms per pending tx (it is pure calldata parsing; assert it in
  a bench).
- A written report on what the funnel did after one week with it enabled. If
  the answer is "nothing", that is the deliverable and 1inch/0x stay closed.

---

## 3. Definition of done for Phase 2

Phase 2 is complete when all of these hold:

1. CI is enabled and green; `cargo test --all`, `forge test`, `tsc --noEmit` and
   `next build` all run on every PR.
2. The funnel reports per-opportunity and per-invocation counts distinctly, and
   the dashboard labels both.
3. Pool discovery covers V2 and V3 with disjoint caches, bounded log windows,
   retry-safe `seen` tracking, and ≥ 6 non-network tests each.
4. Multi-leg V2 arb is live in `on_block` within every budget in W4, with the
   before/after funnel delta reported.
5. V3 sandwich sizing exists behind a default-off toggle, within its RPC and
   latency budgets, with the victim-revert trap tested.
6. UniversalRouter decoding either shipped with a funnel report, or formally
   deferred with the funnel data that justifies deferring it.
7. `docs/STRATEGIES.md`, `docs/ROADMAP.md`, `docs/RISK.md` and `.env.example`
   reflect the shipped state. Every new toggle is documented.
8. `live_execution` is still false and the two-key switch is untouched.

---

## Appendix A — Footguns

Each of these has bitten this codebase once already.

**Never use `f64` for sandwich or arb sizing.** Always `U256`. The JIT
strategy's `size_position` uses floats and that is fine — it is approximate and
the fork is the arbiter. A floating-point sandwich sizer round-trips to zero in
the fork on roughly 1-in-1000 swaps: unit tests pass, dashboard lies, bundles
revert.

**`v2_amount_out` rounds down**, matching on-chain `getAmountOut` exactly. Any
port that rounds differently produces bundles that revert in the fork.

**`ternary_search_max` assumes unimodality.** True for V2 sandwich and cyclic
arb profit curves. A new strategy with a non-unimodal curve gets a silent local
maximum.

**`optimal_sandwich_in`'s victim-revert check** scores any size that pushes the
victim below `amountOutMin` as zero profit. Do not optimise it away.

**Never add a `sol!` interface without verifying the selector.** The macro
derives the 4-byte selector from the signature; reordering or renaming params
changes it. Some `decode_*` functions hardcode selectors — check both sides.
`decode_v3_swap` and `IUniswapV3Pool::swapCall::SELECTOR` are the reference.

**Do not serialise a `U256` without `.to_string()`.** Large values otherwise
serialise in a form viem will not parse. The existing code is consistent; match it.

**`eth_call` does not return a revert reason on mainnet** — only on the Anvil
fork. Any pre-filter reading `sim.revert_reason` must live inside the fork path.

**Watch the spawn budget.** The per-tx loop spawns one task per (tx, strategy)
pair — 5× the pending rate at peak. `max_inflight_per_strategy` and
`SIM_TIMEOUT_MS` are the backstops; both may need tuning when W4/W5 land.

**Things that look like over-engineering but are not:** the per-call
`pool_by_addr: HashMap` index in strategy files (avoids repeated `RwLock`
walks); the flat `if let Some(x) = …;` chains instead of `?` (early-return per
candidate, not per tx); the `sol!` blocks in `dex.rs`, `jit.rs` — those are the
ABI truth claims.

---

## Appendix B — Existing API surface to reuse

Do not reimplement any of these.

| Symbol | Location | Use |
| --- | --- | --- |
| `dex::v2_amount_out` / `v2_amount_in` | `dex.rs:179,194` | exact integer V2 pricing |
| `dex::ternary_search_max` | `dex.rs:209` | unimodal integer optimiser |
| `dex::optimal_two_leg_arb` | `dex.rs:328` | reference for W4's cycle sizing |
| `dex::optimal_sandwich_in` | `dex.rs:249` | V2 sandwich sizing incl. victim trap |
| `dex::fetch_v2_pool` | `dex.rs:374` | batched V2 pool snapshot |
| `dex::get_pair` | `dex.rs:436` | factory `getPair` |
| `dex::quote_v3` | `dex.rs:459` | QuoterV2 exact-in — W5's sizing primitive |
| `strategies::PoolCache` | `strategies/mod.rs:90` | V2 cache: `get/insert/all/pair_for/load/refresh_all` |
| `strategies::decode_swap` / `SwapIntent` | `strategies/mod.rs:217,230` | V2 router decoding |
| `strategies::scan_pair_created` | `strategies/mod.rs:283` | V2 `PairCreated` scan (generalise in W2) |
| `strategies::sandwich::build_leg` | `sandwich.rs:113` | pool-direct V2 swap calls |
| `strategies::jit::decode_v3_swap` / `V3SwapIntent` | `jit.rs:182,190` | V3 router decoding |
| `strategies::jit::V3State` | `jit.rs:211` | slot0 + liquidity snapshot |
| `engine::Stats::record_funnel` | `engine.rs:100` | the only funnel mutation path |

**Adding a strategy** (the most dangerous routine change): add the
`types::Strategy` variant (update `all()`), add a `config::StrategyToggles`
field and parse it in `Config::from_env`, `pub mod` it in `strategies/mod.rs`,
implement `StrategyImpl` (`jit.rs` is the most complete example), wire it in
`engine.rs::new`, add funnel integration (**not optional**), and add a "Why no
simulations?" entry to `frontend/components/RiskPanel.tsx`.

**Adding a feed event:** add a `types::FeedEvent` variant, emit from `engine.rs`,
add a renderer — SSE dispatch is tag-based. High-rate events need their own
channel or a sampled emit; `broadcast::channel` is sized for the current mix.

---

## Appendix C — Config knobs

| Var | Default | Notes |
| --- | --- | --- |
| `POOL_DISCOVERY` | `true` | V2 discovery per block (`config.rs:261`) |
| `POOL_DISCOVERY_V3` | *(new in W3)* `false` | keep off until W5 consumes it |
| `STRATEGY_SANDWICH_V3` | *(new in W5)* `false` | |
| `MIN_NET_PROFIT_WEI` | `1` | do not raise/lower to chase funnel numbers |
| `MAX_POSITION_WEI` | `100e18` | notional gate |
| `MAX_BASE_FEE_WEI` | `500 gwei` | |
| `MAX_INFLIGHT_PER_STRATEGY` | `32` | also W4's candidate cap |
| `SIM_TIMEOUT_MS` | `2500` | second backstop on spawn storms |
| `BRIBE_BPS` | `9000` | **do not change in Phase 2.** Builders sort by payment per gas; a 50% bribe loses to a 90% bribe on the same opportunity. Lower only makes sense when amortising across a batch. |
| `LIVE_EXECUTION` | `false` | two-key switch; out of scope |

**Reading the funnel when tuning risk.** `simulationsReverted >>
simulationsSucceeded` means gates are too loose — the fix is almost always the
strategy's pre-filter sizing, not `MIN_NET_PROFIT_WEI`. `candidatesEmitted > 0`
with `gatedByRisk` dominating means gates are too tight — raise
`MAX_BASE_FEE_WEI` (if base fee is the gate), `MAX_POSITION_WEI` (notional), or
`MAX_INFLIGHT_PER_STRATEGY` (queueing).

---

## Appendix D — Watch items (not Phase 2 work)

Review these quarterly; act when the data moves.

**Private orderflow keeps growing.** Watch the funnel's `pendingSeen` rate over
months. If it trends down, every public-mempool strategy trends down with it.
`MEV_SHARE_SSE_URL` and `EXTRA_MEMPOOL_WS` are the partial mitigations already
configured. The real long-term direction is intent auctions (solver networks);
the executor contract is the right primitive, the solver integration is missing,
and it is not on the roadmap yet. It probably should be.

**L2 economics differ structurally.** Gas is ~10⁻⁴ of mainnet, so the mainnet
`BRIBE_BPS` calibration is wrong there. Sequencer auctions (Arbitrum express
lane, OP preconfirmations) are the "sequencer feed" the README references;
integrate via the chain's published protocol, not polling. Cross-chain MEV is
real but narrow — build one chain properly first.

**New AMM designs:** add them as *pricing modules* in `dex/`, the way
`v2_amount_out` and `quote_v3` are, and let strategies compose them. Never as a
new strategy. Curve and Balancer math are the next obvious additions.

**Account abstraction:** 4337 user ops bundle multiple calls, which the
"one tx = one swap" pre-filter silently drops; sponsored gas breaks the victim's
cost-basis assumption in sandwich sizing. Small share today — watch it.

**Legal/operational:** simulation-only mode is a feature, not a development
convenience. The relay's `eth_callBundle` cross-check is a real, timestamped
audit trail; treat it as a log you would be willing to share.

---

## Appendix E — Priority order beyond Phase 2

From `docs/ROADMAP.md`, the practical order once Phase 2 closes:

1. **Phase 1 replay validation** — re-simulate historical blocks from the
   database against actual relay bid traces to get a true-positive rate for the
   pipeline. That number is the only one that matters before `LIVE_EXECUTION`.
2. Competition modelling and the latency budget (mempool→bundle under ~150 ms).
3. Nonce/inventory manager, reorg reconciliation.
4. Phase 3 live-execution work, opt-in, separate PRs.
5. New chains, in roadmap order: Base → Arbitrum → BNB → Solana. Solana is a
   separate engine, not a config change.

---

## Appendix F — Keeping the codebase small

~6,400 lines of Rust, ~300 of Solidity, ~2,400 of TypeScript. That is a feature.
Every line is one you have to defend; every dependency is one you have to vet;
every ABI definition is a truth claim about mainnet you have to keep honest. If
a feature is heading past a hundred lines, ask whether twenty lines of
restructuring gets the same outcome.

If a maintainer six months from now opens this repo and asks "where does this
start?", the answer must still be `bot/crates/mev-bot/src/main.rs` — one
function, ~150 lines — with `engine.rs` next and everything else following from
there. Preserve that property.
