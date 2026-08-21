# Phase 2 — Engineering Work Order

**Audience:** the dev teams picking up Phase 2 of JerseyMikes.
**What this is:** the consolidation of the former `PHASE_2_DESIGN.md` (the
engineering spec) and `PHASE_2_REVIEW.md` (the maintainer review of that spec)
into one actionable work order, reconciled against the actual state of the
checkout. Both source files are deleted; they are recoverable from git history
at commit `e62bbc2`.
**Lifecycle:** this document is **temporary**. Delete it when the Definition of
Done in §3 is met — see §4, *Retiring this document*.
**Companion:** [`MAINTAINING.md`](MAINTAINING.md) is the permanent guide to how
this codebase thinks. It is not superseded by this document and does not get
deleted. Read its §1 (Mindset), §3 (Common Change Patterns) and §5 (Footguns)
before writing code; this work order references specific sections rather than
restating them.
**Scope:** everything below is simulation-only. Nothing in Phase 2 touches the
live-execution path.

---

## 0. The three rules for this phase

Longer versions of the first two are in `MAINTAINING.md` §1.

**Measure before you expand.** Phase 2 has one hard ordering rule: the funnel
must be *correct* (W1) and read for a full week before any new strategy surface
is added. Every workstream below is instrumented so its effect on the funnel is
observable. "The simulation tape looks busier" is a feeling; "multi-leg arb
added 12 candidates/block that pass risk" is data.

**Don't loosen gates to chase opportunities.** Few opportunities is the designed
steady state. If the funnel reads zero, the cause is almost never
`MIN_NET_PROFIT_WEI` — it is a quiet mempool feed, an empty pool cache, or an
RPC that doesn't support `eth_getRawTransactionByHash`.

**Nothing broadcasts.** `live_execution` stays off for all of Phase 2. Phase 1
(replay validation, competition modelling, latency budget) is the prerequisite
for going live and is explicitly *not* in scope here. Do not touch
`LIVE_EXECUTION`, `BRIBE_BPS`, or the profit guard in `MevExecutor.sol`.

---

## 1. Verified current state

Checked against the checkout at `e62bbc2`, not against the two source documents
— several of their claims were stale. Treat this table as ground truth.

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

Two corrections to the source documents worth calling out, because they change
the plan:

- The design doc presented the funnel counter and pool discovery as work to be
  done. Both are already merged. The review caught this; the work that remains
  on them is *correctness*, not construction (W1, W2).
- **The Rust in this repo has never been compiled or tested by CI.** Neither
  source document stated this plainly. W0 exists because of it and blocks
  everything else.

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

This ordering is the review's recommended sequence, made concrete: measure
first, then pool coverage, then multi-leg arb, then V3 sandwich, aggregators
last. It matches `MAINTAINING.md` §7.

**Explicitly out of scope for Phase 2.** 1inch v6 and 0x v2 decoders (revisit
after W6 produces data); Curve/Balancer/Maverick pool math;
Compound/Morpho/Maker liquidations; MEV-Share backrun bidding; any new chain;
anything in Phase 1 or Phase 3 of `ROADMAP.md`. If a ticket starts growing
toward one of these, stop and re-scope.

**Footguns by ticket.** Rather than restate `MAINTAINING.md` §5, here is the
mapping — read the referenced section before starting the ticket:

