# Optimizations — Rust backend + Next.js dashboard

Part 1 covers the Rust bot, part 2 the console. The console measurements were
taken on Next.js 15; the console now ships **Next.js 16.3.2** and
`frontend/package.json` is authoritative for versions. Both parts were verified
with the full CI suite plus live runs; see the verification sections.

---

# Part 1 — Rust backend optimizations

Work against the A / B / C list. Baseline before any change: `cargo build` clean,
**203 tests passing**. After: **216 tests passing**, release build clean, clippy
warning set byte-identical to baseline, all touched files `rustfmt`-clean.

Every new tunable defaults to the **previous behaviour**, so upgrading without
setting anything changes nothing operationally.

---

## A. High-impact

### 1. Hot-path spawn storm — `engine.rs`

**Was:** `evaluate()` spawned one task *per strategy per transaction*. Ten
strategies × ~200 txs = 2000+ spawns/block, each carrying a full `PendingTx`
clone (calldata included). `on_block` did the same per strategy.

**Now:**
- **One spawn per transaction.** Inside it a `JoinSet` runs the strategies
  concurrently — same parallelism (these are IO-bound futures), bounded fan-out,
  owned by a single task that can be dropped as a unit.
- **`Arc<PendingTx>`** instead of a clone per strategy: nine fewer copies of
  every transaction's calldata.
- **`Arc<Vec<Vec<u8>>>`** for victim raw bytes, which were previously re-cloned
  *per opportunity* — a widened search (multi-leg arb, V3) emits many
  opportunities from one victim.
- **A global `STRATEGY_CONCURRENCY` semaphore**, acquired with `try_acquire`.
  The live path never blocks: when saturated it sheds and counts
  (`evaluationsShed`) rather than queueing work that would complete after the
  block it was aimed at. That backlog used to be invisible inside an unbounded
  task queue.
- `on_block` likewise uses one task + `JoinSet` for the whole tick.

`evaluate_awaited` (replay) now shares the identical `fan_out` code path, so
live and replay stay scored the same way.

### 2. SQLite write pressure — `store.rs`, `engine.rs`

**Was:** every opportunity, simulation, bundle and block was a blocking `INSERT`
from the producing task, each its own implicit transaction, all contending on
one connection mutex.

**Now:**
- **`AsyncStore`**: a bounded channel + background writer on `spawn_blocking`.
  It drains up to 256 ops and commits them in a **single transaction** — one
  commit instead of N, which is where nearly all the saving is. Producers do a
  non-blocking `try_send`.
- **Bounded, drop-on-full.** Persistence here is observability, not settlement;
  the bot's decisions don't depend on these rows. A full queue drops and counts
  (`persistence.dropped`) instead of stalling the searcher. Surfaced on
  `/api/status` and `/api/metrics`.
- **Delivered blocks**: `record_relay_block_with_txs` writes the block and all
  ~200 transactions in one transaction with a `prepare_cached` statement,
  replacing ~200 separate commits every 12 s. Runs on a blocking thread.
- **Pragmas**: kept WAL + `synchronous=NORMAL`, added `wal_autocheckpoint`,
  a 5 s `busy_timeout` (batch commits vs. dashboard reads), an ~8 MB page cache
  and `temp_store=MEMORY` for the dashboard's scanning aggregate queries.

Hot-path SQL was refactored into connection-level helpers so the sync API and
the batched writer share exactly one set of statements.

### 3. Funnel counters — `engine.rs`

`parking_lot::RwLock<HashMap>` → **`dashmap::DashMap`** (`FunnelMap`). The funnel
is written on every strategy invocation — the hottest write path — and all ten
strategies serialised behind one writer lock for updates touching *disjoint*
keys. DashMap shards by key. The alert evaluator no longer clones the whole map
each tick. Covered by a new 8-thread concurrency test.

### 4. Pool discovery + inventory refresh — `engine.rs`

Both sat in front of the strategies on the block task, so their cost was latency
the strategies didn't get, and neither needs to run every block (inventory only
moves when the bot transacts; pools appear far slower than 12 s).

