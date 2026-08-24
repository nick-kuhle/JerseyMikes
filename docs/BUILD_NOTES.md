# Build & verification notes

> Engineering log. Current supported versions, deployment, and execution
> behaviour are defined by `SETUP.md`, `DEPLOYMENT.md`, and `SIM_TO_LIVE.md`;
> earlier entries are retained as provenance for decisions that are still
> load-bearing.

## Current verification status

Every gate below is run before a branch is proposed for merge, and again by CI
on the merge commit. See "What CI verifies" for the authoritative command list.

| Gate | Command | Result |
| --- | --- | --- |
| Rust format | `cargo fmt --all -- --check` | clean |
| Rust lints | `cargo clippy --all-targets -- -A clippy::too_many_arguments` | clean under `#![deny(warnings)]` |
| Rust tests | `cargo test --all` | **282 passed, 0 failed** |
| Contract format | `forge fmt --check` | clean |
| Contract build | `forge build --sizes` | `MevExecutor` runtime **11,497 bytes** (limit 24,576) |
| Contract tests | `forge test -vvv` | **32 passed, 0 failed** |
| Artifact drift | `node script/compile-check.js` + `git diff --exit-code` | no drift |
| Frontend | `npx tsc --noEmit`, `next build` | clean on `next@16.3.2` |

Toolchain: Rust stable (1.90+), Foundry stable (1.7+), Node 22, solc 0.8.30.
`make doctor` checks all four are present and reports versions.

## 2026-08-23 — console: strategy eligibility and uncertified results

The bot has reported per-strategy eligibility on `GET /api/config` for some
time — a `strategyEligibility` array of `{name, liveCandidate,
shadowOnlyReason}` built from `Strategy::all()`. Nothing consumed it. The
console showed only the qualification report, so the two questions an operator
has to keep apart were collapsed into one:

- **Eligibility** is a build-time property (`Strategy::live_candidate()`). It
  never changes at runtime, and a shadow-only row is blocked by how its
  opportunities settle or must be ordered.
- **Qualification** is an evidence property earned over the 168-hour window,
  and it moves in both directions.

Without the first, a shadow-only strategy sits at `PENDING` indefinitely and
looks exactly like a row that merely needs more soak time. That is a week of an
operator's patience spent waiting for evidence that cannot arrive.

**`frontend/components/EligibilityPanel.tsx` (new).** Renders the array as
live-eligible rows first, then shadow-only rows with their reason. It fetches
once on mount rather than on the console's 4s tick — the answer is boot-fixed,
and `Console` already re-keys its whole panel tree on chain change, so a chain
switch remounts the panel and re-fetches. Placed directly beneath the
qualification report in the go-live section, where the distinction is
load-bearing. A bot that predates the field renders an explicit empty state
rather than an invented eligibility claim.

Two supporting changes:

- `frontend/lib/types.ts` gains `StrategyEligibility` and a full
  `ConfigResponse` (the config route had no type at all).
- The demo fallback in `app/api/bot/[...path]/route.ts` now serves the same
  array, with the reason strings **copied verbatim** from
  `Strategy::shadow_only_reason()` so the no-bot view cannot drift into saying
  something the bot would not.

**Uncertified results are no longer indistinguishable from no result.** The
simulations table rendered `revertReason ?? "no edge"` into a `nowrap` cell.
When valuation cannot price a profit token, the reason is a full sentence
beginning `uncertified accounting:`, so it both overflowed and read like an
ordinary miss at a glance. These mean opposite things: a revert is nothing
there, while uncertified means the bundle may well have been profitable but the
bot refuses to claim a number it cannot certify — usually fixed by
`TOKEN_VALUATION=true`, not by strategy work. The new `SimVerdict` cell shows a
distinct amber `uncertified`, truncates other long reasons, and keeps the full
text on hover.

Gates: `npx tsc --noEmit` and `next build` clean on `next@16.3.2`. No Rust or
Solidity source changed, so those gates are unaffected.

## 2026-08-23 — non-native profit valuation, liquidation promotion, Flashblocks ingest

Three changes that together close the gap between "the liquidation math is
tested" and "a liquidation can actually be bid".

**1. Non-native profit tokens are now priced (`valuation.rs`, new).**
`sim/anvil.rs` measured a bundle's outcome in native balance delta only. A
strategy whose profit arrives as a collateral token — which is every
liquidation — therefore settled with `net_profit_wei = 0` and
`success = false`, and `risk.rs::submittable()` refused it. Every liquidation
row was structurally unbiddable, regardless of how profitable it was.

