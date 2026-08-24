# PATH TO LIVE

**Audience:** the on-call operator and the engineer sitting next to them.
**Goal:** fire **one or two real** `eth_sendBundle`s as soon as the box is
wired, then disarm and start the seven-day soak.

> **This document begins where development ends.** Everything here is an
> *operator* procedure run against a finished, released build — the four CI
> jobs green, `mev-bot doctor` clean on the target host, no open code items on
> the live path. The soak is a measurement period, not a late development
> phase: it answers "does this correct system make money here, safely, over
> time", not "is the system finished".
>
> If a soak surfaces a code defect, that is a build-phase escape. Stop the
> soak, ship the fix through CI, and **restart the soak clock at zero** — a
> qualification verdict is only evidence about the exact build that produced
> it. Do not patch a running soak.

This is the only page you need in the room. The other docs are companions,
not prerequisites:

| When you need… | Open |
| --- | --- |
| Deploying `MevExecutor` | [`GO_LIVE.md`](GO_LIVE.md) |
| Why a gate refused a send | [`SIM_TO_LIVE.md`](SIM_TO_LIVE.md), [`RISK.md`](RISK.md) |
| systemd / Docker / backups | [`DEPLOYMENT.md`](DEPLOYMENT.md) |
| Toolchains and a quiet first run | [`SETUP.md`](SETUP.md) |
| Day-0 → Day-7 production soak | [`DAY0_RUNBOOK.md`](DAY0_RUNBOOK.md) |

---

## 0. What “ASAP” means (and what it does not)

There are **two** live paths. Do not mix them up.

| Path | When | How many sends | Qualification `PASS` required? |
| --- | --- | --- | --- |
| **A — live smoke (this document)** | today, to prove signing → relay → nonce → executor | 1–2, even if they lose money on chain | **No.** Bounded by `LIVE_SMOKE_MAX`. |
| **B — money switch** | after 7 continuously observed days | unbounded (still gated) | **Yes**, per strategy. |

Smoke is **not** a back door around the soak. It is a hard-capped, durable
counter (`risk_state.live_smoke_used` in SQLite). A restart cannot refill it.
Default is `LIVE_SMOKE_MAX=0` (off). The binary hard-caps it at **5**.

Smoke still refuses to send unless **every other gate** is green:

1. live (not replay) lane
2. engineering live-candidate strategy (`sandwich` / `sandwich_v3` /
   `atomic_arb` / the four `liquidation*` rows)
3. risk + inventory (searcher has gas ETH; flash-loan arb needs no executor WETH)
4. boot arming: `LIVE_EXECUTION=true` **and** the literal `I_UNDERSTAND_LIVE_RISK=yes`
5. `BROADCAST_ENABLED=true`
6. runtime mode is live (an armed boot **starts** live)
7. a remaining smoke slot **or** a strategy `PASS`
8. no unresolved nonce-recovery block
9. the exact reserved-nonce payload simulates successfully and is net-positive

Unprofitable sims are never sent. Shadow-only strategies (`jit`, all
liquidations, `oracle_frontrun`, `sniper`) are never sent. Do not flip
`DECODE_UNIVERSAL_ROUTER`. Do not raise `ARB_MAX_CYCLE_LEN`. Do not wipe
the database.