Added a `should_run` cooldown gate — `POOL_DISCOVERY_INTERVAL_BLOCKS` /
`INVENTORY_REFRESH_BLOCKS`, both defaulting to 1 (unchanged behaviour). A
**rewind always re-runs**: after a re-org the cached state was built against a
chain that no longer exists. Discovery's log cursor is range-based, so a skipped
block widens the next scan window rather than losing pools. Both are now timed
into new `discovery` / `inventory` latency histograms.

### 5. Replay isolation — `engine.rs`

**The real bug here:** `on_relay_block(...).await` ran *inline in the ingest
loop*. Scoring ~200 transactions plus fork resets stalled ingestion itself —
live mempool transactions sat unread in the channel behind a post-mortem of an
already-mined block.

**Now:** a dedicated replay worker with its own bounded queue
(`REPLAY_QUEUE_DEPTH`). Ingest does a non-blocking hand-off; a full queue skips
the block and counts `replayBlocksDropped`. `REPLAY_LANES` (default 1) controls
how many delivered blocks may score at once — documented as only safe above 1
with one isolated replay fork per lane. `reconcile_block` (synchronous SQLite,
up to 500 rows) moved onto the blocking pool in both call sites.

---

## B. Medium-impact

- **`ingest.rs` ring buffer**: `Vec` + `drain(..64)` → fixed-capacity
  `VecDeque`. The old drain shifted every remaining element left on each flush
  (O(n) memmove per batch, on the ingest task, at mempool rate). It was also
  **unbounded** — if hydration fell behind the feed, it grew until the process
  died. Now bounded at 2048 with oldest-first eviction, which is the correct one
  to lose: an un-hydrated hash that old is almost certainly mined or replaced.
- **`dex/graph.rs` runtime limits**: added `search_with`, exposing both budgets
  as `ARB_ENUMERATION_BUDGET_MS` (was a hard 25 ms) and `ARB_MAX_POOLS` (was a
  hard 200). `search` remains as a back-compatible wrapper.
- **Hot-path logging**: a `LogLimiter` (one line per second per site, reporting
  how many were suppressed) on the five highest-frequency `debug!` sites. The
  counts already live in the funnel, so the log's only job is a representative
  example. The load-shedding line was *raised* to `warn!` — it's actionable.
- **MEV Blocker support**: new `TxSource::MevBlocker` +
  `spawn_mev_blocker` subscribing to `mevblocker_partialPendingTransactions` at
  `wss://searchers.mevblocker.io`, off unless `MEV_BLOCKER_WS` is set. Correctly
  classified as a **live** funnel lane (not replay) — these aren't mined yet.
  Added `TxSource::backrun_only()` documenting the real constraint: the payload
  is **unsigned**, so these can only ever be back-run, never sandwiched. The
  existing "victim raw bytes required" gate enforces that automatically, so
  sandwich/JIT candidates self-reject rather than mis-simulating.

---

## New observability

`evaluationsShed`, `replayBlocksDropped`, `persistence.{queued,dropped}`, and
`discovery` / `inventory` latency stages — all on `/api/status` and exported to
Prometheus via `/api/metrics` automatically.

These are the three saturation signals the bot previously had no way to report:
strategy fan-out shedding, replay queue overflow, and persistence backlog.

## New tunables

All documented in `.env.example`; all default to prior behaviour.

`STRATEGY_CONCURRENCY=64` · `WRITE_QUEUE_CAPACITY=8192` ·
`REPLAY_QUEUE_DEPTH=4` · `REPLAY_LANES=1` ·
`POOL_DISCOVERY_INTERVAL_BLOCKS=1` · `INVENTORY_REFRESH_BLOCKS=1` ·
`ARB_ENUMERATION_BUDGET_MS=25` · `ARB_MAX_POOLS=200` · `MEV_BLOCKER_WS=`

## Tests

13 added (203 → 216), covering: cooldown gating incl. the re-org rewind case,
the log limiter, MEV Blocker lane classification, concurrent funnel updates
across 8 threads, the new saturation counters, batched relay-block inserts and
idempotency, async writer persistence end-to-end, and the guarantee that a full
write queue **drops rather than blocks**.

## Verification

Full CI-equivalent run **with Foundry 1.7.1 installed**:

