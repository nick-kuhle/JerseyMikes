# Base safety foundation — operator migration note

This revision intentionally keeps Base fail-closed while correcting raw transport, cancellation, smoke exposure and sequencer qualification semantics.

## Behavior changes

### Raw RPC acceptance

`RpcClient::call_raw` returns the unwrapped JSON-RPC `result`. Raw submission now treats any `Ok(result)` as RPC acceptance and persists a wrapped response for the existing relay-shaped storage API. Previously, a successful transaction hash was incorrectly searched for another nested `result` field and marked rejected after it had already been sent.

This is a settlement-critical correction: after upgrade, monitor both `relay_submissions` (`relay='raw'`) and canonical receipts. RPC acceptance still does not mean inclusion or success.

### Raw cancellation

Cancellation now decodes the original signed type-2 transaction and derives a replacement from its actual nonce and fee caps. It:

1. percentage-bumps both original caps by `RAW_CANCEL_BUMP_BPS` (default 12.5%);
2. ensures `maxFeePerGas` covers twice the current base fee plus the replacement priority fee;
3. enforces a priority floor of configured `PRIORITY_FEE_WEI + 1 gwei`;
4. refuses above `RAW_CANCEL_MAX_FEE_WEI` (default 500 gwei);
5. keeps nonce reuse blocked on missing base fee, malformed payload, cap refusal, RPC rejection or an already-mined original.

Configure explicitly in Base production env:

```ini
RAW_CANCEL_BUMP_BPS=1250
RAW_CANCEL_MAX_FEE_WEI=500000000000
```

These defaults are not a substitute for a Base Sepolia/controlled-node replacement drill.

### Raw smoke gas exposure

SQLite migration adds:

```text
risk_state.live_smoke_gas_risk_wei TEXT NOT NULL DEFAULT '0'
```

Raw pre-qualification smoke now requires two limits:

```ini
LIVE_SMOKE_MAX=<attempt cap, hard maximum 5>
LIVE_SMOKE_MAX_GAS_COST_WEI=<reviewed total worst-case exposure>
```

Before each unqualified raw send, the bot decodes every bot-owned type-2 payload and durably reserves:

```text
sum(gasLimit × maxFeePerGas)
```

The count and wei value are updated under the same SQLite connection lock. A zero/exhausted cap, malformed payload, corrupt durable counter or persistence error refuses the send. The reservation is intentionally sticky and conservative; it is not refunded after a cheap receipt or a rejected RPC request.

`GET /api/status` now exposes:

```json
{
  "liveSmoke": {
    "max": 0,
    "used": 0,
    "remaining": 0,
    "gasAtRiskWei": "0",
    "maxGasCostWei": "0"
  }
}
```

### Sequencer qualification

For `QUALIFICATION_BACKEND=sequencer`, only `block_comparisons` contributes to the independent second-opinion population. `actual_mev_matches` contributes only to the corresponding-outcome population.

The same actual route-match row can no longer satisfy both minimums. Because current victimless Base atomic arb does not write an independent state comparison, its expected verdict after upgrade is `INSUFFICIENT SAMPLE`. This is correct and must not be worked around by lowering thresholds.

### Atomic-arb fee prefilter

Atomic-arb prefilter gas cost now uses `PRIORITY_FEE_WEI`, matching the configured signing/simulation economics instead of a hardcoded 1 gwei.

## Rollout

1. Back up each SQLite DB with the existing online backup procedure.
2. Deploy this binary with all live switches off.
3. Run `mev-bot doctor`; resolve the new warnings:
   - raw smoke count without a wei cap;
   - non-zero `BRIBE_BPS` on a sequencer-only chain.
4. Start Base in shadow mode and verify the additive SQLite migration.
5. Confirm `/api/status.liveSmoke` includes both wei fields.
6. Confirm Base atomic-arb qualification is insufficient rather than passing from reused route matches.
7. Do not reuse qualification screenshots/evidence from the prior semantics for go-live authorization.
8. Proceed with [`BASE_REVENUE_PATH_WORK_ORDER.md`](BASE_REVENUE_PATH_WORK_ORDER.md).

## Validation for this revision

- Rust format check
- CI-equivalent Clippy
- 253 Rust library tests, including regressions for:
  - unwrapped raw transaction-hash acceptance;
  - EIP-1559 fee-envelope decode;
  - replacement fee bump/base-fee/hard-cap behavior;
  - raw smoke gas reservation;
  - sequencer evidence non-reuse;
  - configured atomic-arb priority fee
- Frontend TypeScript and production build
- Solidity compile/artifact drift and frontend bytecode parity

Foundry remains the canonical contract test job in CI; this revision does not change Solidity.