`value_in_native(rpc, cfg, cache, token, amount, block, haircut_bps)` prices
one whole unit of the token at the **pinned pre-bundle fork block** and scales,
so the valuation is consistent with the simulation it is valuing. Route order
is Uniswap V3 QuoterV2 across the four canonical fee tiers (100 / 500 / 3,000 /
10,000, best quote wins) → Uniswap V2 `getReserves` → `None`. It is
**fail-closed**: no route means no value, which means no bid, never a guess. A
configurable haircut (`VALUATION_HAIRCUT_BPS`, default 200 = 2%) discounts the
quote for the depth the quoter does not model. Decimals are read on chain
(`0x313ce567`) and cached; stablecoins short-circuit. Results are cached per
`(token, block)`, negatives included, and pruned as the fork advances.

Wired into `sim/anvil.rs` behind `TOKEN_VALUATION`, which defaults to **off**
in keeping with the rest of the risk surface — an unpriced non-native profit
stays uncertified until an operator opts in. With it on, the accounting
now adds the valued non-native delta to the native delta, and a new `certified`
flag replaces `native_accounting` in the success predicate — a bundle is
certified when its profit is either native or priced by a route we trust.

**2. The four liquidation strategies were promoted to live candidates**
(`types.rs`). Aave, Compound V3, Morpho Blue and Maker had been shadow-only for
exactly one reason: the valuation gap above. With it closed, the reason is
gone, and roughly 3,000 lines of tested liquidation math became eligible to
bid. `shadow_only_reason()` now covers only JIT, the sniper, and oracle
front-running, whose limitations are settlement- and ordering-shaped and are
not fixed by pricing.

Promotion means *eligible*, not *approved*: each row still has to earn a `PASS`
from `qualification.rs` on its own evidence before it can broadcast. Nothing
about the nine broadcast gates changed.

**3. Flashblocks ingest (`ingest.rs`, `rlp.rs`, `config.rs`).** Base seals a
preconfirmed Flashblock every 200 ms against a 2 s full block — 10× the event
resolution — and the Flashblock diff is the only Base feed that carries **raw
signed transaction bytes**, which is what a bundle needs to transport a target
transaction. `spawn_flashblocks` subscribes over raw WebSocket
(`eth_subscribe ["newFlashblocks"]`, `FLASHBLOCKS_WS_URL`) on the existing
`WsSubscription` backoff machinery, and `parse_flashblock` normalises the diff
into `PendingTx` with `TxSource::Flashblock`, which is already `backrun_only()`
everywhere it is routed.

That required a raw-transaction decoder the crate did not have:
`rlp::decode_raw_transaction` handles legacy and typed (0x01/0x02/0x03)
envelopes and recovers the sender with `k256` ECDSA public-key recovery. The
parser tolerates the shapes the feed actually emits (diff at top level or under
`diff`; entries as hex strings or as objects with `rawTransaction`/`raw`/
`input`) and **skips anything it cannot decode rather than guessing** — a
half-understood victim transaction is worse than a missed one.

**Verification.** `cargo fmt --all -- --check` clean; `cargo clippy
--all-targets -- -A clippy::too_many_arguments` clean under
`#![deny(warnings)]`; `cargo test --all` **282 passed, 0 failed** (+15 new:
7 for the RLP decoder, 5 for the Flashblock parser, 7 for valuation routing,
caching, haircut arithmetic and overflow). `forge fmt --check`, `forge build
--sizes` and `forge test` clean, 32 passed. Both formatter checks were made
**blocking** in CI in the same change (they had been `continue-on-error`).

**Known follow-on.** `Opportunity.profit_token` exists and is persisted, but
most construction sites still hardcode `Address::ZERO`. Valuation is therefore
correct and tested but not yet exercised end to end: the liquidation strategies
must set `profit_token` to the collateral they seize before the new path does
any work in production. Tracked in `docs/ROADMAP.md`.

## 2026-08-22 — Aave per-reserve liquidation config

