# Switching from simulation to live

This is the production switch-over procedure. Deployment, arming, broadcast
capability, runtime mode, risk, inventory, and strategy qualification are
independent gates. Missing any one of them leaves the bot in shadow mode.

## What the switch can do now

The live path sends signed `eth_sendBundle` payloads concurrently to every URL
in `BUNDLE_RELAY_URLS`. It retries transport failures with the same
`replacementUuid`, persists relay responses and durable nonce reservations,
and reconciles the exact submitted transaction hashes after
`FINALITY_DEPTH` canonical descendants. Partial or incoherent inclusion is
reported explicitly; it is never treated as a successful execution.

This is real transaction-signing and relay-submission code. Do not arm it as a
rehearsal. Rehearsal uses all defaults (`BROADCAST_ENABLED=false`,
`LIVE_EXECUTION=false`).

## Qualification is per strategy

`GET /api/qualification` and the console report one of:

- `PASS`
- `FAIL`
- `INSUFFICIENT SAMPLE`

for every strategy. A strategy can be submitted only when its own row is
`PASS`. A global elapsed timer does not qualify anything.

Each live-candidate row requires, within the same window:

1. at least `QUALIFICATION_HOURS` of canonical block observations (168 hours by
   default), with no gap larger than `QUALIFICATION_MAX_GAP_SECS`;
2. no dropped persistence writes in the current process;
3. `QUALIFICATION_MIN_SAMPLES` successful exact-payload fork simulations;
4. `QUALIFICATION_MIN_RELAY_COMPARISONS` fork-versus-relay comparisons;
5. `QUALIFICATION_MIN_ACTUAL_MATCHES` corresponding canonical on-chain
   outcomes with confidence of at least 8000 bps;
6. at least `QUALIFICATION_MIN_ACCURACY_BPS` of relay and actual comparisons
   within `QUALIFICATION_MAX_ERROR_BPS` relative error.

Defaults are 30 samples/comparisons, 20% error tolerance, and 80% accuracy.
Restart downtime is visible as an observation gap because the qualification
window comes from canonical block records in SQLite, not process uptime.

Only `sandwich`, `sandwich_v3`, and `atomic_arb` currently have the engineering
properties required to become live candidates (atomic settlement into the
profit token and an executor-enforced retained-profit guard). Every other
strategy is still reported independently, but remains `INSUFFICIENT SAMPLE`
with its engineering limitation until that limitation is implemented.

## Pre-flight order

### 1. Secure the control surface

Loopback is the default and recommended bind:

```ini
API_BIND=127.0.0.1:8080
```

A non-loopback bind requires `API_AUTH_TOKEN`; the bot refuses to start without
one. The console receives the same value as the server-only `BOT_API_TOKEN`.
Never use a `NEXT_PUBLIC_*` variable for a secret.

### 2. Preserve the qualification database

The SQLite database is qualification and settlement evidence. Do not replace
it during deployment.

- systemd: set `DB_PATH=/var/lib/jerseymikes/mev.sqlite` and back up that
  directory.
- Docker: preserve the `bot-data` named volume.

Check `GET /api/status` for `persistence.dropped == 0` and inspect
`GET /api/qualification` before proceeding.

### 3. Separate the two signers

```ini
FLASHBOTS_SIGNER_KEY=<unfunded relay reputation key>
SEARCHER_PRIVATE_KEY=<funded transaction-signing EOA>
```

The bot derives `SEARCHER_ADDRESS` from `SEARCHER_PRIVATE_KEY` and rejects a
configured address that does not match. Never reuse the owner/deployer key as
the searcher key.

### 4. Deploy the current executor

The executor ABI changed to guarded phased execution. Redeploy the bytecode
from this revision, allowlist the derived searcher address, and set:

```ini
EXECUTOR_ADDRESS=0x...
```

Do not point this bot at an executor from an older release. Follow
[`GO_LIVE.md`](GO_LIVE.md) and verify owner, searcher allowlist, vault, WETH,
code, and funding on chain.

### 5. Tighten risk

The repository defaults are intentionally broad for observation. Set a real
minimum net profit, position limit, base-fee limit, drawdown stop, gas limit,
and builder share before arming. See [`RISK.md`](RISK.md).

### 6. Require an actual strategy PASS

Do not infer readiness from a green aggregate number. Save the complete
`/api/qualification` response with the change record and confirm the strategy
you intend to enable says `PASS`.

### 7. Arm all three static gates

```ini
BROADCAST_ENABLED=true
LIVE_EXECUTION=true
I_UNDERSTAND_LIVE_RISK=yes
```

`LIVE_EXECUTION` without the literal acknowledgement does not arm. Broadcast
capability without live arming is rejected at startup. Restart, verify
`liveArmed=true` and `broadcastEnabled=true`, then use the authenticated runtime
mode control to enter live mode.

## Effective broadcast predicate

A bundle reaches a relay only when all are true:

1. it is in the live (not replay) lane;
2. the strategy is an engineering live candidate;
3. risk and inventory checks pass;
4. the process was armed at boot;
5. runtime mode is live;
6. `BROADCAST_ENABLED=true`;
7. that strategy's current qualification verdict is `PASS`;
8. startup nonce recovery is not blocking reuse;
9. the exact reserved-nonce payload succeeds in simulation.

## Nonce recovery and finality

Live candidates pass through a single serialized nonce lane. The chosen nonce
is used by both simulations and submission. Before sending, the reservation is
written synchronously to SQLite. At startup, unresolved replacement UUIDs are
cancelled at all configured relays. If every cancellation cannot be proven,
new broadcasts remain blocked through the old target block; nonce reuse is not
guessed.

Switching runtime mode to simulation, changing runtime risk, or tripping the
drawdown stop cancels all active replacement UUIDs. Cancellation uses the same
serialized nonce lane; a nonce is released only after every relay acknowledges.

Accepted bundles remain `included_unfinalized` until `FINALITY_DEPTH`. The bot
then verifies all expected receipts are successful, in one block, share the
canonical block hash, and contain executor evidence. The API and console expose
exact gross profit, builder payment, retained profit, signer gas, and net
profit. `finalized_partial_inclusion`, `finalized_incoherent_inclusion`, and
`finalized_missing_executor_evidence` are explicit incident states. If a
phase-1 opener lands without its close, the owner can clear expired bookkeeping
with `clearExpiredBaseline(tag)` after the opening block, then use `ownerCall` /
`sweep` for deliberate asset recovery. The contract never guesses a recovery
trade.

## Rollback

First switch runtime mode to simulation:

```bash
curl -X POST http://127.0.0.1:8080/api/mode \
  -H 'content-type: application/json' \
  -H "Authorization: Bearer $API_AUTH_TOKEN" \
  -d '{"live":false}'
```

Then set `BROADCAST_ENABLED=false` and `LIVE_EXECUTION=false` and restart. Keep
the database and API security settings unchanged so pending reconciliation and
audit evidence remain available.

## Required monitoring

Watch:

- `/api/qualification` for verdict or continuity changes;
- `/api/executions` for partial/incoherent/finality states;
- `/api/status` for kill switch, inventory, nonce recovery block, and dropped
  persistence;
- relay acceptance responses and head-stall alerts;
- executor/searcher ETH and executor WETH balances.
