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
**Baseline:** merged with `main` at `cd42422`, which added the bloXroute Max
Profit relay delivered-block ingestion. That integration routes already-mined
transactions through the strategy funnel, which interacts directly with W1 —
see §1.7.

**Continuation status (2026-08-21):** W5 (V3 sandwich) and W6 (UniversalRouter
decoder) are now implemented behind default-off toggles
(`STRATEGY_SANDWICH_V3`, `DECODE_UNIVERSAL_ROUTER`). A default checkout is
still the W1–W4 measurement instrument. The one-week funnel gates for
turning those toggles on, and for raising W4 to 3–5 legs, are not waived.
The maintainer reports that `make bot-check`, `make bot-test`, and
`make contracts` pass locally on the W1–W4 baseline; this session could not
run Rust independently. Frontend `tsc --noEmit` is clean.

A follow-up automation session (the same day) re-ran the checks it can run —
contracts solc compile-check (28 sources, `MevExecutor` runtime 9,618 B, no
artifact drift) and the frontend (`tsc --noEmit`, `npm run build`) — and both
are clean, and bumped the frontend's `next`/`react` to patched versions for
CVE-2025-66478 (CVSS 10.0 RSC RCE). It also re-attempted the W0 workflow push
and confirmed it is still rejected without `workflows` permission. Full details
in [`docs/BUILD_NOTES.md`](BUILD_NOTES.md). None of this changes the gates.