Retired the last documented assumption in the Aave strategy: positions are
composed from their actual reserves before a bundle is built.
`Pool.getUserConfiguration` (bitmap over `getReservesList`) selects the
reserves a user touches, batched `DataProvider.getUserReserveData` supplies
the real balances, `getReserveConfigurationData` the real liquidation bonus
(cached per block; instance-owned caches so live/replay lanes stay
disjoint). The bundle repays the largest actual debt against the largest
actual collateral, sized with the real bonus; near-miss leads publish the
real collateral so oracle back-runs match on the right feed; the oracle
path re-composes at trigger time. Close factor stays the documented
HF-based simplification. All four ABI shapes verified against the live
pool implementation (`0x728a138A…`) and data provider before coding.

**Verification:** `cargo test --all` 203 passed (4 new: bitmap decode,
reserves-list decode, reserve-config word decode, bonus math); clippy
clean. Docs: STRATEGIES.md §4, RISK.md limitation updated.

## 2026-08-22 — alerting + deployment units

Phase 3's ops items: a rule engine over live engine state (`alerts.rs`,
evaluated on `ALERT_EVAL_SECS`), a Prometheus text endpoint
(`/api/metrics`, `metrics.rs` — a JSON→samples renderer plus labelled
per-strategy funnel lines with a `lane` so the two lanes are never summed),
`GET /api/alerts`, alert transitions on the SSE feed, optional webhook
delivery, systemd units + Dockerfiles/compose under `deploy/` (the bot image
ships anvil from the official Foundry image), and `docs/DEPLOYMENT.md` with
a scrape config and starter alert expressions. Log shipping is delegated to
journald / the Docker logging driver — one logging system, in the platform.

Verified: `cargo test --all` 199 passed (10 new: rule table incl. threshold
edges and the boot-noise cases, lifecycle fire/resolve, Prometheus renderer
incl. labelled funnels and label escaping); clippy clean; compose YAML parsed
before commit (the CI-workflow lesson, applied). Still unproven at the time:
the systemd units and images themselves — they need a host with
systemd/Docker; `docs/DEPLOYMENT.md` is the runbook.

## 2026-08-22 — fixture-executor WETH funding + console nav/collapse

**Every sandwich/sniper/JIT leg-0 `CallFailed ... reverted bare`.** Decoded
chain of evidence: `build_leg`'s first call is `token_in.transfer(pair, …)`
— WETH for those strategies — the strategies are deliberately not
flash-funded, and `prepare_state` funded only ETH, never WETH. WETH9's
`transfer` is a bare `require(balanceOf >= wad)`, reproduced against a live
anvil fork as `execution reverted, data: "0x"`. Fix: the **fixture**
executor (no `EXECUTOR_ADDRESS`) is now topped up with 10k WETH via an
impersonated `WETH.deposit` after every (re)fork. The first version of the
fix had a bug the live probe caught immediately: depositing *exactly* the
funded balance fails anvil's `gas * price + value` check — the executor is
bumped to 30k ETH first. Verified end-to-end on a real fork: setBalance →
deposit accepted → `balanceOf` = 10,000 → leg-0 transfer admitted. A real
`EXECUTOR_ADDRESS` is deliberately NOT topped up (its forked balance is the
honest live-readiness picture — fund it for real before live sandwich/JIT).

**Console declutter** (the "scroll gets stuck inside a card" report): a
sticky jump nav (P/L · Activity · Transactions · Relay · Funnel · Controls ·
Go live · Executor) with smooth scrolling, collapsible sections (relay
blocks and go-live default-collapsed, the rest open), so the page between
tables is short instead of one endless scroll past embedded scroll areas.
Also: the transaction-history strategy filter was still missing the four
new liquidation rows (a leftover from the union widening — the `<select>`
literal is not a `Record` so tsc never flagged it).

**Verification:** `cargo test --all` 189 passed; clippy clean; tsc clean;
`next build` succeeds; the WETH sequence proven on a live anvil fork.

## 2026-08-22 — CallFailed inner decoding + risk-panel patch poisoning fix

Two operator reports, one PR:

1. **`CallFailed(index=0)` stopped one level short.** The executor reverts
   with the failed leg's *raw* revert bytes (`revert CallFailed(i, ret)`),
   and the decoder discarded them. It now unpacks the `bytes returndata`
   argument (tuple-head offset/length, all bounds-checked — a decoder must
   never panic on hostile bytes) and decodes it recursively (depth-capped
   at 3). Sniper funnel rows now read e.g. `CallFailed(index=0):
   Error("UniswapV2: K")` — which is exactly how a honeypot, a transfer
   tax and a timing miss are told apart.