**Required code.** This page assumes the live-smoke build
([PR #38](https://github.com/nick-kuhle/JerseyMikes/pull/38) —
`LIVE_SMOKE_MAX`, searcher-nonce pin on the anvil fork). If you are still
on `main` from PR #37 only, `LIVE_SMOKE_MAX` does not exist and nothing
will send without a seven-day `PASS`. Merge #38 first.

---

## 1. People and keys (do this before touching `.env`)

Three keys, three trust domains. Mixing any two is a stop.

| Role | Env | Funded? | Who holds it |
| --- | --- | --- | --- |
| Owner / deployer (cold or hardware) | `DEPLOYER_PRIVATE_KEY` — deploy only, then **delete** | gas for deploy + `setSearcher` | operator, not the bot host |
| Searcher hot key | `SEARCHER_PRIVATE_KEY` | **~0.05–0.1 ETH** for gas | bot host only |
| Flashbots reputation key | `FLASHBOTS_SIGNER_KEY` | **never** | bot host only |

- The bot **derives** `SEARCHER_ADDRESS` from `SEARCHER_PRIVATE_KEY`. Do not
  set `SEARCHER_ADDRESS` unless you are migrating a leftover dummy.
- Never reuse the owner key as the searcher. Never reuse the searcher as
  the Flashbots signer. `doctor` prints `✗ key separation` if you do.
- Never put any of these in a `NEXT_PUBLIC_*` variable.

---

## 2. Box checklist (nothing sends until every box is ticked)

- [ ] PR #38 is merged (or this host is running that binary).
- [ ] Paid mainnet RPC with **WebSocket**, `eth_getLogs`, and
      `eth_getRawTransactionByHash` in `ETH_HTTP_URL` / `ETH_WS_URL`.
      Public RPCs will throttle you. No `ETH_WS_URL` → no mempool → no
      opportunities.
- [ ] Foundry `anvil` on `PATH` (or `ANVIL_BIN=`).
- [ ] `MevExecutor` **from this revision** deployed on mainnet.
      Older executors are a different ABI — do not reuse them.
      Follow [`GO_LIVE.md`](GO_LIVE.md) (console checklist or
      `forge script script/Deploy.s.sol`).
- [ ] `EXECUTOR_ADDRESS=0x…` set to that deployment.
- [ ] Owner called `setSearcher(<derived searcher>, true)`.
- [ ] Searcher EOA has **0.05–0.1 ETH**. The executor does **not** need
      WETH or ETH for the first shot (atomic arb is a Balancer flash loan).
- [ ] `BUNDLE_RELAY_URLS` includes the Aug-2026 set (re-check relayscan.io):

  ```
  https://rpc.titanbuilder.xyz,https://rpc.quasar.win,https://rpc.eurekabuilder.xyz,https://relay.flashbots.net
  ```

  Do not add builder0x69, Eden, rsync, bloXroute, or BuilderNet.

- [ ] API is loopback (`API_BIND=127.0.0.1:8080`) **or**
      `API_AUTH_TOKEN=$(openssl rand -hex 32)` is set. The bot refuses to
      start on a routable bind without a token.
- [ ] `DB_PATH` points at a **persistent** file you will not delete
      (`/var/lib/jerseymikes/mev.sqlite` in production). The smoke counter
      **and** the qualification clock live here.
- [ ] No ghost names in the env file. These are rejected at boot:

  ```bash
  grep -nE '_(ETH|GWEI)=|BUILDER_SHARE' .env /etc/jerseymikes/env
  # expect no output
  ```

  Forbidden: `MIN_NET_PROFIT_ETH`, `MAX_BASE_FEE_GWEI`, `MAX_DRAWDOWN_ETH`,
  `BUILDER_SHARE_BPS`. Canonical: `MIN_NET_PROFIT_WEI`, `MAX_BASE_FEE_WEI`,
  `MAX_DRAWDOWN_WEI`, `BRIBE_BPS`.

---

## 3. First-shot strategy: `atomic_arb` only

For the proving burst, construct **only** the live-candidate you can fund
with a flash loan. Sandwich needs WETH already sitting in the executor;
skip it until the path is proven.

```ini
STRATEGY_ATOMIC_ARB=true
STRATEGY_SANDWICH=false
STRATEGY_SANDWICH_V3=false

# Shadow-only — they will never send. Leave them on if you want the tape.
STRATEGY_JIT=true
STRATEGY_LIQUIDATION=true
STRATEGY_LIQUIDATION_COMPOUND=true
STRATEGY_LIQUIDATION_MORPHO=true
STRATEGY_LIQUIDATION_MAKER=true
STRATEGY_ORACLE_FRONTRUN=true
STRATEGY_SNIPER=true

DECODE_UNIVERSAL_ROUTER=false
ARB_MAX_CYCLE_LEN=3
```

A strategy whose env toggle is `false` is **not constructed**. You cannot
turn it on later from the dashboard; you restart.

---

## 4. Operator `.env` for the burst

These are **not** repository defaults. Put them in the host env
(`/etc/jerseymikes/env` or the process `.env`) and restart.

```ini
# --- required plumbing ---
ETH_HTTP_URL=https://<your-paid-mainnet-rpc>
ETH_WS_URL=wss://<your-paid-mainnet-ws>
EXECUTOR_ADDRESS=0x<this-revision>
SEARCHER_PRIVATE_KEY=0x<funded searcher>
FLASHBOTS_SIGNER_KEY=0x<unfunded reputation key>
BUNDLE_RELAY_URLS=https://rpc.titanbuilder.xyz,https://rpc.quasar.win,https://rpc.eurekabuilder.xyz,https://relay.flashbots.net
DB_PATH=/var/lib/jerseymikes/mev.sqlite
API_BIND=127.0.0.1:8080
# API_AUTH_TOKEN=           # required only if API_BIND is not loopback

# --- the four live switches ---
LIVE_SMOKE_MAX=2
BROADCAST_ENABLED=true
LIVE_EXECUTION=true
I_UNDERSTAND_LIVE_RISK=yes

# --- liberal for the proving shots; tighten after ---
MIN_NET_PROFIT_WEI=1
MAX_DRAWDOWN_WEI=0
BRIBE_BPS=9000
MAX_GAS_PER_BUNDLE=3000000
```

`I_UNDERSTAND_LIVE_RISK` must be the literal string `yes`. `true` / `1` /
`YES` do **not** arm.

`LIVE_SMOKE_MAX=2` is the right first value. Do not set 5 “just in case” —
each consumed slot is a real signed bundle aimed at a builder.

---

## 5. Photograph the box (`doctor`) before you arm

On the **same host**, against the **same env file** you will run:

```bash
make bot-build
./mev-bot doctor          # or: make doctor
```

Walk the output. You want this shape (markers: `✓` pass, `·` info, `!`
warning, `✗` stop):

| Line | Must be |
| --- | --- |
| `http rpc` | `✓` with a real head |
| `chain id` | `✓` `0x1` |
| `raw tx access` | `✓` (sandwich later; arb does not need it) |
| `websocket` | `· configured` — **not** `! not set` |
| `searcher key` | `✓` derived address matches what you allowlisted |
| `broadcast gate` | `enabled` once you have flipped the four switches |
| `live smoke` | `! LIVE_SMOKE_MAX=2 used=0 remaining=2` |
| `anvil` | `✓` |
| every `bundle relay` | reachable (HTTP status, not `✗ unreachable`) |
| `key separation` | `✓` |
| `executor` | `✓` has on-chain code |
| `executor searcher` | `✓` allowlisted |
| `kill switch` | `✓ not tripped` (or `· no database yet`) |
| `env names` | `✓ canonical wei/bps names only` |
| footer | `mode: LIVE \| broadcast: enabled \| smoke: LIVE_SMOKE_MAX=2` |

**Stops.** Fix these before restarting into the burst:

- `✗ SEARCHER_PRIVATE_KEY required` / missing `EXECUTOR_ADDRESS` — arming
  will refuse to boot.
- `✗ executor … has no on-chain code` — you pointed at the wrong address
  or have not deployed this revision.
- `! executor searcher … NOT allowlisted` — owner must `setSearcher`.
- `✗ kill switch durable trip persisted` — a previous process stopped
  itself. Only `POST /api/risk/reset` (with the bearer) re-arms it.
- `✗ env names` — a ghost `*_ETH` / `*_GWEI` / `BUILDER_SHARE_BPS` is set.
  Remove it. The bot will not start.
- `BROADCAST_ENABLED=true` without both arming keys — boot refuses.

---

## 6. Arm, confirm, wait for a shot

```bash
# systemd
sudo systemctl restart mev-bot
journalctl -u mev-bot -f

# or foreground
make bot-run
```

An armed boot **starts already live**. You do not need the console toggle
to begin sending. The toggle is the pause switch.

Confirm the process agrees:

```bash
curl -sS http://127.0.0.1:8080/api/status | jq '{
  mode, liveArmed, broadcastEnabled,
  liveSmoke,
  kill: .risk.killSwitchTripped,
  inventory,
  persistence
}'
```

Expected:

```json
{
  "mode": "live",
  "liveArmed": true,
  "broadcastEnabled": true,
  "liveSmoke": { "max": 2, "used": 0, "remaining": 2 },
  "kill": false,
  "persistence": { "dropped": 0 }
}
```

`inventory.searcherGasEthWei` (alias `ethWei`) must be non-zero. If it is
`"0"` the live inventory gate will refuse every send.

Then wait. The bot will not invent a bundle. It sends the next
**net-positive, exact-payload** `atomic_arb` that clears risk. That can be
minutes or hours depending on flow and `MIN_NET_PROFIT_WEI`. Do not “help”
it by lowering the profit floor below 1 wei or by loosening `submittable`.

Watch, in another terminal:

```bash
# smoke counter + last head
watch -n 5 'curl -sS http://127.0.0.1:8080/api/status | jq "{head: .head.number, smoke: .liveSmoke, submittable: .stats.submittable, funnel: .stats.funnel.atomic_arb}"'

# own submissions (empty until a slot is consumed)
curl -sS http://127.0.0.1:8080/api/executions | jq .

# logs that mean a real send
journalctl -u mev-bot -f | grep -E 'PROFITABLE bundle|consuming a live-smoke slot|eth_sendBundle|LIVE_SMOKE'
```

A consumed slot looks like:

```
WARN submission: consuming a live-smoke slot — sending without qualification PASS
     strategy=atomic_arb used=1 max=2
```

`GET /api/status` → `liveSmoke.used` increments **immediately before** the
relay call, whether the builder accepts or not. That is fail-closed: a
transport error still burns the slot.

---

## 7. After one or two sends — disarm the same hour

Do **not** leave `LIVE_SMOKE_MAX>0` overnight.

1. Pause immediately (no restart):

   ```bash
   curl -X POST http://127.0.0.1:8080/api/mode \
     -H 'content-type: application/json' \
     -H "Authorization: Bearer $API_AUTH_TOKEN" \
     -d '{"live":false}'
   ```

   Loopback with no token: omit the `Authorization` header.

2. Then make it survive a restart. In the env file:

   ```ini
   LIVE_SMOKE_MAX=0
   BROADCAST_ENABLED=false
   LIVE_EXECUTION=false
   I_UNDERSTAND_LIVE_RISK=no
   ```

3. Restart. Confirm:

   ```bash
   curl -sS http://127.0.0.1:8080/api/status | jq '{mode, liveArmed, broadcastEnabled, liveSmoke}'
   ```

   Expected: `mode=simulation`, `liveArmed=false`, `broadcastEnabled=false`,
   `liveSmoke.max=0`. `liveSmoke.used` stays at 1 or 2 — that is the durable
   record, not a bug.

4. **Keep the SQLite file.** The qualification clock is the same database.
   Deleting it resets you to Day 0.

Save the change record with the `/api/status`, `/api/executions`, and
`/api/qualification` responses from the burst.

---

## 8. Then start the seven-day soak (Path B)

Same binary, same executor, same database, **no** live switches:

```ini
BROADCAST_ENABLED=false
LIVE_EXECUTION=false
I_UNDERSTAND_LIVE_RISK=no
LIVE_SMOKE_MAX=0
```

Turn `STRATEGY_SANDWICH` / `STRATEGY_SANDWICH_V3` back on if you want those
rows accumulating evidence. Leave `DECODE_UNIVERSAL_ROUTER=false` and
`ARB_MAX_CYCLE_LEN=3`.

Daily:

- `GET /api/status` → `persistence.dropped == 0`, `liveArmed == false`.
- `GET /api/qualification` → the `atomic_arb` (and sandwich) rows moving
  toward 168 h, ≥30 fork samples, ≥30 relay comparisons, ≥30 high-confidence
  actual matches, ≥80 % inside 20 % relative error.
- Backups landing (`journalctl -u mev-db-backup.service`).

A restart does **not** reset the clock. Losing the database does.

When a strategy row says `PASS`, follow
[`DAY0_RUNBOOK.md`](DAY0_RUNBOOK.md) “Day 7 — the money switch”. That is
the production live path. Smoke is done.

---

## 9. If nothing sends

Work this list top to bottom. Each item is a real gate, not a hint.

| Symptom | Cause | Fix |
| --- | --- | --- |
| `liveSmoke` missing from `/api/status` | binary is pre-PR #38 | merge / deploy that build |
| `liveArmed: false` | `I_UNDERSTAND_LIVE_RISK` is not the literal `yes`, or `LIVE_EXECUTION` is off | fix env, **restart** (the console cannot arm) |
| `broadcastEnabled: false` | `BROADCAST_ENABLED` not true, or boot refused the pair | `BROADCAST_ENABLED=true` requires both arming keys |
| `liveSmoke.remaining: 0` | budget used or `LIVE_SMOKE_MAX=0` | you are done, or you forgot to set it |
| `mode: simulation` on an armed process | someone posted `{"live":false}` | `POST /api/mode {"live":true}` |
| `killSwitchTripped: true` | durable trip | authenticated `POST /api/risk/reset` |
| `inventory.ethWei: "0"` | searcher has no gas | send 0.05–0.1 ETH to the derived address |
| `inventory.broadcastBlockedUntilBlock` ≥ head | unresolved private bundle at startup | wait for that target to expire; do not guess a nonce |
| funnel `atomic_arb.candidatesEmitted = 0` | no mempool / discovery empty | `ETH_WS_URL`, `doctor` websocket line, wait a few blocks |
| `candidatesEmitted > 0` but `submittable = 0` | sims reverting or below `MIN_NET_PROFIT_WEI` | read `revertReason`; `Unprofitable` is the profit guard doing its job |
| `searcher tx 0 rejected: nonce too low` on the **fork** | stale inventory vs fork nonce | PR #38 pins the fork to the signed nonce — you are on the old binary |
| same error at a **builder** | we signed a used nonce | confirm `inventory.nonce` matches `eth_getTransactionCount(searcher, "pending")` |
| `NotSearcher()` revert | allowlist mismatch | owner `setSearcher(derived, true)` |
| boot: `BROADCAST_ENABLED=true requires both LIVE_EXECUTION…` | one of the two keys is off | set both, restart |
| boot: unused `MIN_NET_PROFIT_ETH` etc. | ghost env name | delete it |

Emergency stop, any time:

```bash
curl -X POST http://127.0.0.1:8080/api/mode \
  -H 'content-type: application/json' \
  -H "Authorization: Bearer $API_AUTH_TOKEN" \
  -d '{"live":false}'
```

Then `BROADCAST_ENABLED=false` and restart. Shadow lanes keep running.

---

## 10. What you will spend

| Item | Typical | Notes |
| --- | --- | --- |
| Deploy `MevExecutor` | ~0.002–0.01 ETH | once per revision |
| `setSearcher` | ~50k gas | once |
| Searcher float | 0.05–0.1 ETH | stays in the EOA; not spent unless a bundle is **included** |
| A reverting private bundle | **0** (builder drops it) | still burns a smoke slot |
| An included losing bundle | gas + whatever the trade lost | why we cap smoke at 2 |
| Executor WETH | 0 for `atomic_arb` | sandwich later, not now |

`MevExecutor` reverts `Unprofitable` below `minProfit`. A correctly simulated
atomic private bundle that reverts should not cost gas. Relay/builder
defects and partial inclusion are why we still use a tiny budget and then
disarm.

---

## 11. Do not

- Do not set `LIVE_SMOKE_MAX` in `.env.example` or any committed default.
  It is an operator knob.
- Do not raise `LIVE_SMOKE_MAX` above 2 for the first burst.
- Do not leave the four live switches on after the shots land.
- Do not wipe or replace `DB_PATH`. That erases smoke accounting **and**
  the soak clock.
- Do not flip `DECODE_UNIVERSAL_ROUTER`. That is W6, data-gated.
- Do not raise `ARB_MAX_CYCLE_LEN` above 3. That is W4, data-gated.
- Do not loosen `RiskEngine::submittable` or send unprofitable sims.
- Do not enable sandwich for the first shot unless the executor already
  holds WETH and you have accepted that inventory risk.
- Do not reuse one key for owner + searcher + Flashbots signer.
- Do not point this binary at an executor from an older release.

---

## 12. One-page copy/paste (after the box is wired)

```bash
# 1. env (host file): LIVE_SMOKE_MAX=2, BROADCAST_ENABLED=true,
#    LIVE_EXECUTION=true, I_UNDERSTAND_LIVE_RISK=yes,
#    STRATEGY_ATOMIC_ARB=true, STRATEGY_SANDWICH=false, STRATEGY_SANDWICH_V3=false
# 2. photograph
./mev-bot doctor
# 3. arm
sudo systemctl restart mev-bot
# 4. confirm
curl -sS http://127.0.0.1:8080/api/status | jq '{mode, liveArmed, broadcastEnabled, liveSmoke}'
# 5. wait until liveSmoke.used >= 1 (and you are satisfied, max 2)
# 6. pause
curl -X POST http://127.0.0.1:8080/api/mode \
  -H 'content-type: application/json' \
  -d '{"live":false}'
# 7. env: LIVE_SMOKE_MAX=0, BROADCAST_ENABLED=false,
#         LIVE_EXECUTION=false, I_UNDERSTAND_LIVE_RISK=no
sudo systemctl restart mev-bot
# 8. keep the database; begin the 7-day soak (DAY0_RUNBOOK.md Phase 4)
```