Once the GitHub `workflows` permission was granted and the workflow was enabled,
CI started running and surfaced two failures that had never been exercised:
the `embedded bytecode is current` (artifact-drift) job failed because
`compile-check.js` embedded solc's default IPFS metadata hash (which includes
each source file's absolute path) into `MevExecutor.runtime.hex`, so the checked-in
artifact only reproduced in the sandbox where it was generated. Fixed by passing
`metadata: {bytecodeHash: "none", useLiteralContent: true}` (matching
`foundry.toml`), regenerating the artifact (runtime 9,618 → 9,577 B), and
verifying a fresh checkout now reproduces it byte-for-byte. See
`docs/BUILD_NOTES.md`. The `bot (rust)` job's `cargo test --all` failure
(exit 101) is now also fixed: it was a deterministic wrong assertion in
`competition::tests::half_the_bid_is_unlikely` (the test expected
`p ∈ (0.05, 0.20)` but the shipped `LOGISTIC_K = 2.2` gives
`p = σ(-1.1) ≈ 0.2497` at half the winning bid), corrected to match the
model, plus a hardening of the dense-graph budget test's wall-clock ceiling.
All four CI jobs are green on the working branch (PR #15). What remains for
W0 is a human step: setting the workflow as a **required** check on PRs to
`main`.

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

## 1. State before this branch

Checked against the checkout at `e62bbc2`, not against the two source documents
— several of their claims were stale. This is the baseline W1–W4 were written
against; §1.5 covers what has since landed.

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
| Rust build/test verification | **Locally verified by the maintainer** | `make bot-check` and `make bot-test` pass; the authoring sandbox still cannot independently run Rust |
| Contracts build/test | **Locally verified by the maintainer** | `make contracts` passes; `node contracts/script/compile-check.js` also passes in the sandbox |
| CI | **Written, not remotely enabled** | `ci/github-actions-ci.yml` remains parked outside `.github/workflows/`; the GitHub App push is still rejected without `workflows` permission |

Two corrections to the source documents worth calling out, because they change
the plan:

- The design doc presented the funnel counter and pool discovery as work to be
  done. Both are already merged. The review caught this; the work that remains
  on them is *correctness*, not construction (W1, W2).
- **Rust and Foundry are now locally verified, but not by remote CI.** The
  maintainer reports green `make bot-check`, `make bot-test`, and
  `make contracts` runs. W0 remains open until the workflow is enabled and the
  checks run as required PR checks.

---

## 1.5 What landed on this branch

W1–W4 are implemented. Each ticket below keeps its full spec — the spec is what
reviewers check the code against — and each now opens with what exists.

| Area | File | What changed |
| --- | --- | --- |
| Funnel units | `engine.rs` | `FunnelCounters` split into per-call (`invocations_with_output`, `invocations_empty`) and per-opportunity (`candidates_emitted`) fields; new `Stats::record_invocation` is the only way to bump the first stage; 4 new tests |
| Funnel UI | `frontend/lib/types.ts`, `FunnelPanel.tsx`, `demo.ts` | new columns, per-column unit labels, "two units" explainer, summary cards split |
| Log scanning | `strategies/mod.rs` | generic `scan_factory_logs` + `decode_pair_created` / `decode_pool_created`; `try_scan_pair_created` / `try_scan_pool_created` return `Option` so a failed RPC is distinguishable from an empty range; 4 new fixture tests |
| V3 metadata | `dex.rs`, `strategies/mod.rs` | `V3Pool` type and a `V3PoolCache` that shares nothing with `PoolCache` |
| Discovery | `strategies/discovery.rs` | rewritten behind a `DiscoverySource` trait; retry-safe accept/reject sets; reorg overlap; span cap; V3 scan; 11 tests, none touching the network |
| Cycle search | `dex/graph.rs` (new) | `build_edges`, `adjacency`, `enumerate_cycles`, `evaluate`, `optimal_cycle_in`, `search`, budgets, 13 tests |
| Arb wiring | `strategies/arb.rs` | `on_block` now runs the cycle search; `build_cycle_opportunity` builds N-leg call sequences; back-run path untouched; 2 equivalence tests |
| Funnel lanes | `engine.rs`, `FunnelPanel.tsx` | `FunnelLane::{Live,Replay}` keyed off `TxSource`; `funnelReplay` in the API; lane toggle in the dashboard; 3 new tests (§1.7) |
| Replay back-pressure | `engine.rs`, `config.rs` | `evaluate_awaited` + `RELAY_TX_CONCURRENCY` semaphore bounds the delivered-block fan-out (§1.7) |
| Parent-block replay | `types.rs`, `ingest.rs`, `strategies/*`, `engine.rs`, `sim/*` | `MinedAt` tagging, `state_block`/`target_block`/`base_fee` routing, uncached historical pool reads, dedicated replay fork (§1.8) |
| Config | `config.rs`, `.env.example` | `POOL_DISCOVERY_V3` (default off), `ARB_MAX_CYCLE_LEN` (default 2, clamped to 2–5), `RELAY_TX_CONCURRENCY` (default 16), `REPLAY_FORK` (default on), `ANVIL_REPLAY_PORT` |

Two deliberate defaults: **V3 discovery is off** (nothing consumes the V3 cache
until W5) and **`ARB_MAX_CYCLE_LEN` is 2**, which makes the new search
reproduce the old pair-to-pair behaviour exactly. Nothing in this branch
changes what the bot does until someone flips a toggle, which is what the
"measure first" gate requires.

Beyond the specs as written, three problems were found and fixed while
implementing:

- **A failed `eth_getLogs` used to advance the scan cursor.** The old code
  could not tell an error from an empty range, so a single failed scan silently
  skipped those blocks forever. Same class of bug as the pool-level one the
  review caught, one level up.
- **Dust pairs were re-read every block, forever.** The old code deliberately
  never marked a filtered pair as seen, which is right for transient failures
  but meant every sub-0.5-WETH pair on mainnet — the long tail, hundreds a day
  — was re-fetched on every block. Now: non-WETH pairs are rejected permanently
  (the token pair is immutable), dust pairs are re-checked every 50 blocks, and
  only genuine RPC failures retry immediately.
- **Reorgs left a hole.** A monotonic cursor steps over a rewound range. The
  scan now re-covers a 12-block overlap; duplicate logs are idempotent, missing
  ones are not.

### Continuation fixes after the local build pass

Two follow-up correctness fixes are now part of the W2/W3 implementation:

- **The sniper had the same failed-scan cursor bug.** `sniper.rs` now uses the
  fallible shared scan, the same bounded/reorg-overlapping window as discovery,
  and advances its cursor only after `eth_getLogs` succeeds. A pool is marked
  seen only after its metadata read succeeds, so a transient pool RPC failure
  remains retryable.
- **V3 metadata and event boundaries are explicit.** `V3Pool` now records the
  creation block, `PoolCreated` decoding reads `blockNumber`, and both
  `PairCreated` and `PoolCreated` decoders reject the other event's topic0.

## 1.6 Verification status — read this before reviewing

| Component | Verified how |
| --- | --- |
| Frontend | **Fully verified.** `npx tsc --noEmit` clean and `npm run build` succeeds |
| Rust | **Locally verified by the maintainer.** `make bot-check` and `make bot-test` pass; the authoring sandbox has no Rust toolchain |
| Contracts | **Locally verified by the maintainer.** `make contracts` passes; the sandbox's solc-only artifact check also passes |
| Event topics | **Computed, not guessed.** `PoolCreated` topic0 was derived with keccak256 and the same method reproduces the repo's existing `PairCreated` constant exactly |
| Rust tests | **Executed by the maintainer.** `make bot-test` passes; the test runner, rather than the historical count in this document, is the source of truth for the exact number of tests |
| Remote CI | **Not enabled yet.** The workflow is prepared at `ci/github-actions-ci.yml`, but pushing it into `.github/workflows/` is still blocked by GitHub `workflows` permission |

The authoring sandbox still cannot reach the Rust distribution hosts and has no
`cargo`, `forge`, or `anvil` binaries, so it cannot independently reproduce the
maintainer's Rust and Forge runs. The local checks are a meaningful verification
of W1–W4, but they are not a substitute for required PR checks. **Do not mark
W0 complete until the workflow is enabled and green on the working branch.**

An automation session re-ran the parts it can run and recorded clean results:
the contracts solc-only artifact check (28 sources, `MevExecutor` runtime
9,618 B, `git diff --exit-code` on `bot/crates/mev-bot/artifacts` and
`contracts/abi` shows no drift) and the frontend (`npx tsc --noEmit`,
`npm run build`). (The runtime is now 9,577 B after the deterministic-artifact
fix described above.) It also bumped the frontend to `next@15.5.7` /
`react@19.1.2` to patch **CVE-2025-66478** (a CVSS 10.0 RSC RCE affecting the
previous `next@15.5.4` App-Router build) — see `docs/BUILD_NOTES.md`. These
re-verifications update the evidence for W1–W4; they do not enable remote CI,
which is still blocked as W0 describes.

The W1–W4 logic and tests have now had a compiler and test runner turn through
them locally. Any future CI failure should be treated as a real regression or
environment difference, not as an unverified baseline assumption.

## 1.7 Interaction with the bloXroute delivered-block ingestion

`main` now polls the bloXroute Max Profit relay for delivered blocks, fetches
each block's transactions, and routes **every one of them** through the same
`strategy → risk → simulation` path as a mempool transaction
(`TxSource::RelayDelivered`, `engine::evaluate`). As a source of Phase 1 replay
data that is exactly right. It creates two problems for Phase 2, both fixed on
this branch.

**1. It would have destroyed the funnel as a live instrument.** A mainnet block
delivers ~150 already-mined transactions every 12 seconds. Counted in the same
`FunnelCounters` as live mempool flow, replay traffic outnumbers it by an order
of magnitude, and every ratio the dashboard shows — and every before/after
comparison W4 and W5 are judged by — becomes a measurement of the backfill
instead. That is the W1 defect again, arriving through a different door.

The fix is a provenance split, not a switch: `FunnelLane::{Live, Replay}`,
chosen by `TxSource`, with two parallel counter maps. `/api/funnel` and
`/api/status` return `funnel` (live, unchanged shape) alongside `funnelReplay`,
and the dashboard has a lane toggle with the post-mortem lane clearly marked.
Both teams get what they wanted: relay transactions are scored and visible,
the live signal stays clean.

**2. It queues ~1000 tasks per block.** `on_relay_block` spawned one task per
strategy for every transaction with nothing between it and the runtime — a
structural version of the spawn-budget footgun in `MAINTAINING.md` §5, firing
every block rather than on a burst. `max_inflight_per_strategy` only bounds
simulations, so the strategy bodies and their RPC calls ran unbounded, which
would starve the latency-critical mempool path and trip provider rate limits.

Delivered blocks are already mined, so replay has no deadline and can trade
latency for footprint. `evaluate` now has two forms — a fan-out form for live
flow (unchanged, still returns immediately) and an awaited form for replay —
and the relay path runs the awaited form behind a `RELAY_TX_CONCURRENCY`
semaphore (default 16). Every transaction is still scored; they just no longer
arrive all at once.

## 1.8 Parent-block replay routing

The delivered-block backfill scored already-mined transactions against the
**current head**. That is not a rounding error — pool reserves, oracle prices
and the victim's own nonce have all moved on, which is why
`docs/BLOXROUTE_RELAY.md` originally listed nonce failures as a known
limitation. Every stage now routes to the transaction's own block:

1. **Tagging.** `MinedAt { block_number, base_fee_per_gas }` is stamped onto
   each delivered transaction from the block response that already carries it.
   `PendingTx::{state_block, target_block, base_fee}` derive the rest, and live
   flow is unchanged (`mined_at: None` → head).
2. **Historical reads.** `ctx.pool_at()` reads V2 pools at `B - 1` **without
   caching them** — the shared cache is head-shaped and `graph::search` prices
   all of it at once, so one historical entry would corrupt the live arb
   search. V3 state reads pin `eth_call` with `ctx.block_tag()`.
3. **Target routing.** `opp.target_block = B` reaches `consider` (risk-gated
   and costed at `B`'s base fee, not today's) and `Simulator::run`, which forks
   at `B - 1` and pins the relay cross-check to the same parent.
4. **Fork isolation.** Replay gets its own anvil on `ANVIL_REPLAY_PORT`,
   pinned with `ensure_fork_exact` (resets either direction). The live fork
   keeps its forward-only `ensure_fork_at`. Sharing one instance would
   `anvil_reset` in both directions on every alternating simulation, with the
   mempool path blocked behind the same mutex.

With `REPLAY_FORK=false` or anvil missing, delivered-block opportunities are
recorded and skipped with a stated reason rather than mis-scored against head
state.

## 2. Workstream summary

| ID | Workstream | Depends on | Size | Gate to start | Status |
| --- | --- | --- | --- | --- | --- |
| **W0** | Enable CI; get a green `cargo check` / `cargo test` / `forge test` | — | S | none | **🟡 all four CI jobs green, incl. `bot (rust)` (117/117 on `cargo test --all`); only remaining step: make the workflow a *required* PR check (maintainer with admin — branch protection is not settable by the automation token)** |
| **W1** | Fix funnel counter semantics + labels | W0 | S | none | **✅ implemented and locally verified** |
| **W2** | Harden V2 discovery; extract shared log decoding | W0 | M | none | **✅ implemented and locally verified** (including sniper retry fix) |
| **W3** | V3 pool discovery (`PoolCreated`) + separate V3 cache | W2 | M | none | **✅ implemented and locally verified** (shipped off) |
| **W4** | Multi-leg V2 atomic arb (3–5 legs) | W1, W2 | L | **1 week of funnel data** | **✅ implementation locally verified, shipped at 2 legs** (`ARB_MAX_CYCLE_LEN=2`) |
| **W5** | V3 sandwich sizing via QuoterV2 | W1, W3 | L | **1 week of funnel data** | **✅ implementation locally written, shipped off** (`STRATEGY_SANDWICH_V3=false`). Do not flip until the funnel week is read and `POOL_DISCOVERY_V3` is on. |
| **W6** | UniversalRouter calldata decoding | W1 | M | **funnel shows a public-mempool gap** | **✅ implementation locally written, shipped off** (`DECODE_UNIVERSAL_ROUTER=false`). Do not flip until the funnel shows a public-mempool gap. |

Sizes: S ≈ 1–2 days, M ≈ 3–5 days, L ≈ 1.5–2 weeks including tests and review.

### The one thing to do now

**W0's remaining step is administrative: make the workflow required.** All
four CI jobs — `contracts (foundry)`, `frontend (next.js)`,
`embedded bytecode is current`, and `bot (rust)` — are green on the working
branch. The artifact-drift job's original failure (checkout-path-dependent
bytecode from solc's IPFS metadata hash) is fixed, and the `bot (rust)`
`cargo test --all` failure turned out to be a deterministic wrong assertion
in `competition::tests::half_the_bid_is_unlikely` (expected `p ∈ (0.05,
0.20)`; the shipped `LOGISTIC_K = 2.2` gives `p = σ(-1.1) ≈ 0.2497` at half
the winning bid) — now corrected, with the dense-graph budget test's
wall-clock ceiling also hardened against slow shared runners. The next step
is for a maintainer with admin access to mark the CI checks required on PRs
to `main` (branch protection is neither readable nor settable by the
automation token); until then W0 stays open.

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

**Status: all four jobs green; one administrative step left.** The maintainer
granted GitHub `workflows` permission on 2026-08-21, so the workflow was moved
into `.github/workflows/ci.yml` and CI now runs on every push and PR. Results:
`contracts (foundry)` and `frontend (next.js)` pass; `embedded bytecode is
current` (artifact-drift) **failed on first enable** because `compile-check.js`
embedded solc's IPFS metadata hash (which includes absolute source paths) into
`MevExecutor.runtime.hex` — fixed by passing `bytecode_hash: "none"` and
regenerating the artifact (see `docs/BUILD_NOTES.md`). `bot (rust)` then failed
on `cargo test --all` (exit 101) — **diagnosed and fixed** (PR #15): the log
showed a single failing test, `competition::tests::half_the_bid_is_unlikely`,
whose `p ∈ (0.05, 0.20)` assertion contradicted the shipped `LOGISTIC_K = 2.2`
(`p = σ(-1.1) ≈ 0.2497` at half the winning bid, deterministically). The test
and its doc comment were corrected to the model; `LOGISTIC_K` was not touched.
The dense-graph budget test's 200 ms wall-clock ceiling was also relaxed to a
2 s catch-unbounded-search ceiling (plus a `MAX_CANDIDATES` sanity assert)
since tight wall-clock asserts flake on shared runners. `cargo test --all` is
now 117/117 on CI (Rust 1.98.0). W0 is **not** complete until the workflow is
also **required** on PRs to `main` — a maintainer with admin access must set
that (branch protection is neither readable nor settable by the automation
token, so it could not be verified or configured from here).

**Why first.** Remote CI is still the source of truth for required PR checks,
clippy, and the exact test environment. The local maintainer run substantially
reduces the remaining uncertainty, but it does not replace a green workflow.
Budget for environment-specific failures when the workflow is enabled; fix
those here rather than deferring them.

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

> **Status: implemented and locally verified.** `engine.rs` (`FunnelCounters`,
> `record_invocation`, `snapshot`), `api.rs` inherits the new keys via
> `snapshot()`, and the dashboard is updated and verified. Counters are
> additionally split by provenance into a live and a replay lane so the
> bloXroute delivered-block backfill cannot dilute them — see §1.7. The
> maintainer reports passing `make bot-check` and `make bot-test`; remote CI is
> still pending workflow permission.

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

> **Status: implemented and locally verified.** `strategies/discovery.rs` is
> behind a `DiscoverySource` trait with 11 network-free tests;
> `scan_factory_logs` is extracted in `strategies/mod.rs` and is used by both
> discovery and the sniper. Beyond the original spec, the implementation fixes
> failed-scan cursor advancement, unbounded dust retries, reorg overlap, and the
> sniper's equivalent retry/cursor bug. See §1.5.

`strategies/discovery.rs` is correct on the point that matters most — it only
inserts into `seen` *after* a successful fetch that passes the filters, so a
transient `fetch_v2_pool` failure or a rate-limited provider does not
permanently blacklist a pool. That invariant is load-bearing during provider
rate limiting and reorg recovery, and it is exactly what the review flagged.
**Do not "optimise" it by marking seen early.** The retry-after-failure path is
covered by the network-free discovery tests.

**Tasks**

1. **Deterministic tests.** Introduce a seam so discovery is testable without a
   live RPC — a trait over the two calls it makes (`scan_pair_created`,
   `dex::fetch_v2_pool`) with a fake in tests is the smallest change. Cover:
   - a transient `fetch_v2_pool` failure leaves the pair *unseen*, and the next
     block's scan retries it and succeeds;
   - the log window advances correctly (`last_log_block == 0` → `head - 50`;
     otherwise it advances with the bounded reorg overlap; a rewound or equal
     head re-scans the overlap instead of advancing a cursor);
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

> **Status: implemented, locally verified, shipped off.** `dex::V3Pool` (now
> including its creation block), `strategies::V3PoolCache`, topic-validated
> `decode_pool_created`, `try_scan_pool_created`, and
> `PoolDiscovery::discover_v3_with` are behind `POOL_DISCOVERY_V3` (default
> `false`). Turn it on when W5 needs it.

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

- `PoolCreated` topic/data decoding against a captured mainnet-shaped JSON
  fixture in the test module (no network).
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

> **Status: implementation locally verified, shipped at 2 legs.** `dex/graph.rs`
> plus `arb.rs::build_cycle_opportunity` are wired into `on_block` with the
> documented budgets. `ARB_MAX_CYCLE_LEN` defaults to `2`, which reproduces the
> previous pair-to-pair behaviour through the new code path — an equivalence
> test pins that. Raise it to 3–5 only after the funnel week, and record the
> before/after candidate volume.

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

> **Status: implemented, shipped off.** `strategies/sandwich_v3.rs` sizes
> via a `V3Quoter` trait (production = QuoterV2, tests = fake CP pool).
> `STRATEGY_SANDWICH_V3` defaults to `false`; the strategy is not
> constructed when the toggle is off, so it adds zero RPC. The pool must
> already sit in the W3 V3 cache (`POOL_DISCOVERY_V3`). Funnel row is the
> new `Strategy::SandwichV3` variant (`sandwich_v3`). Victim-revert trap,
> 12-call budget, router-routed legs with `amountOutMinimum = 0` on the
> back-run, and selector tests for both SwapRouter and SwapRouter02 are
> in the crate. **Do not flip the toggle until the funnel week is read.**

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

> **Status: implemented, shipped off.** `dex/calldata/universal_router.rs`
> decodes `execute(commands, inputs)` and the deadline overload for
> `V3_SWAP_EXACT_IN` / `V2_SWAP_EXACT_IN`, including a preceding
> `WRAP_ETH`. `decode_router(tx, weth, universal)` is the single entry
> point sandwich and arb consume; when `DECODE_UNIVERSAL_ROUTER` is
> false (the default) it is exactly `decode_swap`. Fixture tests cover
> five well-formed shapes plus malformed inputs that must return `None`.
> **Do not flip the toggle until the funnel shows a public-mempool gap.**

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

**Progress at this handoff update:** items W1–W6 have implementation. W0's
remote workflow is green; making it a required PR check is the remaining
administrative step. W4 remains at its compatibility default of two legs,
W5 is behind `STRATEGY_SANDWICH_V3=false`, and W6 is behind
`DECODE_UNIVERSAL_ROUTER=false`, until the funnel baseline is read for a
week (and, for W6, shows a public-mempool gap).

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
| `POOL_DISCOVERY_V3` | *(new in W3)* `false` | turn on together with `STRATEGY_SANDWICH_V3` |
| `STRATEGY_SANDWICH_V3` | *(new in W5)* `false` | V3 sandwich; adds a `sandwich_v3` funnel row |
| `DECODE_UNIVERSAL_ROUTER` | *(new in W6)* `false` | expands V2 sandwich / arb surface when on |
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