2. **One bad env value vetoed every dashboard edit.** `MAX_GAS_PER_BUNDLE`
   set above the 30M ceiling (the same fat-fingered value behind the
   earlier "intrinsic gas too high" session) flowed from `.env` into
   `GET /api/risk`, and the panel re-sent *every* field on each edit — so
   each patch failed validation ("outside [21000, 30000000]"), nothing
   applied, and the form snapped back. Fixed on both sides: the env value
   is clamped into the validator's range at boot (with a warning naming
   the variable), and the panel now sends **only the fields that moved**
   since the last applied state, so a stale value can never veto an
   unrelated edit.

**Verification:** `cargo test --all` 189 passed (4 new: inner
Error(string), inner custom error, bare/short inner data, malformed
offsets never panic); clippy clean; `tsc --noEmit` clean; `next build`
succeeds.

## 2026-08-22 — runtime risk envelope + go-live console surface

Operator request: make the risk envelope instant from the console (it was
env-and-restart), clean the UI up for go-live, and spell out which web3
functions the live path actually needs.

**Backend** (`risk.rs`, `engine.rs`, `sim/*`, `api.rs`):

- `RuntimeRisk` — a shared, lock-protected risk envelope + strategy-toggle
  set, seeded from the environment and mutated by `POST /api/risk`.
  Patches validate completely before applying (all-or-nothing), and
  strategy toggles can only narrow: enabling a strategy that was not
  constructed at boot is refused with the restart instruction, mirroring
  the live-mode switch. The risk engine, the fork simulator's
  `minProfit`/`bribeBps` guards, the gas clamp and the signed-bundle gas
  cap all read this same state, so a dashboard change is in force for the
  next opportunity, not the next boot.
- `GET /api/risk` (effective + boot + per-strategy enablement + kill
  switch), `POST /api/risk` (patch), `POST /api/risk/reset` (re-arm the
  drawdown kill switch). `/api/status` now reports the runtime-effective
  set (`strategies`) alongside `bootStrategies`, and the full risk block;
  `/api/config` exposes the searcher EOA for the deploy panel's prefill.

**Frontend:**

- `RiskPanel` controls tab rebuilt as instant-apply (debounced 500 ms,
  full-patch POSTs, applied/error status line, demo-flag aware); the
  kill switch got a card with its reset; the `.env` snippet is labelled
  "persist current values as boot defaults". Diagnostics/sources tabs
  untouched.
- `GoLivePanel` (new) — `GO_LIVE.md` Path A as six gated steps: wallet on
  chain 1 (with one-click switch), gas balance, deploy (free
  `eth_estimateGas` estimate; browser sends creation bytecode +
  ABI-encoded `MevExecutor(balancerVault, weth)` args; receipt followed;
  address remembered in localStorage), fund the executor, `setSearcher`
  (prefilled from `/api/config`), verify (code/owner/allowlist reads) +
  copy the `EXECUTOR_ADDRESS` line. Reads ride the read-only `/api/eth`
  proxy (`eth_estimateGas` added to its allowlist; write methods still
  refused).
- The creation bytecode is a byte-for-byte copy of the bot artifact at
  `frontend/lib/MevExecutor.creation.hex`, and the artifact-drift CI job
  now diffs that copy against the artifact — the panel can never deploy
  bytecode the bot does not simulate against.

**Post-merge-fix note:** the first push of this PR broke CI *silently* —
the new artifact-drift step was written through a patch script whose Python
string continuation ate the YAML newlines, the workflow file stopped
parsing, and all four checks sat at "Expected — waiting for status"
forever (no job ever started). Nothing local catches that: the Rust and TS
toolchains validated every other file, but no tool parses
`.github/workflows/*.yml`. Fixed, and the step's shell logic was then
executed locally in both directions (identical copies pass; a drifted copy
fails with the message) before pushing, along with a full YAML parse.
Lesson recorded: validate workflow YAML locally before every push.

**Verification:** `cargo test --all` 185 passed (5 new: patch apply/read,
atomic rejection, wei parsing, narrow-only toggles, engine gating on
runtime values); clippy clean on touched files; `npx tsc --noEmit` clean;
`next build` succeeds; production server smoke: GET/
POST `/api/bot/risk` + reset + strategy toggles round-trip (demo fallback
included), page renders, `/api/eth` forwards estimateGas and refuses
`eth_sendTransaction`.

## 2026-08-22 — sim diagnostics: gas-limit clamp + revert-reason decoding