| Check | Result |
| --- | --- |
| `cargo build` / `cargo build --release` (LTO + `panic=abort`) | clean |
| `cargo test --all` | **216 passed, 0 failed** |
| `cargo clippy --all-targets -- -A clippy::too_many_arguments` | **0 errors**, warning set identical to baseline |
| `forge build --sizes` | clean |
| `forge test` | **13 passed, 0 failed** |
| artifact drift (`git diff` on artifacts/abi) | no drift |

### Live end-to-end run against real anvil forks

A standalone `anvil` served as the chain; the bot forked from it, so the full
simulator path was exercised end to end.

- **Fork simulator spawned successfully** — no "anvil fork unavailable"; the
  fork bound its port and `prepare_state` completed. `doctor` passes every
  check including anvil detection.
- **1430 strategy invocations = 143 txs × 10 strategies** flowed through the
  new `JoinSet` fan-out. Funnel counters stayed consistent at 163 per strategy
  across all ten shards, confirming no lost updates in the DashMap swap.
- **`discovery` / `inventory` histogram counts exactly equalled `blocksSeen`
  (20 = 20)**, proving the cooldown default of 1 preserves every-block
  behaviour bit-for-bit.
- **Batched writer verified against the file on disk**: blocks queued through
  `AsyncStore` were read back out of the SQLite file by a separate read-only
  connection, `journal_mode=wal`, 0 dropped.

### Load-shedding stress test

Ran with `STRATEGY_CONCURRENCY=1` and flooded the mempool with 60 concurrent
real transactions:

- `evaluationsShed` incremented and the rate-limited `warn!` fired with the
  actionable "raise STRATEGY_CONCURRENCY" message and a correct `suppressed`
  count.
- The bot **stayed healthy throughout** — kept ingesting blocks, `persistence.
  dropped = 0`. This is the intended degradation: shed the excess, keep the
  searcher on-time, make the pressure visible.

## Not done

- **`spawn_blocking` for heavy strategies** (suggested in item 1): the
  strategies are IO-bound (RPC calls), not CPU-bound, so moving them to the
  blocking pool would add thread hops without removing work from the runtime.
  The `JoinSet` + semaphore addresses the actual problem, which was spawn
  volume. Worth revisiting only if profiling shows a strategy burning CPU rather
  than awaiting the network.
- **Work-stealing queue** (also item 1): tokio's multi-threaded runtime already
  work-steals across workers; a second layer on top would be redundant.

---

# Part 2 — Frontend optimizations

*(Measured on Next.js 15; the console now ships 16.3.2. The techniques and the
relative deltas carry over — the absolute route sizes below are from the
15-line build.)*

Baseline: `tsc --noEmit` clean, `next build` clean, route `/` at 209 kB.
After: both clean, route at 218 kB (+9 kB for the virtualizer), and the live
tape's DOM cost is now bounded by viewport height rather than buffer depth.

## High-impact

### 1. `LiveFeed.tsx` — virtualized tape

The tape buffers up to `FEED_MAX` (400) events and used to mount a `<tr>` for
every one: 400 rows, each rebuilding its explorer links, `shortHash` calls and
wei→ETH formatting, on every update — while ~14 are actually on screen.

Now virtualized with `@tanstack/react-virtual`:

- Only the visible window + 12 rows of overscan is mounted. **Measured in a
  real browser: 26 rows mounted against ~85 buffered.**
- Rows are `React.memo`'d on the event they render. Feed events are immutable
  once parsed, so a row still inside the window after a flush reuses its
  previous render instead of rebuilding.
- The list reads `useDeferredValue(events)`, so under a burst React can serve a
  slightly stale tape to keep the filter dropdown and the rest of the page
  interactive.
- Row height is measured from the DOM via `measureElement` rather than trusted
  from a constant, so a font or padding change cannot silently desynchronise
  the scrollbar from the content.
- Auto-scroll-to-top now only fires when the user is already at the top —
  scrolling back through history no longer yanks the viewport away.

### 2. SSE consumption — batched + typed (`lib/feed.ts`, `Console.tsx`)

The old handler called `setEvents` once per frame: one React render per event,
against a table that can only paint at 60 Hz.

New `useFeed` hook:

- Frames accumulate in a ref (no render) and flush on a 120 ms timer — one
  render per batch however many events arrived. **Measured: 105 SSE events →
  16 DOM mutations, a 6.5:1 reduction.** An idle feed skips the flush entirely.
