# Risk & safety

> Moving a simulating bot to live? Do it in the order set out in
> [`SIM_TO_LIVE.md`](SIM_TO_LIVE.md) — secure the API first, tighten these
> knobs second, arm last.

## Why broadcasting is fail-closed

The transport exists, but no single switch can invoke it. A bundle reaches a
relay only after all of these independent checks:

1. live (not replay) strategy lane;
2. engineering live-candidate strategy;
3. risk, drawdown, gas, and exact-account inventory approval;
4. boot arming (`LIVE_EXECUTION=true` and `I_UNDERSTAND_LIVE_RISK=yes`);
5. independent capability (`BROADCAST_ENABLED=true`);
6. authenticated runtime mode is live;
7. the candidate strategy's own qualification verdict is `PASS`;
8. no unresolved startup nonce-recovery block;
9. exact reserved-nonce fork simulation succeeds.

The defaults disable both arming and broadcast capability. `eth_callBundle`
remains a read-only accuracy cross-check; `eth_sendBundle` uses the separately
configured relay reputation signer while bundle transactions use the funded
`SEARCHER_PRIVATE_KEY`.

## Execution controls

| Layer | Where decided | Change cadence |
| --- | --- | --- |
| Broadcast capability | `BROADCAST_ENABLED` | restart |
| Boot arming | `LIVE_EXECUTION` + literal `I_UNDERSTAND_LIVE_RISK=yes` | restart |
| Runtime mode | authenticated `POST /api/mode` | immediate, can only narrow boot arming |
| Strategy qualification | canonical evidence in SQLite | continuously recomputed |
| Risk/strategy narrowing | authenticated `POST /api/risk` | immediate |

An unarmed process cannot be switched live (`409 Conflict`). An armed process
may be paused immediately. None of these controls bypasses another. See
[`SIM_TO_LIVE.md`](SIM_TO_LIVE.md) for the exact qualification, durable nonce,
relay retry/cancellation, finality, and rollback behavior.

## Why a failed opportunity costs nothing

`MevExecutor` measures retained profit and reverts with
`Unprofitable(realised, required)` below `minProfit`. A correctly simulated
atomic private bundle that reverts should be dropped by the builder and consume
no gas. This is not a guarantee against relay/builder defects or partial
inclusion; the bot therefore uses non-reverting bundle policy, exact receipt
reconciliation, explicit partial-inclusion incident states, and a drawdown
stop. The searcher must still hold gas ETH.

Additional on-chain guards, all optional per bundle:

| Guard | Effect |
| --- | --- |
| `minProfit` | reverts unless the realised delta clears it |
| `blockDeadline` | reverts if the bundle slips to a later block |
| `maxBaseFee` | reverts if base fee spiked since we sized the trade |
| `bribeBps` | pays the builder a share **of realised profit**, so a losing bundle pays nothing |
| `searchers` allowlist | only approved addresses can call `execute` |
| transient-storage guards | reentrancy, flash-loan callback, V3 mint callback |

## Off-chain risk parameters

Set in the environment; see `.env.example`.

| Variable | Default | Meaning |
| --- | --- | --- |
| `MIN_NET_PROFIT_WEI` | `1` | Record anything that is not a loss |
| `MAX_POSITION_WEI` | `100 ETH` | Cap on notional per bundle |
| `MAX_BASE_FEE_WEI` | `500 gwei` | Refuse to play in a gas spike |
| `BRIBE_BPS` | `9000` | 90% of gross to the builder |
| `MAX_GAS_PER_BUNDLE` | `3,000,000` | Bundle gas ceiling |
| `MAX_DRAWDOWN_WEI` | `0` (off) | Cumulative simulated loss that trips the kill switch |
| `MAX_INFLIGHT_PER_STRATEGY` | `32` | Concurrent simulations per strategy |

The names are wei- or bps-denominated on purpose. `MIN_NET_PROFIT_ETH`,
`MAX_BASE_FEE_GWEI`, `MAX_DRAWDOWN_ETH`, and `BUILDER_SHARE_BPS` are **not**
read: if any of them is set, `run`/`api` refuse to start and `doctor` prints
`✗ env names`. That is fail-closed against an older checklist whose values
would otherwise silently no-op and leave the liberal defaults above in force.