Operator-reported symptoms after running the merged liquidation coverage:

1. `back-run rejected: rpc error: {"code":-32000,"message":"intrinsic gas
   too high -- tx.gas_limit > env.block.gas_limit"}` — reproduced exactly
   against a live anvil 1.7.1 fork: `eth_sendTransaction` with a `gas`
   above the fork's block gas limit is rejected before execution. Mainnet's
   block gas limit is **60M** in 2026 (measured live; the fork adopts it),
   so the default `MAX_GAS_PER_BUNDLE=3M` can never trip this — the
   operator's config has it set above the limit (or attaches to an anvil
   started with a low `--gas-limit`).
2. `tx 0x63438a…f3de19 reverted` — the hash does not exist on mainnet: it
   is a fork-local hash for an executor leg injected via
   `eth_sendTransaction`. The tx was *admitted* but mined with status 0 —
   the executor's profit guard or a protocol rejection doing its job — and
   the old receipt loop recorded only "tx … reverted", with no reason.

**Fixes** (all in `sim/anvil.rs` + one method on the RPC client):

- The fork reads its block gas limit after every (re)fork and executor txs
  are clamped to 95% of it (5% headroom for the victim txs sharing the
  mined block). A boot-time warning names `MAX_GAS_PER_BUNDLE` when
  clamping engages instead of discovering it one rejected back-run at a
  time. The signed-bundle path (`eth_callBundle`) clamps to the EIP-7825
  per-transaction protocol cap (16,777,216 gas, live since Fusaka
  2025-12-03) — every txpool, builder and relay rejects over-limit txs the
  same way the fork does.
- Status-0 receipts are re-executed via `eth_call` while the simulation
  snapshot is still live, and the revert bytes are decoded: Solidity
  `Error(string)`/`Panic`, the executor's own guards
  (`Unprofitable(realised=…, required=…)` with the guard named in prose),
  and the protocols' known rejections (`HEALTHY_POSITION`, `NotForSale`,
  `TooMuchSlippage`, `NotLiquidatable`, `MARKET_NOT_CREATED`,
  `LiquidationCallFailed`, `Paused`). Unknown selectors render as
  `custom error 0x…(args)` instead of a bare "reverted". Selector constants
  are generated by `sol!` at compile time (keccak), not hand-copied.
- Victim-tx reverts are labelled distinctly ("the target's own protection
  fired; the bundle is invalid by design") so the funnel separates
  victim-revert traps from our-leg rejections — the victim-revert trap the
  sandwich docs describe, now visible in the data.
- `RpcClient::call_raw_with_error` returns the JSON-RPC error object
  (code/message/data) instead of flattening it, which is what makes the
  revert bytes reachable.

**Verification:** `cargo test --all` — 180 passed, 0 failed (9 new: the
clamp table including the reproduced 70M/60M case, decoder cases for
Error/Panic/Unprofitable/known selectors/unknown-selector/empty, and the
floor invariant). Clippy clean on the touched files. The rejection itself
was reproduced against a real anvil 1.7.1 fork before writing the fix, and
the fork's gas-limit adoption (60M) measured live over a public RPC.

## 2026-08-21 — liquidation coverage: Compound V3, Morpho Blue, Maker + oracle front-running

Implements the Phase 2 roadmap line "Compound V3, Morpho and Maker
liquidations; oracle-update front-running". Four new strategy rows
(`liquidation_compound`, `liquidation_morpho`, `liquidation_maker`,
`oracle_frontrun`), a shared near-miss leads registry
(`strategies/leads.rs`) that connects the health-polling strategies to the
oracle front-runner, four env knobs (`.env.example` "LIQUIDATION COVERAGE"
section), and the frontend funnel rows. `docs/STRATEGIES.md` §4b–4e documents
each strategy; `docs/ROADMAP.md` ticks the box.

**Interface verification done before writing code.** Nothing in this change
trusts remembered ABIs:

- Every Morpho selector was confirmed against the deployed singleton's
  bytecode dispatcher (`0xBBBB…FFCb`, immutable): the live interface is
  **v1.1** — `liquidate((loan,collateral,oracle,irm,lltv),borrower,
  seizedAssets,repaidShares,data)` = `0xd8eabcb8`, `position(bytes32,address)`
  = `0x93c52062`, `market` = `0x5c60e39a`, `idToMarketParams` =
  `0x2c3c9157`. The repo's `main` (Blockscout's verified ABI agrees) renamed
  from `liquidationCall`, added `receiver` params, and reordered
  `MarketParams` — implementing against 2024-era docs would have produced
  reverting calldata.