- One reversal and one slice per *batch* rather than an array spread per event.
- **Typed parsing replaces `JSON.parse(...) as FeedEvent`**, which was a lie the
  compiler believed — a malformed frame became an object with a missing `kind`
  and crashed the renderer on a field it was promised existed.
  `parseFeedEvent` validates the discriminant and the fields each variant
  actually reads, returning `null` for anything else. A protocol change now
  degrades into a dropped row instead of a broken dashboard.

### 3. `FunnelPanel` + `RiskPanel` — memoization

- **FunnelPanel**: the ten-strategy row build and the eight-field aggregate
  reduce are now `useMemo`'d, and the whole panel is `memo`'d. Its **5 s
  `setTick` timer was deleted outright** — it re-rendered the entire panel on a
  cadence unrelated to the data, *on top of* the 4 s status poll that already
  delivers new counters as a prop. `SummaryStat` is memoized. The W6 card's
  three reduces are memoized too (its `Date.now()` uptime is deliberately left
  unmemoized so the "collecting (n/7 days)" badge stays honest).
- **RiskPanel**: took the whole `StatusResponse` but reads exactly one field.
  That object is rebuilt by every poll — a fresh identity even when nothing
  changed — so the form, its ten strategy toggles and three tabs re-rendered
  four times a second *while the user was typing into it*. The prop is now
  `killSwitchTripped?: boolean` and the component is `memo`'d.

### 4. Wallet / `ContractPanel` — cached reads

- **ContractPanel**: `read` depended on `target`, `address` and `publicClient`,
  and the 20 s interval was re-armed whenever that identity changed — so typing
  one character into the address box tore down the timer and fired a fresh
  round of `eth_call`s **per keystroke**. Reads are now cached per
  `(contract, account)` with a 15 s TTL, in-flight reads for the same key are
  joined rather than duplicated, the interval is armed once (reading current
  values through a ref), and it **skips while the tab is hidden**. Only explicit
  actions — the read button, or a completed write — force a refetch.
- **`lib/wallet.tsx`**: wallets fire `accountsChanged`/`chainChanged` more often
  than the underlying state changes (some on every focus; a chain switch emits
  both), and each event triggered a proxied `eth_getBalance`. Balance is now
  cached for 10 s. A real account change and the explicit `refreshBalance`
  bypass the cache; disconnect clears it.

## Additional wins found while working

- **`EquityChart`**: `memo`'d, with its 250-point cumulative reduce `useMemo`'d.
  An SVG chart is among the most expensive things on the page and its input only
  changes on the status poll — never on an SSE flush.
- **Identity-preserving state updates in `Console`**: every poll handed each
  `useState` a brand new array/object even on the ~95% of ticks where the bot
  returned identical rows, defeating every `memo` downstream. `keepIfSame` keeps
  the previous reference when the payload is unchanged, letting React bail out.
- **`safeHost` in the tape**: `new URL()` on a relay string throws on malformed
  input, and it ran inside a row's render path — one bad relay string used to
  take the whole tape down. Now tolerated.

## Verification

| Check | Result |
| --- | --- |
| `npx tsc --noEmit` | clean |
| `npm run build` | clean — route `/` 218 kB |
| Browser suite (Playwright, live SSE) | **10/10 passed** |

Browser checks, run against the real dashboard with the demo feed streaming:
tape virtualizes (26 mounted vs ~85 buffered); buffer materially exceeds the
DOM; content renders; scrolling swaps the virtual window; the window stays
bounded while scrolling; the bottom is reachable with a **0 px** residual gap
(measured atomically — the feed is live, so scrollHeight grows between a scroll
and a later read); the last row renders; `filter=block` shows only blocks;
resetting the filter keeps virtualization; and **no console errors or React
warnings** (hook order, duplicate keys).

One measurement subtlety worth recording: an early run reported a 31 px bottom
gap. That was the *test* racing the live feed, not drift — scrolling and
measuring in a single `evaluate` gives 0 px, and a static (filtered) list also
gives 0 px. The row-height constant was nonetheless corrected from an assumed
25 px to the measured 31 px, and `measureElement` now makes it self-correcting.