| Ticket | Read first |
| --- | --- |
| W1 | §3.3 step 6 (funnel integration is not optional) |
| W2, W3 | §3.1 (read pools from chain, don't add constants) |
| W4 | §2 (`v2_amount_out` rounding, `ternary_search_max` unimodality), §5 (no `f64` in arb sizing) |
| W5 | §2 (`optimal_sandwich_in`'s victim-revert check), §5 (`sol!` selectors, `eth_call` revert reasons) |
| W6 | §5 (`sol!` selector verification) |
| any new strategy variant | §3.3 (the full six-step checklist) |

---

## W0 — Make the build verifiable

**Why first.** No one has ever run `cargo check` on this crate. Every other
workstream's acceptance criteria is meaningless until a compiler has an opinion.
Budget for the possibility that the existing code does not compile cleanly on
first contact. This is the review's recommended next step #1, promoted to a
blocking ticket.

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
4. Fix whatever breaks. Compile errors and clippy findings get fixed here, not
   deferred. Behaviour changes beyond the minimum needed to compile go in a
   separate PR.
5. Update `docs/BUILD_NOTES.md` to record what CI now verifies.

**Acceptance criteria**

- CI is green on the working branch and required on PRs to `main`.
- `cargo test --all` passes with all 51 existing tests running (not skipped).
- `cargo clippy --all-targets` has zero warnings, or documented `#[allow]`s with
  a reason comment.
- `docs/BUILD_NOTES.md` no longer claims anything is unverified that CI now
  verifies.

**Non-goals.** No end-to-end integration test here. It needs a deterministic
mainnet state fixture and is a research project of its own; it is a known,
accepted gap (`MAINTAINING.md` §4).

---

## W1 — Fix the funnel's semantics

**The defect** (raised in the review, located here). `engine.rs:295-303` and
`engine.rs:329-337` bump `candidates_emitted` / `candidates_skipped` **once per
strategy invocation**, not once per `Opportunity`:

```rust
let opps = strat.on_block(&this.ctx, &head).await;
this.stats.record_funnel(kind, |f| {
    if opps.is_empty() { f.candidates_skipped += 1; } else { f.candidates_emitted += 1; }
});
```

A block that yields 30 candidates and a block that yields 1 are indistinguishable.
Downstream counters (`gated_by_risk`, `simulations_*`, `submittable`) *are*
per-opportunity, so the funnel silently mixes two units and the dashboard's
implied conversion rate between the first stage and the rest is wrong. This
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
- Snapshot serialises both units for a strategy never recorded (empty map must
  not panic — existing `funnel_starts_empty` covers this).
- Global atomics remain independent of the funnel (existing regression test).

**Acceptance criteria**

- `/api/funnel` returns both units, with `candidates_emitted ≥
  invocations_with_output` for every strategy under load.
- The dashboard labels each row with its unit.
- Documented in `docs/STRATEGIES.md` so operators know what they are reading.

---

## W2 — Harden V2 discovery

`strategies/discovery.rs` is correct on the point that matters most — it only
inserts into `seen` *after* a successful fetch that passes the filters, so a
transient `fetch_v2_pool` failure or a rate-limited provider does not
permanently blacklist a pool. That invariant is load-bearing during provider
rate limiting and reorg recovery, and it is exactly what the review flagged.
**Do not "optimise" it by marking seen early.** It currently has no test.

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
2. **Reorg behaviour.** Not raised in either source doc. `last_log_block` moves
   forward monotonically, so a reorg that rewinds the head silently skips the
   re-org'd range. Either re-scan a small overlap window (simplest:
   `from = last.saturating_sub(REORG_DEPTH) + 1` with `REORG_DEPTH = 12`) or
   document why the gap is acceptable. Duplicate logs are already harmless once
   the dedup test above passes.
3. **Extract shared log decoding.** `scan_pair_created` (`strategies/mod.rs:283`)
   hardcodes the two V2 factories and the `PairCreated` topic, and
   `strategies/sniper.rs` does its own scan. Pull out a generic
   `scan_factory_logs(rpc, addresses, topic, from, to) -> Vec<Log>` plus
   per-event decoders, so W3 can add `PoolCreated` without copy-paste. The
   review is explicit that this happens **before** W3, not during it.
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
gate would accept. The review calls this out as unsafe; it is the most dangerous
available mistake in Phase 2.

**Tasks**

1. Add `V3Pool { address, token0, token1, fee, tick_spacing, block }` to `dex.rs`
   (mutable state — `slot0`, `liquidity` — is read on demand via the existing
   `jit.rs` pool-state helper; do not cache it).
2. Add a separate `V3PoolCache` alongside `PoolCache`. Same `insert`/`get`/`all`
   shape, no shared storage.
3. Scan `PoolCreated(address,address,uint24,address)` from
   `known::UNIV3_FACTORY` (`0x1F98431c8aD98523631AE4a59f267346ea31F984`) using
   the `scan_factory_logs` helper from W2. Note the ABI difference from
   `PairCreated`: `token0`, `token1` and `fee` are indexed topics; `tickSpacing`
   and `pool` are in `data`. Getting this wrong yields garbage addresses that
   look valid.
4. Filter to WETH-quoted pools and to actionable fee tiers (500 / 3000 / 10000).
   Skip 100 unless someone shows a reason.
5. Gate behind the existing `POOL_DISCOVERY` toggle plus a new
   `POOL_DISCOVERY_V3` (default **off** until W5 can consume it) in
   `config.rs::Config::from_env` and `.env.example`.

**Tests**

- `PoolCreated` topic/data decoding against a captured mainnet log fixture
  (checked in as JSON — no network).
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

Decided in the design doc, endorsed by the review as the auditable choice. Do
not relitigate it in code review. A textbook Bellman–Ford over the log-rate
graph produces false positives: edge weights are logs of exchange rates, and a
coarse fixed-point `log_e` reports edges with coincidentally equal bit-lengths
as relaxable, surfacing cycles that do not exist on chain. Doing it correctly
needs a high-precision `log_e` table plus a Newton iteration on input size — a
research-grade port, not a PR. Direct enumeration is correct by construction
because every leg is priced with the same `dex::v2_amount_out` the existing
tests pin against Solidity. The cost is more candidates, which is why the
budgets below are mandatory.

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
   unimodal with positive fees, which is what that search assumes
   (`MAINTAINING.md` §2).
6. Build calls by reusing `strategies::sandwich::build_leg` per leg: transfer
   in, call `swap`, output lands at the next pool. Capital is a zero-fee
   Balancer flash loan, as today.
7. Replace the pair-pair double loop in `arb.rs::on_block` (`arb.rs:48-62`).
   **Leave `on_pending`'s back-run path intact** — it operates on post-victim
   state and is a different search.

### Mandatory budgets

The design doc said "3–5 legs" with no numbers and the review refused it on
exactly that basis. These are the numbers; they are acceptance criteria, not
suggestions.

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

Unit tests in `graph.rs` for: empty input; identical pools; diverged pools (a
real triangle that must be found); 3-leg triangle profit matches a hand-computed
`v2_amount_out` chain; dust pool dropped; cycle closes to the start token; dedup
across anchors; depth never exceeds `MAX_CYCLE_LEN`; the time budget cancels
cleanly and returns best-so-far.

A criterion bench (or a plain timed test) on a synthetic 200-pool graph proving
the 25 ms budget holds. Put the number in the PR description.

### Acceptance criteria

- All budgets above enforced in code and covered by a test.
- `arb.rs::on_block` produces a superset of today's 2-leg results on the same
  pool set — add a test with two pools where the multi-leg path must find the
  same cycle the old loop found.
- Funnel delta reported: `candidates_emitted` per block before and after on the
  same feed. If it does not move, say so — that is a valid result.

---

## W5 — V3 sandwich via QuoterV2

**Gate: after W1 and W3.** Ship behind a config toggle (`STRATEGY_SANDWICH_V3`,
default off), as the review requires.

### Sizing — QuoterV2, not hand-rolled Q64.96

Also decided and endorsed. `TickMath.getSqrtRatioAtTick` and
`SqrtPriceMath.getNextSqrtPriceFromAmount0RoundingUp` need `mulDiv` over 512-bit
intermediates, which `alloy-primitives` does not provide, and a subtly wrong
port yields silently wrong quotes that a same-author unit test will happily
assert. `dex::quote_v3` (`dex.rs:459`) already calls
`QuoterV2.quoteExactInputSingle` via `eth_call` against `known::UNIV3_QUOTER_V2`
and returns the exact integer output. One extra RPC round-trip per candidate,
correct by construction, consistent with how JIT already works.

### Implementation

1. Decode the victim: reuse `jit::decode_v3_swap` (`jit.rs:190`) — it already
   handles `ISwapRouter02.exactInputSingle` (`0x04e45aaf`) and returns
   `V3SwapIntent`. Verify the selector against the `sol!` definition on both
   sides (`MAINTAINING.md` §5).
2. Candidate filter: `amount_in > 0`, `token_in != token_out`, pool present in
   the W3 V3 cache.
3. Read pool state with the existing `jit.rs` `pool_state` helper (`slot0`,
   `liquidity`, `token0`, `token1`).
4. Size the front-run with `dex::quote_v3` inside the search. **Respect the RPC
   budget below** — a naive ternary search over `quote_v3` is ~120 `eth_call`s
   per candidate and will get the bot rate-limited off its provider.
5. Victim-revert trap: any `x` that pushes the victim's output below their
   `amountOutMinimum` scores **zero**. This is the trap that catches
   front-runners who do not model the slippage bound (`MAINTAINING.md` §2). It
   is non-negotiable and needs an explicit test.
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
~50 ms/block (`MAINTAINING.md` §3.3). Profile before merging; record the p95 in
the PR.

### Tests

Selector decoding against a captured mainnet `exactInputSingle` calldata
fixture; a victim whose `amountOutMinimum` makes every size unprofitable
produces zero opportunities; sizing is monotone in the quoter's responses (fake
quoter, no network); toggle off means zero added RPC calls.

