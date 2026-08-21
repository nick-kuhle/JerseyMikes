# Phase 2 design review

Date: 2026-08-21

## Feedback

The design makes the right risk-first choice: direct cycle enumeration is easier
 to audit than an approximate graph relaxation, and QuoterV2 is preferable to a
second implementation of concentrated-liquidity math. The funnel should remain
the first milestone because it gives every later strategy a measurable baseline.
UniversalRouter-only decoding is also the right initial aggregator scope;
1inch and 0x should not be added until the funnel demonstrates a meaningful
public-mempool gap.

The design is not yet a complete implementation plan, however:

- The current checkout implements the funnel and a V2 discovery slice, but not
  multi-leg arb, V3 sandwich sizing, V3 pool discovery, or aggregator decoding.
- `PoolDiscovery` must not permanently remember a pool when an RPC read fails.
  This PR fixes that retry behavior. It is especially important during provider
  rate limiting and reorg recovery.
- Discovery currently populates a V2 cache. `PoolCreated` and a V3 cache need a
  separate implementation; treating a V3 pool as a V2 pair would be unsafe.
- Funnel counters currently count strategy calls that return one or more
  opportunities, not individual opportunities. That is useful as an invocation
  metric, but the API/dashboard should label it accordingly or count each
  opportunity before the next strategy expansion.
- The design's “3–5 legs” language needs a concrete gas and RPC budget before
  implementation. A bounded candidate cap and cancellation/backpressure should
  be tested before enabling it on a live feed.

Because Rust is unavailable in the authoring environment, this review does not
claim a Rust build or test pass. The PR should be treated as a small correctness
fix plus an implementation checkpoint, not as completion of all Phase 2 work.

## Recommended next steps

1. Run `make bot-test` and `make bot-check` in CI or a Rust-enabled checkout.
2. Add deterministic tests for discovery retry, log-window progression, and
   duplicate logs (including a transient `fetch_v2_pool` failure).
3. Extract shared log decoding before adding V3 `PoolCreated`; keep V2 and V3
   caches/types separate.
4. Implement and benchmark the cycle graph with a hard candidate/time budget.
5. Add V3 QuoterV2 integration behind a feature/config toggle and test calldata
   selectors and victim-min-output rejection.
6. Revisit aggregator decoding only after one week of funnel data.
7. Complete Phase 1 replay validation before changing any live-execution guard.

These steps align with `docs/MAINTAINING.md` §4 and §7: measure first, then add
pool coverage, then multi-leg arb, then V3 sandwich, and only afterward pursue
live execution or new chains.