---

# Part 3 — Web3 / contract layer

The review's verdict that `MevExecutor.sol` is "already excellent" held up. It
is small, deterministic, and the profit guard and reentrancy protection are
sound. **Two of the three recommendations turned out to be actively harmful or
redundant, and are deliberately not implemented.** What follows is what the
evidence supported.

## Recommendations evaluated

### ❌ "Add a `receive()` fallback with a clear revert (currently only `fallback`)"

**Not implemented — the premise is inverted, and doing it would break live
funds.**

The contract already has `receive() external payable {}` and has **no**
`fallback()`. So the recommendation is backwards on both halves.

Worse, making `receive()` revert would be a real bug. `_settle` pays builder
bribes in ETH; when profit is denominated in WETH it calls
`IWETH.withdraw(bribe)`, and WETH9 pays out by sending ETH **straight back to
the executor** — through `receive()`. Proven by experiment: patching
`receive()` to revert turns the suite red with `withdraw failed` and breaks
`test_coinbaseBribeIsPaidFromProfit`. Every WETH-denominated bribe would
silently stop working. The same applies to any batch leg that unwraps WETH or
takes an ETH refund from a router.

The absence of `fallback()` is already correct: unknown selectors revert by
default, at zero bytecode cost.

**Done instead:** documented *why* `receive()` must stay permissive, and added
three regression tests (`test_wethProfitBribeRoutesEthThroughReceive`,
`test_plainEthTransferIsAccepted`, `test_unknownSelectorReverts`) so this
refactor cannot land unnoticed later.

### ❌ "Add more granular events (`ProfitRealized`, `BundleExecuted`)"

**Not implemented — pure cost, zero consumers.**

Two findings:

1. **Nothing consumes the contract's events.** Every `eth_getLogs` in the
   codebase is pool discovery or a lending protocol. The dashboard reads the
   bot's SQLite + SSE feed, never chain logs.
2. **They would duplicate `Executed`,** which already emits
   `(tag, profitToken, profit, bribe, gasUsed)`. `ProfitRealized` is a strict
   subset; `BundleExecuted`'s `gasUsed` is already there.

Measured cost of adding both: **+3,618 gas per execution** and +154 bytes
runtime. At 20 gwei and 100 landed bundles/day that is ~2.6 ETH/year, for logs
nobody reads. On a contract whose entire design goal is "never land
unprofitable", spending gas on redundant logs is the wrong trade.

If a log consumer is ever built, `Executed` already carries the data; the right
move then is to index it off-chain, not to emit more.

### ✅ "`quote()` is underused — expose it more clearly for off-chain sizing"

**Implemented — this one was right, and there was a real usability bug behind
it.**

`quote()` is gated on `msg.sender == address(0)`, which is correct and load
bearing: the function executes the batch for real before reporting the delta,
so reachability from a transaction would let anyone move the contract's funds
through arbitrary calls. `address(0)` cannot originate a transaction, so the
body can only run inside a simulated call. Verified against a live chain: an
`eth_call` with no `from` reports `msg.sender == address(0)`; with `from` set it
does not.

The problem: **a lot of tooling cannot omit `from`.** Wallets, block explorers,
viem's `simulateContract`, and several RPC providers all inject a sender, and
for them `quote` is simply unreachable.

- Added **`quoteFrom(calls, profitToken)`** — the identical dry-run, gated on
  `onlySearcher` instead of `address(0)`. Keeps the arbitrary-calls surface
  closed to the public while letting an allowlisted operator size from a wallet
  or an explorer. Both share one `_quote` internal, so they cannot drift; a test
  asserts they return identical deltas for the same batch.
- Rewrote the NatSpec to say *how to call it* (omit `from`, pair with state
  overrides), why the gate exists, and what to use when you can't omit `from`.
- Documented `encode_quote` in `bundle.rs` (it was dead code with no
  explanation) and added `encode_quote_from`.
- **Surfaced it in the dashboard**: a "dry-run quote" control in the executor
  panel, issuing an `eth_call` with no `from`. Verified in a browser against a
  locally deployed executor — returns `delta 0 ETH · gas 456`, matching `cast`
  exactly.

## Improvements found independently

### Custom errors instead of require-strings (−139 bytes)