### Acceptance criteria

- Toggle-gated, default off, documented in `.env.example` and `docs/STRATEGIES.md`.
- No live-RPC tests.
- Funnel distinguishes V2 from V3 sandwich outcomes (either a new `Strategy`
  variant or a labelled sub-counter — pick one and be consistent across
  `types.rs::Strategy::all()`, `config::StrategyToggles`, `engine.rs::new`,
  `frontend/lib/format.ts::STRATEGY_LABEL` and `STRATEGY_COLOR`; the full
  checklist is `MAINTAINING.md` §3.3).

---

## W6 — UniversalRouter decoding

**Gate: only after a week of funnel data shows a meaningful public-mempool gap.**
Both source documents agree on this, and the review is explicit that 1inch and
0x wait for evidence.

Be honest about the payoff. A large share of big-swap flow on mainnet is
deliberately *not* in the public mempool — Flashbots Protect, MEV-Blocker, CoW
batch auctions, 1inch Fusion, UniswapX (`MAINTAINING.md` §6.1). Decoding public
calldata only helps for the portion that is public, which is the portion
established searchers already compete for. Highest effort, lowest certainty,
therefore last and gated on data.

**Scope: UniversalRouter only** (`0x3fC91A3afd70395Cd496C647d5a6CC9D4B2b7FAD`).
Decode `execute(commands, inputs)` by walking the command byte string and the
per-command input arrays, handling at minimum `V3_SWAP_EXACT_IN` and
`V2_SWAP_EXACT_IN`. 1inch v6 and 0x v2 are **out of scope**.

