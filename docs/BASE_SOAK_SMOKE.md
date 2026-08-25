# Base soak & controlled smoke — §3.3

**Date:** 2026-08-25  
**Status:** environment-blocked, documented, never faked.

## Why blocked

Work order 3.3 requires after Workstreams 1–3 change semantics:

1. Restart a clean 168-hour Base qualification clock
2. Deploy/verify Base executor and allowlist fresh Base searcher key
3. Rehearse raw replacement cancellation on Base Sepolia/controlled infra
4. Set reviewed `LIVE_SMOKE_MAX=1` and `LIVE_SMOKE_MAX_GAS_COST_WEI`
5. Execute one qualified, state-pinned cross-venue arb
6. Reconcile RPC acceptance, inclusion, executor event, receipt, final economics
7. Immediately disarm and document

**None of these can be completed in this sandbox:**

- No paid Base archive RPC with Flashblocks feed configured (`BASE_HTTP_URL`, `FLASHBLOCKS_WS_URL`) — public `mainnet.base.org` rate-limits and has no preconfirmation feed.
- No Base Sepolia RPC or funded Sepolia keys for cancellation rehearsal.
- No 168h uninterrupted process — sandbox resets every ~hours and wipes toolchain (6 resets observed).
- No live capital or executor deployment keys — operator must provide and revoke PAT, never commit keys.

**We do not fake qualification.** `qualification.rs` detects restart gaps from canonical block observations in SQLite; lowering `QUALIFICATION_HOURS` never creates evidence. A `PASS` requires 30 samples/comparisons per strategy and 168h continuously observed, with `MAX_GAP_SECS` enforced.

## What is ready for soak when env exists

- **B0 dual-chain boot:** `make setup` creates `.env` + `.env.base` (0600), `bot/data/` 0700, safe default `DB_PATH=data/base.sqlite`. `make doctor` validates both chain IDs (Base `0x2105`). `make bot-run` supervises isolated Ethereum :8080 and Base :8081, stops both if either exits.
- **B1 canonical ingestion:** `CHAIN_BLOCK_INGEST=true` (on by default on Base), `ChainBlockStats` + `RpcStats` + `SourceFunnel` in `/api/status`, `dataMode` triage.
- **B2 Flashblocks provenance:** `PreconfirmedState` with feed/parent/state identity/sequence/timestamp/TTL, dedupe on `(feed, state, tx_hash)`, bounded window, gap/reconnect accounting, fixture tests in `tests/fixtures/flashblocks/`.
- **B3 venue-edge refactor:** V2, V3, Aerodrome volatile edges with exact integer math, per-pool fee, route discovery, fork tests. Aerodrome stable behind `DEX_AERODROME_STABLE=false`.
- **B4 state-pinned sim + raw send:** source state identity/TTL/nonce/profit rechecks before and after reservation, zero-foreign-payload raw EIP-1559, same-nonce cancellation with percentage-bumped caps + base fee cover + hard cap.
- **B5 independent evidence:** `state_comparisons` unique on `(opportunity_id, source_state_id, route, amount_in, direction)`, reorg-aware `canonical=0` flip, duplicate frames cannot inflate, wrong state/route cannot match, disjoint from `actual_mev_matches`.
- **B6 sniper W4:** launch discovery `sniper_launches` (W4.1), Aero execution adapters (W4.2), LP-lock gate (W4.3), data-plane diagnostics (W0.3).

## How to run the soak when operator ready

```bash
# 1. Create Base-only env from template, do NOT reuse mainnet DB/keys
cp .env.example.base .env.base
# Fill BASE_HTTP_URL with paid archive RPC, FLASHBLOCKS_WS_URL if provider offers it
# Set CHAIN_ID=8453, API_BIND=127.0.0.1:8081, DB_PATH=/var/lib/jerseymikes/base.sqlite (managed) or bot/data/base.sqlite (local)
# Set BROADCAST_ENABLED=false, LIVE_EXECUTION=false, I_UNDERSTAND_LIVE_RISK=no for soak

make setup
make doctor          # must report Base 0x2105
make bot-run-base    # or make bot-run for dual

# 2. Watch /api/status?chain=base
# - x-data-source: bot, not demo
# - dataMode: live_canonical_only (no feed) or live_preconfirmation (feed up)
# - chainBlocks.blocks_fetched increasing, fetches_failed low, rateLimited low
# - flashblocks.connectionState 2s/10s, lastFrameAgeMs < 2s, sealedMatchRateBps graded after N blocks
# - sourceFunnels: chainBlock candidates >0, simulated >0

# 3. Let it run 168h uninterrupted, no restarts, no DB deletion
# Qualification: GET /api/qualification?chain=base — must reach PASS with 30 samples/comparisons per strategy

# 4. When PASS, deploy/verify Base executor (separate from mainnet) and fresh searcher key
# 5. Rehearse cancellation on Base Sepolia:
#    - Send raw tx, then replacement with same nonce, higher caps, verify receipt
#    - Check gas-at-risk accounting

# 6. Bounded smoke (only after signed review):
#    LIVE_SMOKE_MAX=1
#    LIVE_SMOKE_MAX_GAS_COST_WEI=<reviewed cap>
#    BROADCAST_ENABLED=true, LIVE_EXECUTION=true, I_UNDERSTAND_LIVE_RISK=yes
#    Execute one qualified, state-pinned cross-venue arb, then immediately disarm

# 7. Reconcile: RPC acceptance, inclusion, executor event, receipt, final economics — document in docs/BASE_SMOKE_REPORT.md
```

## What we will NOT do

- Fake a 168h clock by lowering `QUALIFICATION_HOURS` in production — test only.
- Claim Base live MEV revenue without `state_comparisons` PASS + raw cancellation drill + smoke reconciliation.
- Reuse mainnet keys, DB, or qualification for Base.
- Commit RPC URLs, private keys, or PATs.

Until soak passes, label Base as **shadow measurement / canonical replay**, not live revenue.

## References

- `docs/BASE_REVENUE_PATH_WORK_ORDER.md` — full work order
- `docs/BASE_SAFETY_FOUNDATION.md` — raw acceptance, cancellation, gas-at-risk
- `docs/BASE_FEED.md` — Flashblocks feed contract
- `bot/crates/mev-bot/src/qualification.rs` — gap detection, thresholds