- Every Comet selector (`absorb` `0xc3cecfd2`, `buyCollateral` `0xe4e6e779`,
  `isLiquidatable`, `quoteCollateral`, `userCollateral`, `getAssetInfo`,
  `numAssets`) was confirmed against the dispatcher of the *live
  implementation* behind the proxy (`CometWithExtendedAssetList`
  `0x83D49126…`, resolved via Blockscout), and the `AssetInfo` decode was
  validated against live returns (13 assets; word1-low-20-bytes yields WBTC
  at index 1, WETH at 2, wstETH at 5). The modern flow is absorb →
  discounted `buyCollateral` (absorb pulls no base).
- Maker addresses resolved from the on-chain chainlog
  (`0xdA0Ab1e0…`): `MCD_DOG` is `0x135954d1…` **not** the pre-2024
  `0xaD7c337E…`; gem joins / OSM pips for ETH-A, WBTC-A, WSTETH-A recorded;
  `dog.ilks(ilk)` cross-checked against the chainlog's clip entries. The
  Dog/Clipper sources (`sky-ecosystem/dss`) were read for the exact bark
  room/dust arithmetic, take's slice/owe floor logic, ray price units, and
  the kick-reward location (`clip.kick` mints `tip + tab·chip` to the
  kicker — the current Dog's bark itself pays no reward).

**Verification.**

- `cargo test --all`: **171 passed**, 0 failed (25 of them new) — new suites
  cover selector pinning against the verified bytes, the Morpho share/debt
  rounding and incentive curve, Comet absorb/buyCollateral encoding, Maker
  bark arithmetic (room/dust/partial), ray price scaling, the leads registry
  band, and oracle classification.
- `cargo clippy --all-targets -- -A clippy::too_many_arguments`: zero
  warnings in the new/edited files (pre-existing warnings elsewhere are
  unchanged).
- `cargo fmt --all` applied to the changed files only.
- Live interface checks over public RPCs (no key): chainlog reads, Comet
  `numAssets`/`getAssetInfo` decode, Morpho OR-topic activity `eth_getLogs`
  (both Borrow generations), `dog.ilks(ETH-A)` → clip address, and the
  gem-join LogNote layout (which caught the third pre-CI bug below).

The three bugs caught before CI ever ran, for the record: the Comet
`AssetInfo.asset` decode initially took the wrong 12 bytes of the asset word
(unit test), the Maker hole/dirt arithmetic initially used `u128` for
rad-scale uint256 values (`Hole` ≈ 2.5e53 does not fit; would have panicked
on the first real read — unit test), and the Maker urn harvest initially
watched Vat `frob` LogNotes that the live Vat never emits (live RPC probe;
urns now come from the gem joins' *anonymous* LogNotes with the urn in
topics[2]).

Each of those three was caught by a local check before the branch was
proposed — which is the point of running the full gate set locally rather
than treating CI as the compiler.

**Frontend:** widening the `Strategy` union breaks every
`Record<Strategy, …>` consumer, not just the funnel panel: `RiskPanel`'s toggle state/env snippet and two demo-data
Records needed the new rows too, plus `demoNote`'s exhaustive switch
(`TS2366`). Fixed and verified end-to-end: `npm install`, `npx tsc --noEmit`
clean, `npm run build` succeeds.

**Out of scope for that change:** contracts were unchanged — the executor's
generic `Call[]` covers every new leg, so the artifact-drift job stays green
by construction — and end-to-end anvil simulation of a live liquidation,
which needs an archive endpoint.

## 2026-08-21 — console + mode-switch pass

Added the operator surface for Phase 3 and the W6 gate measurement (see
`PHASE_2_HANDOFF.md` §1.5 for the change list). Verification:

- **Frontend: fully verified.** `npx tsc --noEmit` clean,
  `npm run build` succeeds, and the dev server was exercised end-to-end in
  demo mode: `GET/POST /api/bot/mode` (flip to live and back, invalid-body
  rejection), the `/api/eth` read-only RPC proxy (live `eth_chainId` /
  `eth_getBalance` through it; `eth_sendTransaction` / `personal_sign`
  refused by the allowlist), and demo rows carrying `victims` for the
  explorer links.
- **Rust.** One error surfaced late in this change — an `E0382`
  borrow-after-move in `Engine::new` (the struct literal moves `cfg`, so the
  new `mode:` field could not read `cfg.live_execution` after it) — fixed by
  building `LiveMode` before the literal ([PR
  #18](https://github.com/nick-kuhle/JerseyMikes/pull/18) commit `345decc`).
  `cargo clippy --all-targets` and `cargo test --all` (the two new `LiveMode`
  tests included) pass alongside `contracts (foundry)`, `frontend (next.js)`
  and the artifact-drift job. Lesson recorded and since adopted as policy:
  compile and test locally before proposing a branch — CI is the merge gate,
  not the compiler.
- The mode API cannot arm a process: `LiveMode::set_live(true)` on an
  unarmed bot returns the restart instructions (surfaced as `409`), pinned
  by `live_mode_can_never_arm_an_unarmed_process`. See `docs/RISK.md`.

## What CI verifies

CI is enabled and lives at
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml); it runs on every
push (all branches) and every PR. All four jobs are required and all four are
green on `main`.

| Job | Commands |
| --- | --- |
| `contracts (foundry)` | `forge fmt --check`, `forge build --sizes`, `forge test -vvv` |
| `embedded bytecode is current` | recompiles with solc-js and fails if `bot/crates/mev-bot/artifacts`, `frontend/lib/MevExecutor.creation.hex` or `contracts/abi` drifted from the sources |
| `bot (rust)` | `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -A clippy::too_many_arguments`, `cargo test --all` |
| `frontend (next.js)` | `npm ci`, `tsc --noEmit`, `next build` |

No job is advisory. Both formatter checks (`forge fmt --check` and
`cargo fmt --all -- --check`) were made blocking once the tree was clean; a
formatting diff now fails the build like any other regression.

## How a change is verified

The full gate set runs locally before a branch is proposed, and CI re-runs it
on the merge commit. Nothing merges on a partial run.

```bash
make bot-check      # cargo fmt --all -- --check && cargo clippy --all-targets
make bot-test       # cargo test --all
make contracts      # forge fmt --check && forge build --sizes && forge test -vvv
cd contracts && node script/compile-check.js   # artifact reproduction
git diff --exit-code -- bot/crates/mev-bot/artifacts contracts/abi \
  frontend/lib/MevExecutor.creation.hex        # artifact drift
cd frontend && npx tsc --noEmit && npm run build
```

`script/compile-check.js` is a solc-js fallback that reproduces the embedded
runtime bytecode without Foundry, so the artifact-drift gate is runnable from
a Node-only environment. It compiles 28 sources with zero errors and confirms
the `MevExecutor` runtime at **11,497 bytes**. Solc reports the known
transient-storage and test-contract-size warnings; those are warnings, not
compile failures, and both are expected (see `docs/OPTIMIZATIONS.md`).

Two things CI cannot prove, and which therefore belong to the operator soak
rather than to the dev gate: the systemd units and container images under
`deploy/` (they need a host with systemd and Docker), and end-to-end anvil
simulation against an archive endpoint. Both are covered by
`docs/PATH_TO_LIVE.md`.

### Making the artifact deterministic (why 9,618 → 9,577)

`compile-check.js` now passes `metadata: {bytecodeHash: "none", useLiteralContent: true}`
to solc, matching `foundry.toml`'s existing `bytecode_hash = "none"`. Previously
the script used solc's default IPFS metadata hash, whose inputs include the
*absolute path* of every source file — so the emitted `MevExecutor.runtime.hex`
changed depending on which directory the repo was checked out into. The
`artifact-drift` CI job (`git diff --exit-code -- bot/crates/mev-bot/artifacts
contracts/abi`) therefore failed in CI (checked out at
`/home/runner/work/...`) even though the artifact reproduced byte-for-byte on
the machine that generated it. Removing the embedded metadata hash made the
artifact reproducible from any checkout — at the time it dropped the runtime
from 9,618 to 9,577 bytes; the contract has grown since and the current
runtime is 11,497 bytes. Functionality is unchanged; the bytecode is injected
into the anvil fork via `anvil_setCode`.

## Frontend dependency posture

The console tracks the current Next.js release line. `next` is pinned at
**16.3.2** with `react`/`react-dom` at 19.x. The bump path ran
`15.5.4 → 15.5.7 → 16.x`: the first step closed **CVE-2025-66478**, a CVSS
10.0 RCE in the React Server Components protocol affecting App Router
deployments on `next@15.5.4`; the move onto the 16 line cleared the remaining
tail of lower-severity Next/PostCSS/sharp advisories (image-optimizer,
middleware and server-action DoS/SSRF paths this dashboard does not use).

`npm audit` is expected to be clean on the pinned tree. Re-run it, plus
`npx tsc --noEmit` and `npm run build`, whenever the lockfile changes; the
`frontend (next.js)` CI job enforces the latter two.

Historical note on CI enablement: `.github/workflows/ci.yml` requires the
GitHub `workflows` permission to modify. Pushes from a token without that
scope are rejected with `refusing to allow ... to create or update workflow
'.github/workflows/ci.yml' without 'workflows' permission`. Any change to the
CI definition therefore needs a credential carrying that scope.

## The `competition` test failure was a wrong assertion, not a flake (2026-08-21)

Once CI ran for real, the `bot (rust)` job kept failing at
`cargo test --all` (exit 101) while
`cargo fmt --check` and `cargo clippy --all-targets` passed in the same job.
The raw log showed exactly one failing test:

```
---- competition::tests::half_the_bid_is_unlikely stdout ----
thread 'competition::tests::half_the_bid_is_unlikely' panicked at src/competition.rs:110:9:
p=0.24973989440488234
test result: FAILED. 116 passed; 1 failed
```

This is deterministic arithmetic, not an environment flake: with the shipped
`LOGISTIC_K = 2.2`, a bid at half the winning price gives
`p = σ(-1.1) = 0.2497…` on every machine. The test asserted
`p ∈ (0.05, 0.20)` — a range (and a module doc comment promising "~0.88 at
2×, ~0.12 at half") written against a different steepness than the constant
that shipped. No environment could ever pass that assertion against `K=2.2`;
the local "all green" runs predated this code. Fixed by pinning the test to
`p ∈ (0.20, 0.30)` around the actual value and correcting the doc comment
(2× → ~0.90, half → ~0.25, noting the `ours/win − 1` centring makes the two
asymmetric). `LOGISTIC_K` itself is runtime competition-model behaviour and
was deliberately left untouched.

Two adjacent notes from the same pass:

- **The dense-graph budget test was also hardened.**
  `enumeration_of_a_dense_graph_stays_inside_the_budget` asserted
  `elapsed < ENUMERATION_BUDGET * 8` (200 ms) around a debug-profile search —
  flake-prone on slow, noisy shared runners even though it was not this
  failure's cause. The 25 ms budget bounds cycle *enumeration* (the deadline
  is checked on every recursion step, and the expired-deadline contract is
  pinned by its own test); sizing already-enumerated cycles runs after that
  check. The test now asserts a generous 2 s ceiling that an unbounded search
  would still blow through, plus `found.len() <= MAX_CANDIDATES`. It
  deliberately does *not* assert non-emptiness: the fixture prices every pool
  identically, so fees make every cycle a loss and an empty candidate list is
  the correct result there.
- **Process change that came out of it.** This failure is the reason the
  full gate set is now run locally before a branch is proposed. A
  deterministic assertion mismatch is not something CI should be the first
  to discover.

The fix was confirmed by the CI run on the working branch
([run 32514548356](https://github.com/nick-kuhle/JerseyMikes/actions/runs/32514548356)):
`bot (rust)` green — `cargo fmt --check`, `cargo clippy --all-targets`, and
`cargo test --all` with 117 passed, 0 failed — alongside green
`contracts (foundry)`, `frontend (next.js)`, and `embedded bytecode is
current` jobs. The artifact-drift job also re-proves on every run that a
fresh `actions/checkout` (i.e. a fresh clone with submodules) reproduces
`MevExecutor.runtime.hex` byte-for-byte.

## Regenerating the embedded artifacts

`bot/crates/mev-bot/artifacts/MevExecutor.runtime.hex` is injected into the
anvil fork with `anvil_setCode`, so simulation works before the contract is
deployed anywhere. Regenerate after any contract change:

```bash
cd contracts && npm install && node script/compile-check.js
# or, with Foundry:
forge build && jq -r '.deployedBytecode.object' out/MevExecutor.sol/MevExecutor.json \
  > ../bot/crates/mev-bot/artifacts/MevExecutor.runtime.hex
```

CI fails if the checked-in artifact drifts from the compiler output.