**Structure**

- `dex/calldata/mod.rs` — `DecodedSwap` + `decode_any_router(tx) -> Option<DecodedSwap>`
  trying each decoder in order.
- `dex/calldata/universal_router.rs` — the decoder.
- Wrap the existing `strategies::decode_swap` (`strategies/mod.rs:230`) and the
  new decoder behind one entry point returning the `SwapIntent` that sandwich
  and arb already consume. Do not fork the consumers.

**Acceptance criteria**

- Fixture-driven tests: ≥ 5 captured mainnet UniversalRouter calldata samples
  decoded to the expected `SwapIntent`, plus malformed inputs that must return
  `None` rather than panic.
- Decoder adds < 1 ms per pending tx (pure calldata parsing; assert in a bench).
- A written report on what the funnel did after one week with it enabled. If the
  answer is "nothing", that is the deliverable and 1inch/0x stay closed.

---

## 3. Definition of done for Phase 2

Phase 2 is complete when all of these hold:

1. CI is enabled and green; `cargo test --all`, `forge test`, `tsc --noEmit` and
   `next build` run on every PR.
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

## 4. Retiring this document

When §3 is satisfied, this file gets deleted. Before deleting it:

1. Tick the Phase 2 boxes in [`ROADMAP.md`](ROADMAP.md) and remove its pointer
   to this file.
2. Fold anything that became a **durable rule about the codebase** into
   [`MAINTAINING.md`](MAINTAINING.md) — likely candidates are the W4 search
   budgets, the W5 RPC budget, and the V2/V3 cache separation rule. A budget
   that only lived in this file will be violated by the next person.
3. Move operator-facing behaviour into [`STRATEGIES.md`](STRATEGIES.md) and any
   new gates into [`RISK.md`](RISK.md).
4. Remove this document's row from the README documentation table.
5. `git rm docs/PHASE_2_HANDOFF.md`.

`MAINTAINING.md` is **not** deleted with it. It is the permanent guide and
outlives every phase.

---

## Appendix A — Existing API surface to reuse

Do not reimplement any of these. (Verified against the checkout; line numbers
are from `e62bbc2`.)

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

For the procedures — adding a strategy, adding a feed event, adding a pair,
tuning risk gates — follow `MAINTAINING.md` §3. Adding a strategy in particular
has a six-step checklist there; skipping step 6 (funnel integration) is how a
new strategy becomes unobservable.

---

## Appendix B — Config knobs touched by Phase 2

| Var | Default | Notes |
| --- | --- | --- |
| `POOL_DISCOVERY` | `true` | V2 discovery per block (`config.rs:261`) |
| `POOL_DISCOVERY_V3` | *(new in W3)* `false` | keep off until W5 consumes it |
| `STRATEGY_SANDWICH_V3` | *(new in W5)* `false` | |
| `MIN_NET_PROFIT_WEI` | `1` | do not move to chase funnel numbers |
| `MAX_POSITION_WEI` | `100e18` | notional gate |
| `MAX_BASE_FEE_WEI` | `500 gwei` | |
| `MAX_INFLIGHT_PER_STRATEGY` | `32` | also W4's candidate cap |
| `SIM_TIMEOUT_MS` | `2500` | backstop on spawn storms |
| `BRIBE_BPS` | `9000` | **do not change in Phase 2** — rationale in `MAINTAINING.md` §3.5 |
| `LIVE_EXECUTION` | `false` | two-key switch; out of scope |

Interpreting the funnel when a gate looks wrong: `MAINTAINING.md` §3.5.

---

## Appendix C — What Phase 2 deliberately does not answer

Carried over from the design doc's closing section, because it is still true:

- Phase 2 does not guarantee opportunities. It adds strategies; the simulator
  discovers opportunities. The count per block is a property of the market,
  competition and latency, not of this code.
- Phase 2 is not a path to live trading. The simulation-only guard stays. Phase 1
  (replay validation, competition modelling, latency budget) is the prerequisite
  and is not addressed here.
- Neither source document was written with a working Rust toolchain, and no
  claim in them about compilation or test results was ever verified. W0 is how
  that gets fixed; until it is green, treat any performance or correctness claim
  in this file as a hypothesis.