These start **deliberately liberal**. The first run's job is to measure how much
MEV is reachable and where the losses come from, not to be profitable. Suggested
tightening order once there is data:

1. Raise `MIN_NET_PROFIT_WEI` above the observed noise floor (start around
   0.002 ETH — roughly two blocks of failed-inclusion opportunity cost).
2. Lower `MAX_POSITION_WEI` per strategy to the size where the realised-vs-
   predicted profit error stops growing.
3. Turn on `MAX_DRAWDOWN_WEI`.
4. Drop `BRIBE_BPS` and watch inclusion rate — this is the parameter with the
   most money in it.

## Known limitations

- **Sizing assumes our bundle is the whole block.** Competing searchers'
  bundles, and any transaction between ours and the victim's, are not modelled.
  Real-world profit will be lower than simulated profit.
- **Victim replay needs raw bytes.** If the RPC does not serve
  `eth_getRawTransactionByHash`, sandwich and JIT opportunities are skipped
  rather than guessed at.
- **MEV-Share hints are usually redacted.** Most private orderflow yields only
  a function selector and log topics; strategies that need calldata cannot act
  on those.
- **Aave sizing is per-reserve** (real debt/collateral assets, real bonus
  from the data provider); the close factor remains the HF-based 50%/100%
  simplification rather than v3.1's per-reserve value — the fork simulation
  absorbs the residue.
- **Re-orgs are marked, not replayed.** A parent-hash mismatch or rewind
  flags simulations in the discarded range (`reorged = 1`) and drops them
  from P/L. The bot does not re-simulate the new canonical chain for those
  blocks; the next heads and the delivered-block backfill pick up from there.
- **Inclusion probability is a ranking, not a forecast.** `inclusion_p` is
  a logistic of our bribe versus the realised builder payment. It does not
  model other searchers, builder preference, or relay connectivity.

## Operational notes

- SQLite is production safety state: it contains qualification evidence,
  submitted payloads, nonce reservations, relay responses, finalized
  outcomes, **and the drawdown kill switch**. Back it up; never delete it
  during nonce recovery or go-live.
- `mev-bot doctor` verifies endpoints before a run; qualification and execution
  endpoints verify evidence after it starts. A persisted kill-switch trip
  prints as `✗ kill switch`.
- The drawdown kill switch is durable. A trip is written synchronously to
  SQLite and restored at boot — `systemctl restart` cannot silently re-arm
  a bot that stopped itself. `POST /api/risk/reset` (authenticated) is the
  only re-arm; it clears both the in-memory flag and the durable row.

## Runtime risk surface (`GET/POST /api/risk`)

The risk envelope is **no longer boot-only**. The console's risk panel
applies changes instantly: the risk engine gates the next opportunity, the
fork simulator prices the next bundle's `minProfit`/`bribeBps` guards, and
the signed-bundle gas cap — all from the same shared runtime state. A
`POST /api/risk/reset` re-arms a tripped drawdown kill switch (explicitly,
from the dashboard) and clears the durable SQLite row so the next restart
does not come up already tripped.

What stays boot-only, deliberately:

| Boundary | Why |
| --- | --- |
| **Strategy enablement can only narrow at runtime** | A strategy whose env toggle was off was never constructed (zero RPC cost by design); "enabling" it at runtime would silently do nothing, so the API refuses with the restart instructions — same shape as the live-mode switch. Re-enabling a boot-on strategy that was disabled at runtime is allowed. |
| **Live arming (`LIVE_EXECUTION` + `I_UNDERSTAND_LIVE_RISK`)** | Unchanged: two independent keys read once at boot; `POST /api/mode` can only narrow what they allow. |
| Everything else in `Config` (endpoints, chain, sim settings) | Not runtime state; change requires a restart. |

Validation: patches are all-or-nothing; `bribeBps ≤ 10000`, gas cap in
`[21000, 16777216]` (the EIP-7825 per-tx protocol cap, live since Fusaka),
wei amounts must parse. A rejected
patch changes nothing and returns its reason with a 400. The `.env` snippet
in the panel is demoted to what it always really was: persisting the current
values as **boot defaults**.
