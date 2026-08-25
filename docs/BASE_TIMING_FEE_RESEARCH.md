# Base timing & fee research — §3.2

**Date:** 2026-08-25  
**Status:** measurement infrastructure complete, adaptive fee estimator deferred — insufficient sealed data.

## What is measured today (already in code)

`bot/crates/mev-bot/src/latency.rs` records per-stage histograms with p50/p95/p99:

| Stage | Meaning | Source |
|---|---|---|
| `IngestToStrategy` | pending observed → hydrated PendingTx handed to strategy | `engine.rs` `now_ms() - seen_at_ms` |
| `Strategy` | strategy `on_pending` / `on_block` wall time | per-strategy timer |
| `Risk` | risk gate wall time | `risk.rs` |
| `Simulation` | `eth_call` bundle sim + profit calc | `engine.rs` |
| `Discovery` | pool discovery `eth_getLogs` + pool loads | `engine.rs` |
| `Inventory` | sniper mark refresh | `engine.rs` |
| `Total` | ingest → bundle recorded (pending path budget) | `engine.rs` |

Exposed at `GET /api/latency` with `p50Ms/p95Ms/p99Ms/count` and `withinBudget` (p95 ≤ 150 ms pending-path budget from `PHASE_2_HANDOFF.md`).

Data-plane health (W0.3) adds:

- `RpcStats`: calls/requests/ok/errors/rateLimited/avgLatency/lastOk|Err
- `ChainBlockStats`: blocks_fetched/fetches_failed/txs_seen/last_fetch_ms
- `FlashblockStats`: connectionState (2s/10s), lastFrameAgeMs, sealedMatchRateBps
- `SourceFunnel`: candidates/gatedByRisk/simulated per source (`chainBlock`, `flashblocks`, `sequencerFeed`, `publicMempool`)

All atomics, read on every `/api/status` tick.

## Current observed distributions (shadow, no live capital)

No 168h clean Base qualification has completed yet (environment-blocked — see §3.3). The numbers below are from local fork + anvil + mock-server tests and a few live Base `mainnet.base.org` round-trips (paced 750-850 ms/call):

- `eth_getLogs` for V2 PairCreated over 64-block window: p50 ~180 ms, p95 ~450 ms (public endpoint, rate-limited bursts)
- `fetch_v2_pool` (token0/token1/getReserves batch): p50 ~90 ms, p95 ~220 ms
- `fetch_aero_pool` (4× eth_call + getFee): p50 ~140 ms, p95 ~320 ms
- `aero_get_pool` (factory.getPool): p50 ~60 ms, p95 ~150 ms
- `RpcStats.avgLatencyMs` healthy: 40-120 ms; under 429: errors + rateLimited bump, never counted as ok
- `IngestToStrategy` for Flashblocks: not yet measured live — fixtures only. Lead-time distribution requires provider feed.
- `Strategy` atomic arb: p50 5-15 ms, p95 30-50 ms (graph search is IO-bound, not CPU)
- `Total` pending path: p50 60-100 ms, p95 120-180 ms in local tests — within 150 ms budget when RPC healthy, over when rate-limited.

These are not production SLAs — they are shadow measurements to size the fee estimator.

## Why adaptive priority-fee estimator is deferred

Work order 3.2 requires:

> Build a conservative priority-fee estimator only after enough sealed/preconfirmed data exists, cap it by net EV, and compare static versus adaptive shadow performance.

**Insufficient data today:**

- No 168h clean Base chain with `state_comparisons` populated (WS-R) and sealed-block match rate graded. Flashblocks `sealedMatchRateBps` is null until graded.
- No preconfirmation inclusion data: Base has no builder relay; raw `eth_sendRawTransaction` acceptance → preconfirmation → inclusion path needs provider-specific latency (RPC ack, preconfirmation, inclusion) measured over sealed blocks.
- Priority fee on Base is ordering currency (no relay market). Adaptive estimator must be capped by net EV (`realized - gas - priority`) and must not overbid during rate-limit episodes.

**What exists:**

- Static fee path: `PRIORITY_FEE_WEI` / `MAX_FEE_PER_GAS` from env, checked against `maxBaseFee` in vault guards and risk limits. Replacement cancellation bumps both caps percentage-wise and covers current base fee (`BASE_SAFETY_FOUNDATION.md`).
- Shadow comparison harness: `SourceFunnel` + `latency` + `RpcStats` already separate static vs adaptive if we add a second fee path — we can log both without sending.

**Next step when data exists:**

1. Collect 1 week of `state_comparisons` with `source_state_id`, `canonical_block`, `predicted_wei` vs `realized_wei`, plus `flashblocks.lastFrameAgeMs` and inclusion lag.
2. Build estimator: `priority_fee = min( ev_cap, base_fee * k + p95_inclusion_lag * slope )` with hard cap from `RISK_MAX_PRIORITY_FEE_WEI` and EV cap.
3. Shadow log adaptive vs static fee for same opportunity (do not send adaptive yet), compare inclusion rate and net EV.
4. Only then enable adaptive behind `FEE_ESTIMATOR_ADAPTIVE=false` default.

Until then, static fee remains the only live path, and adaptive is documented as deferred for data, not omitted.

## Acceptance mapping

- [x] p50/p95/p99 measured per stage (latency.rs + status)
- [x] RPC latency/error/rate-limit measured (RpcStats)
- [x] Data-plane per-source funnel measured (SourceFunnel)
- [ ] Adaptive priority-fee estimator — deferred, needs 168h sealed data + inclusion lag
- [ ] Static vs adaptive shadow comparison — blocked on estimator
- [x] Never fake timing data — all numbers from real RPC or fixtures, null until graded

## References

- `bot/crates/mev-bot/src/latency.rs` — histogram + percentile
- `bot/crates/mev-bot/src/rpc.rs` — RpcStats
- `bot/crates/mev-bot/src/ingest.rs` — ChainBlockStats
- `bot/crates/mev-bot/src/flashblocks.rs` — FlashblockStats
- `bot/crates/mev-bot/src/engine.rs` — candidate_source, dataMode, launch_scan
- `docs/BASE_SAFETY_FOUNDATION.md` — raw acceptance + cancellation
