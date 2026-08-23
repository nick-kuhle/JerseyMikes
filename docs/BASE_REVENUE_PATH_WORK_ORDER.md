# Work Order — Base revenue path after safety foundation

**Chain:** Base mainnet (8453)
**Predecessor:** consolidated safety-foundation PR prepared after `main @ 9a3ef3a`
**Audience:** development and operations
**End state:** a Flashblocks-derived, executable multi-venue atomic-arbitrage candidate is simulated against identified preconfirmed state, submitted as only the searcher's raw transaction, independently reconciled against canonical state, qualifies without evidence reuse, and completes a wei-bounded live smoke before Base can be armed.

> **Until this work order is complete, Base remains shadow-only.** Do not deploy/fund the Base executor for live use, set `LIVE_SMOKE_MAX`, or treat a sequencer qualification result as authorization to trade.

---

## 0. Why this work order exists

Multi-chain plumbing is present, but the audit found that the original Base go-live plan skipped three semantic gaps:

1. Base registers one V2 venue; `AtomicArbStrategy::on_pending` requires two V2 venues and returns no pending cross-venue candidates.
2. Atomic arb consumes the V2 pool cache, not the separately discovered V3 cache. “Uni V2/V3 + Sushi” is not the shipped Base surface.
3. Pending arb records a victim hash, while raw transport correctly refuses victim-containing payloads. A generic pending feed therefore cannot become a live raw backrun merely by being connected.

The safety-foundation predecessor deliberately removes the old qualification shortcut: an `actual_mev_matches` row is no longer reused as both independent sequencer evidence and corresponding outcome evidence. Base atomic arb will remain `INSUFFICIENT SAMPLE` until this work order adds a genuine victimless state comparison.

---

## 1. Verified predecessor state

The predecessor should provide all of the following before this work starts:

- WS-K Base router/vault verification complete; no warning markers remain.
- Raw `eth_sendRawTransaction` treats the unwrapped JSON-RPC result correctly as acceptance.
- Raw cancellation decodes the original type-2 fee envelope, percentage-bumps both caps, covers current base fee, and fails closed above `RAW_CANCEL_MAX_FEE_WEI`.
- Unqualified raw smoke requires both `LIVE_SMOKE_MAX` and a durable `LIVE_SMOKE_MAX_GAS_COST_WEI`; each send reserves `sum(gasLimit × maxFeePerGas)`.
- `PRIORITY_FEE_WEI` drives atomic-arb prefilter economics; no 1 gwei literal remains there.
- Sequencer second-opinion evidence and corresponding outcome evidence are non-overlapping.
- Full Rust/frontend/contract CI green.

Re-verify these on the merged predecessor commit, not on an unmerged branch.

---

## 2. Architecture decision

### 2.1 Introduce preconfirmed-state provenance

Do not map Flashblocks onto ordinary public-mempool semantics. Add an explicit source/state identity, for example:

```rust
TxSource::Flashblock {
    block_number: u64,
    flashblock_index: u64,
    state_id: B256,
}
```

The exact type may differ, but it must preserve:

- sealed L2 block number;
- incremental Flashblock/sub-block index;
- an immutable identity for the preconfirmed state used to quote/simulate;
- observation timestamp;
- provider/feed label;
- whether transaction order is preconfirmed or merely pending.

A dependency on preconfirmed state is not a foreign transaction that must be resent. Keep it separate from `victim_hashes`, which retains its current bundle-replay meaning.

### 2.2 Send only the searcher's transaction

For a Flashblocks backrun:

1. Observe ordered preconfirmed transaction(s).
2. Read/quote against that identified preconfirmed post-state.
3. Build and simulate only the searcher transaction against that state.
4. Before signing/sending, verify the state identity is still current and the candidate TTL has not expired.
5. Submit only the searcher transaction by raw RPC.

Never strip a victim from an ordinary bundle to make raw mode accept it. The opportunity must be constructed with the correct preconfirmed-state semantics from the start.

### 2.3 Build one real multi-venue graph

Recommended delivery order:

1. Uniswap V3 ↔ Aerodrome volatile pools.
2. Aerodrome stable pools after exact stable-curve parity.
3. Additional venues only after measured TVL/flow justifies them.

Do not register an AMM as “V2-compatible” unless fee location, reserve accounting, pair discovery and execution calldata are verified.

---

## 3. Workstreams

## WS-O — Flashblocks provider and ingestion contract

### O1 — Provider selection

Use a Flashblocks-aware application RPC/provider. Base's raw infrastructure stream is intended for node infrastructure, not direct application consumption.

Required features:

- `newFlashblockTransactions` or `newFlashblocks` WebSocket subscription;
- preconfirmed/pending state reads;
- transaction receipts/status at preconfirmation time;
- archive HTTP for anvil and canonical reconciliation;
- sustained rate limits for 200 ms cadence;
- documented redistribution/retention terms.

Official reference:
https://docs.base.org/base-chain/api-reference/flashblocks-api/flashblocks-api-overview