`"sweep failed"`, `"bribe failed"` and `"transfer failed"` became
`SweepFailed()`, `BribeFailed()` and `TransferFailed(token, to, amount)`.
Smaller runtime, and the ERC20 failure now names *which* token and amount
failed instead of a bare string.

**The bot's revert decoder was updated in the same commit** — otherwise the
console would have regressed from a named reason to a raw selector, which is
precisely the diagnostic the funnel depends on. All four new selectors were
cross-checked three ways: `cast sig`, an independent keccak implementation, and
a live revert from a deployed contract.

### Corrected a false comment on the transient-storage slots

The three guard slots carried `// keccak256("jerseymikes.…")` comments. **They
are not those hashes** — verified in Python and again in Solidity;
`keccak256("jerseymikes.reentrancy.guard")` is `0x0217913e…3090`, not the value
in the file. They are hand-typed constants.

**The values are left byte-for-byte unchanged.** They are already correct for
their purpose — transient storage is private to the contract, all three are
distinct, and each is far outside the range the compiler would assign — so
changing them would alter runtime bytecode for no benefit. Only the false claim
about their derivation is fixed.

### Considered and rejected: splitting `_run`

`_run` allocates a `bytes[]` of every leg's returndata, and only `ownerCall`
uses it — `execute` and `quote` discard it. Splitting into allocating and
non-allocating variants saves ~200 gas on `execute` but costs **+319 bytes** of
runtime (duplicated loop). Measured both ways, including a single-runner
variant. The optimizer was already handling this well; not worth the size.
Recorded here so nobody re-derives it.

## Net effect

| Metric | Before | After | Δ |
| --- | --- | --- | --- |
| Runtime size | 9,577 B | 9,599 B | **+22** (−139 errors, +161 `quoteFrom`) |
| `execute` gas (min/max) | 25,543 / 120,494 | 25,543 / 120,494 | **identical** |
| `flashExecute` gas (min) | 211,794 | 211,807 | +13 (+0.006%) |
| `sweep` gas | 24,118 | 24,118 | identical |
| Solidity tests | 13 | **27** | +14 |
| Rust tests | 217 | **218** | +1 |

(Sizes as measured at the time of that change. The contract has grown since;
`forge build --sizes` reports the current runtime at **11,497 bytes**, well
under the 24,576-byte EIP-170 limit.)

The hot path is untouched: `execute` costs exactly what it did before. The
+13 gas on `flashExecute` is the larger error-selector table, and the +22 bytes
buys a genuinely reachable `quoteFrom` plus better diagnostics.

## Verification

- `forge build --sizes`, `forge test` → **27 passed**
- `cargo test --all` → **218 passed**; `cargo clippy` → 0 errors
- `tsc --noEmit`, `next build` → clean
- Artifacts regenerated; **forge and solc-js produce byte-identical runtime
  bytecode**, so deterministic compilation is preserved and the CI drift job
  passes
- Live-chain checks against a deployed executor: `quote` succeeds with no
  `from` and reverts `QuoteIsEthCallOnly` with one; `quoteFrom` succeeds for the
  owner and reverts `NotSearcher` for a stranger
- Browser check of the new dashboard control against that same deployment

---

# Security & dependency audit

A full-repo audit run after the contract work. Two issues were found by
reading, then **reproduced against a running bot** before being fixed — the
first one is not theoretical.

## 1. Unauthenticated mutating API on a public bind (HIGH — fixed)

`API_BIND` defaulted to `0.0.0.0:8080` and the API applied
`CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any)`,
with no authentication on any route.

Reproduced end-to-end against a live bot bound to `0.0.0.0`, from a
non-loopback interface, with `Origin: https://evil.example`:

| Request | Result before |
| --- | --- |
| `POST /api/risk {"bribeBps":10000}` | **accepted** — 100% of gross profit redirected to the block builder |
| `POST /api/risk {"maxPositionWei":1}` | **accepted** — sizing throttled to 1 wei |
| `POST /api/risk` (strategy list) | **accepted** — `sandwich` and `atomic_arb` switched off |
| `POST /api/risk/reset` | **accepted** — kill switch cleared remotely |
| response header | `access-control-allow-origin: *` — so any page the operator had open could do all of the above from their browser |

`POST /api/mode {"live":true}` was correctly refused: arming live execution
needs `LIVE_EXECUTION=true` + `I_UNDERSTAND_LIVE_RISK=yes` at boot. Remote
*arming* was never possible; remote *sabotage* was.

The fix is three independent layers, so no single mistake re-opens it:

1. **Safe default** — `API_BIND` is now `127.0.0.1:8080`. The console reaches
   the bot server-side through its `/api/bot/*` proxy, so nothing breaks.
2. **Fail-closed boot check** — `Config::validate` refuses to start when the
   bind is non-loopback and `API_AUTH_TOKEN` is unset, naming both remedies.
   `bind_is_loopback` treats unparseable binds as exposed.
3. **Bearer auth + closed CORS** — the three mutating routes sit behind a
   constant-time bearer check (`API_AUTH_TOKEN`); reads stay open. CORS now
   reflects only `API_ALLOWED_ORIGINS`, empty by default.

Reads are deliberately still unauthenticated: they are what the dashboard
renders, and gating them would break the demo flow for no security gain.

**Scope of the boot check.** Only the commands that actually bind the port
(`run`, `api`) call `validate()`. The first cut called it from
`Config::from_env`, which meant every subcommand inherited the rule —
including `doctor`, the command you run *precisely* to diagnose a bad config,
and `replay`, which opens no socket. `doctor` now reports the problem as a
`✗ api bind` check line alongside the RPC and relay checks and still exits 0.

Verified after the fix, same external interface:

| Case | Result |
| --- | --- |
| `GET /api/status` | 200 — reads unaffected |
| `POST /api/risk` no token | **401** |
| `POST /api/risk` wrong token | **401** |
| `POST /api/risk` correct token | 200, applied |
| CORS header | absent |
| loopback, no token | fully open, unchanged — backward compatible |
| non-loopback, no token | **refuses to boot** with actionable guidance |
| console proxy + `BOT_API_TOKEN` | drives the bot normally; token absent from `.next/static` and served HTML |

`deploy/docker-compose.yml` needs `0.0.0.0` inside the container for the
console to reach the bot, so it now sets `API_AUTH_TOKEN` (a `:?` guard fails
the run with a named error if it is missing) and publishes the port on host
loopback only.

## 2. Flashbots signer key reachable via `Debug`/`Serialize` (MEDIUM — fixed)

`Endpoints` derived both `Debug` and `Serialize` while holding
`flashbots_signer_key`. Nothing printed it today, but the struct is cloned
into every task and one `tracing::debug!(?cfg)` would have put a live key in
the logs. Now `#[serde(skip_serializing)]` plus a hand-written `Debug` that
renders `<redacted>`, mirroring the existing treatment in `signer.rs`. Two
tests lock the behaviour in.

## 3. Dependencies

- **Rust:** `cargo audit` → **0 vulnerabilities** (3 unmaintained transitive
  crates: `derivative`, `paste`, `proc-macro-error2` — no action available).
- **Frontend:** `npm audit` 5 → **3** via `next` 15.5.7 → 15.5.23 and
  `viem` → 2.55.19, both patch-level. That cleared the Next.js SSRF, cache
  poisoning, XSS, DoS and middleware-bypass advisories plus `ws` via viem. The
  last 3 (`postcss`, `sharp`) were nested inside `next`'s own tree and needed
  the breaking major; that upgrade has since been taken — the console is on
  **`next@16.3.2`** with `react`/`react-dom` 19.2.8, and the tree is clean.

## Checked and found sound

Parameterised SQL throughout, clamped API `limit`s, thorough `RuntimeRisk::apply`
validation, the read-only 17-method `/api/eth` proxy, `.gitignore` coverage, and
no committed secrets (every 64-hex string in the tree is an event topic hash, a
transient-storage slot constant, or the public anvil test key).

## Verification

- `cargo test --all` → **223 passed** (218 + 5 new security tests)
- `cargo clippy --all-targets -- -A clippy::too_many_arguments` → clean
- `rustfmt --check` clean on every file touched
- `forge test` → **27 passed**; `tsc --noEmit` and `next build` clean
- Playwright smoke against `next start` → PASS, 0 console errors