Record provider, plan, endpoint capabilities and ToS conclusions in `docs/BASE_FEED.md`. Never commit endpoint credentials.

### O2 — Capture fixture before coding

Capture at least 30 minutes of redacted payload shapes:

- subscription handshake;
- empty and non-empty Flashblocks;
- transaction object variants;
- reorg/replacement/duplicate behavior;
- state/block identifiers;
- disconnect and resume behavior.

Commit sanitized JSON fixtures under `bot/crates/mev-bot/tests/fixtures/flashblocks/`.

### O3 — Parser and deduplication

Implement `spawn_flashblocks` behind a distinct env/config mode. Requirements:

- parse fixture variants without panics;
- deterministic dedupe key includes preconfirmed state identity, not only tx hash;
- bounded reconnect/backoff;
- explicit counters for malformed frames, duplicates, reconnects and state gaps;
- no fallback that silently relabels malformed data as ordinary pending flow.

**Acceptance:** fixture tests; 48-hour shadow capture; sealed-block coverage, duplicate rate and lead-time distributions documented.

---

## WS-P — Executable Base venue graph

### P1 — Common directed-edge abstraction

The current graph is `V2Pool`-specific. Introduce a venue edge interface that can quote exact output and build exact executor calls without forcing V3/Aerodrome into constant-product reserve fields.

Each edge must expose:

- venue and pool identity;
- token in/out;
- state identity/block;
- exact quote for input;
- conservative gas estimate;
- calldata builder;
- capability flags (volatile/stable/concentrated).

Keep existing V2 behavior byte-identical through regression fixtures.

### P2 — Uniswap V3 exact quoting/execution

- Consume the existing V3 discovery cache.
- Quote using on-chain QuoterV2 or a proven exact tick-state implementation; do not approximate V3 as V2.
- Build SwapRouter02/executor calls for the exact path and fee tier.
- Pin quotes to the same preconfirmed state used by simulation.
- Add fork tests across fee tiers and tick crossings.

### P3 — Aerodrome volatile

- Verify factory/router/pool addresses against official sources and live getters.
- Read per-pool fee from the canonical contract location.
- Implement volatile-pool exact integer math and execution calldata.
- Seed only measured liquid routes (initial candidates: WETH/USDC, WETH/cbBTC; verify current liquidity first).
- Add fork parity tests at boundary sizes and fee changes.

### P4 — Aerodrome stable, separate PR/flag

- Port contract-equivalent iterative stable-curve math.
- Fuzz quote parity against on-chain calls; target ≤1 wei where contract rounding permits.
- Bound iteration count and fail closed on non-convergence.
- Keep `DEX_AERODROME_STABLE=false` until soak evidence exists.

**Acceptance:** at least one V3 ↔ Aerodrome volatile route produces a profitable synthetic/fork candidate and an executable single searcher transaction; no mainnet regression.

---

## WS-Q — Preconfirmed-state simulation and raw delivery

### Q1 — State-pinned opportunity

Add fields to `Opportunity` (or a dedicated dependency struct) for:

- preconfirmed state identity;
- observed Flashblock index;
- expiry Flashblock/index;
- triggering transaction hashes for attribution only;
- `requires_foreign_payload=false`.

Do not overload `victim_hashes`.

### Q2 — Simulator backend

Implement a backend that executes only the searcher transaction against the identified preconfirmed state. Candidate approaches must be measured against provider support:

- provider simulation against pending/preconfirmed tag;
- `eth_simulateV1` against preconfirmed state;
- local Flashblocks-aware node state.

Anvil sealed-head replay alone is not sufficient proof for a 200 ms opportunity.

The simulator must return the state identity it actually used. Mismatch with the opportunity's identity is a hard failure.

### Q3 — TTL and last-moment recheck

Before nonce reservation and again before raw send:

- current state identity still equals the simulated identity, or an explicitly supported descendant relation;
- TTL has not expired;
- exact reserved-nonce simulation is still profitable;
- gas and priority fee remain inside the risk envelope.

A stale candidate is dropped and counted, never retargeted optimistically.

### Q4 — Integration test

Add a deterministic test proving:

```text
Flashblocks fixture
  → explicit preconfirmed source
  → cross-venue candidate
  → state-pinned exact sim
  → bundle with zero foreign txs
  → signed chain-8453 type-2 payload
  → raw gateway acceptance
```

This is the acceptance test the original Base plumbing lacked.

---

## WS-R — Independent atomic-arb qualification

### R1 — Define evidence populations before schema

For victimless atomic arb:

1. **Independent state fidelity:** compare predicted pool/post-state deltas from the state-pinned simulation with canonical realized state transitions for the same state identity, route, direction and amount.
2. **Corresponding outcome fidelity:** finalized own execution economics. Competitor transactions may remain research evidence, but must not be treated as exact own outcomes unless input, ordered route, direction and entity accounting all match.

One evidence row/sample ID must belong to only one threshold population.

### R2 — Schema

Prefer a dedicated table such as `state_comparisons` with:

- unique sample ID;
- opportunity ID;
- strategy;
- source/preconfirmed state ID;
- canonical block/hash;
- ordered pool route;
- input amount/direction;
- predicted and realized deltas;
- error bps;
- canonical/reorg marker;
- created timestamp.

Add uniqueness constraints that prevent duplicate frames/reconciliation passes from inflating sample counts.

### R3 — Qualification API/UI

Rename presentation labels from relay-shaped wording where possible while preserving API compatibility:

- `comparisonBackend=sequencer`
- `independentComparisons`
- `actualComparisons`
- evidence provenance and unique counts

The console screenshot must make clear what each population proves.

### R4 — Regression tests

- one actual match cannot satisfy both populations;
- one state comparison cannot be counted twice after replay;
- reorged evidence disappears;
- wrong state ID/route/direction/amount does not match;
- 30 independent + 30 outcomes can pass only when both accuracy thresholds pass.

**Acceptance:** a synthetic seven-day fixture can PASS; removing either population returns `INSUFFICIENT SAMPLE`; production Base remains unqualified until real rows exist.

---

## WS-S — Timing, bidding and soak

### S1 — Decision latency

Expose end-to-end:

- feed receive → parse;
- parse → candidate;
- candidate → quote;
- quote → exact sim;
- sim → sign;
- sign → RPC acknowledgement;
- acknowledgement → preconfirmation/inclusion.

Report p50/p95/p99. A 200 ms feed does not imply the complete decision path fits 200 ms.

### S2 — Priority-fee estimator

Only after sealed/preconfirmed data is collected:

- estimate relevant winning tips by comparable opportunity/order position;
- cap bids by net EV and `PRIORITY_FEE_MAX_WEI`;
- cold/stale estimator falls back to reviewed static value;
- compare shadow static/adaptive outcomes before enabling adaptive mode.

Do not infer ordering solely from the highest transaction tip in a block.

### S3 — Fresh 168-hour soak

Restart the qualification window after WS-O through WS-R deploy. Earlier evidence was produced under different semantics and cannot authorize this path.

Daily checks:

- no process restart/panic;
- no state-identity gaps hidden by reconnect;
- feed coverage and lead-time stable;
- stale-candidate rate understood;
- independent and actual populations grow without duplicate IDs;
- prediction-error median and tails stable;
- mainnet process/DB/funnel unchanged;
- backups verified.

---

## WS-T — Executor, cancellation drill and wei-bounded smoke

Only after real qualification PASS:

1. Reconfirm WS-K getters and deployed bytecode artifacts.
2. Deploy/verify Base `MevExecutor`; allowlist a fresh Base searcher EOA.
3. Keep `BRIBE_BPS=0`.
4. Rehearse accepted raw replacement on Base Sepolia or a controlled node; record original and replacement hashes/caps.
5. Review exact worst-case smoke envelope and set both:

```ini
LIVE_SMOKE_MAX=1
LIVE_SMOKE_MAX_GAS_COST_WEI=<one reviewed exact-payload worst case>
```

6. Arm Base only; submit one qualified cross-venue arb; verify RPC acceptance, preconfirmation, receipt, executor event, finalized economics and durable gas-risk counters.
7. Immediately disarm and set both smoke variables back to zero.
8. Write a signed-off post-mortem before production arming.

A rejected replacement, missing receipt evidence, stale-state send, unexpected gas burn, or qualification provenance ambiguity is a stop condition.

---

## 4. PR sequencing

Keep review surfaces small even if development is parallel:

1. `base-flashblocks-fixtures-ingest` (WS-O)
2. `base-venue-graph-v3-aero-volatile` (WS-P1–P3)
3. `base-preconfirmed-sim-delivery` (WS-Q)
4. `base-independent-qualification` (WS-R)
5. `base-timing-bidding` (WS-S1–S2, flags default off)
6. `base-live-runbook` (WS-S3/WS-T docs and ops)
7. Aerodrome stable as a separate dark PR (P4)

Every PR must preserve mainnet behavior and keep Base live switches off.

---

## 5. Definition of done

- [ ] Official/provider Flashblocks feed captured, parsed, deduplicated and measured.
- [ ] Explicit preconfirmed-state provenance exists; it is not `victim_hashes`.
- [ ] Atomic arb graph includes at least Uniswap V3 and Aerodrome volatile with exact executable quotes.
- [ ] One deterministic integration test reaches a zero-foreign raw chain-8453 payload from a Flashblocks fixture.
- [ ] State identity and TTL are checked before sim and send.
- [ ] Independent state comparisons and actual outcomes are disjoint, unique and reorg-aware.
- [ ] Fresh 168-hour Base qualification PASS at unchanged thresholds.
- [ ] Raw cancellation accepted in a rehearsal with fee-cap evidence.
- [ ] Executor deployed, verified and allowlisted with separate Base keys.
- [ ] One wei-bounded smoke completed, reconciled and immediately disarmed.
- [ ] Mainnet tests, behavior, process, DB and qualification remain unchanged.

Until every item is checked, the accurate operator label is **Base shadow measurement**, not Base live MEV.
