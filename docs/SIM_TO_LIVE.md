# Switching from simulation to live

The checklist for taking a bot that has been simulating and pointing it at real
money. It covers the two settings people trip over — `LIVE_EXECUTION` and
`API_BIND` — and the order to change things in.

Read [`GO_LIVE.md`](GO_LIVE.md) first if you have not deployed `MevExecutor`
yet. That page is about getting a contract on chain; this page is about
flipping the bot over to use it.

---

## The one thing to know first

**Arming the bot and exposing its API are two independent decisions, and the
second one is the dangerous one.**

`LIVE_EXECUTION=true` controls whether the bot may broadcast. `API_BIND`
controls who can reach the control surface that decides *what* it broadcasts.
Get the first one wrong and the bot stays quiet; get the second one wrong and
a stranger can retune your risk envelope while you watch the dashboard.

That is why the bot refuses to start with a network-reachable API and no
password — **in simulation mode too**. See
[Why simulation mode is not exempt](#why-simulation-mode-is-not-exempt).

---

## If you are upgrading an existing `.env`

**Start here — this is the most common failure.**

`.env` is gitignored, so upgrading the code does **not** update your
environment file. If your `.env` predates the API auth change it still says
`API_BIND=0.0.0.0:8080`, and an explicit value always beats the new default.
You get the new enforcement without the new default, and `make bot-run` stops
with:

```
Error: API_BIND is 0.0.0.0:8080 (not loopback) but API_AUTH_TOKEN is unset.
```

Nothing is broken. Pick one of the two fixes below and the bot starts again.

### Fix A — bind to loopback (recommended)

```bash
sed -i.bak 's|^API_BIND=0\.0\.0\.0:8080|API_BIND=127.0.0.1:8080|' .env
```

Right answer when the bot and the console run on the same machine. The console
reaches the bot **server-side** through its own `/api/bot/*` proxy, so the
dashboard keeps working with no further changes. To reach the API from your
laptop, tunnel instead of rebinding:

```bash
ssh -N -L 8080:127.0.0.1:8080 user@host
```

### Fix B — keep the port open, add a shared secret

```bash
# in .env
API_BIND=0.0.0.0:8080
API_AUTH_TOKEN=<paste the output of: openssl rand -hex 32>
BOT_API_TOKEN=<the same value, so the console can drive the bot>
```

Only worth it when something genuinely off-box has to call the API directly.
`BOT_API_TOKEN` is read by the Next.js **server**, never shipped to the
browser.

Confirm either fix with `make doctor`:

```
✓ api bind          127.0.0.1:8080 (loopback, no token needed)
✓ api bind          0.0.0.0:8080 (token required on mutating endpoints)
```

---

## Why simulation mode is not exempt

The guard asks one question: *is a mutating API about to listen on a
network-reachable address without a password?* `LIVE_EXECUTION` does not enter
into it.

`live=false` stops the bot broadcasting **its own** bundles. It does nothing
about these three endpoints, which are open to anyone who can reach the port:

| Endpoint | What an unauthenticated caller can do |
| --- | --- |
| `POST /api/risk` | set `bribeBps` to 10000 — 100% of gross profit to the builder — or throttle `maxPositionWei` to 1 wei, or disable strategies |
| `POST /api/risk/reset` | clear a kill switch that drawdown had tripped, putting the bot straight back to work |
| `POST /api/mode` | flip live mode **if** the process was armed at boot |

So a simulating bot on an open port is still a machine a stranger can
reconfigure. And the sequence that actually costs money is the ordinary one:
you simulate for a week on `0.0.0.0`, get comfortable, then set
`LIVE_EXECUTION=true` — inheriting the exposure into the run that spends real
ETH. Failing at simulation time is the point.

`POST /api/mode {"live":true}` cannot arm a process that was not armed at
boot; that still requires both environment keys and a restart. Remote
*sabotage* was the exposure, never remote *arming*.

---

## The switch, in order

Do these in sequence. Steps 1–3 involve no risk and no spending.

### 1. Secure the API first

Before anything else, apply Fix A or Fix B above and confirm with
`make doctor`. Doing this first means the step that arms the bot changes
exactly one thing.

### 2. Confirm the pre-flight is clean

```bash
make doctor
```

Every line should be `✓` or a `!` you understand. Pay attention to:

- `✓ api bind` — step 1 worked
- `✓ http rpc` / `✓ chain id` — the RPC answers and is on the chain you expect
- `✓ raw tx access` — without it, sandwich and JIT simulations are skipped
- `✓ anvil` — the fork simulator is runnable
- `! flashbots key` — fine for simulation; set it before you rely on relay
  cross-checks

### 3. Tighten the risk envelope

The shipped defaults are deliberately permissive so the first run measures what
is reachable rather than trying to be profitable:

| Setting | Default | Why you change it before live |
| --- | --- | --- |
| `MIN_NET_PROFIT_WEI` | `1` | accepts a 1-wei "profit" that gas will erase |
| `MAX_POSITION_WEI` | `100 ETH` | far more than a first live run should risk |
| strategies | all ten enabled | start with the ones your data says actually land |

See [`RISK.md`](RISK.md) for the suggested tightening order. Do this **before**
arming, not after.

### 4. Deploy and allowlist the executor

If you have not already: [`GO_LIVE.md`](GO_LIVE.md). You need
`EXECUTOR_ADDRESS` set in `.env`, with the bot's `SEARCHER_ADDRESS` on the
contract's searcher allowlist.

### 5. Arm — last, and on purpose

```bash
# in .env
LIVE_EXECUTION=true
I_UNDERSTAND_LIVE_RISK=yes
```

Both are required, and they are only read at startup, so this takes a restart.
You will see:

```
WARN mev_bot: LIVE EXECUTION IS ENABLED — bundles may be broadcast
```

If you see that line and did not intend it, stop the process and remove the
keys.

---

## What "live" does and does not mean in this build

Be precise about this before you fund anything.

Arming sets `live=true`, surfaces `liveArmed` on the API and dashboard, and
enables the runtime live toggle. What it does **not** do in this build is
submit bundles to a relay: `bundle::send_bundle_params` builds the
`eth_sendBundle` payload, but the only caller serialises it into the database
for inspection. Nothing puts it on the wire. Actual submission is Phase 3 of
[`ROADMAP.md`](ROADMAP.md).

So arming today makes the bot *behave* as if it were trading and record exactly
what it would have sent. That is the safest possible rehearsal — and it means
the broadcast path is the least-exercised code in the repo. Treat the first
real submission as a change that deserves its own review, not a config flip.

The on-chain profit guard is unconditional either way: `MevExecutor` reverts
the whole batch if it does not clear `minProfit`, and a reverting bundle sent
through private orderflow is dropped by the builder rather than mined.

---

## Rolling back

Simulation is always one restart away:

```bash
# in .env
LIVE_EXECUTION=false
```

Leave `API_BIND` alone when you roll back — the loopback bind or the token is
correct in both modes, and re-opening the port is the change that would hurt.

To stop trading immediately without a restart, flip the mode toggle in the
dashboard, or call the API directly:

```bash
curl -X POST http://127.0.0.1:8080/api/mode \
  -H 'content-type: application/json' \
  -d '{"live":false}'
# add -H "Authorization: Bearer $API_AUTH_TOKEN" if you chose Fix B
```

There is no "trip the kill switch" request — the kill switch arms *itself*
when realised drawdown exceeds `MAX_DRAWDOWN_WEI`, and `POST /api/risk/reset`
is what clears it afterwards. To halt by tightening instead, set an
unreachable floor:

```bash
curl -X POST http://127.0.0.1:8080/api/risk \
  -H 'content-type: application/json' \
  -d '{"minNetProfitWei":"1000000000000000000000"}'
```

`POST /api/risk` takes a partial patch — send only what you want to change:

| Field | Type |
| --- | --- |
| `minNetProfitWei`, `maxPositionWei`, `maxBaseFeeWei`, `maxDrawdownWei` | decimal wei, as a **string** |
| `bribeBps` | number, 0–10000 |
| `maxGasPerBundle`, `maxInflightPerStrategy` | number |
| `strategies` | partial map, e.g. `{"sandwich": false}` |

---

## Quick reference

| Symptom | Cause | Fix |
| --- | --- | --- |
| `Error: API_BIND is 0.0.0.0:8080 … API_AUTH_TOKEN is unset` on `make bot-run` | old `.env`, network-reachable API, no token | Fix A or Fix B above |
| Same error on `make doctor` | you are on a build from before the fix | update to `main`; `doctor` reports instead of aborting |
| `✗ api bind` in `doctor`, but it finishes | working as intended — `doctor` binds nothing | fix when convenient, required before `run` |
| Dashboard shows demo data after securing the API | console cannot reach the bot | check `BOT_API_URL`, and `BOT_API_TOKEN` if using Fix B |
| `WARN … LIVE EXECUTION IS ENABLED` unexpectedly | both live keys set in `.env` | remove them and restart |

| Variable | Simulation | Live |
| --- | --- | --- |
| `LIVE_EXECUTION` | `false` | `true` |
| `I_UNDERSTAND_LIVE_RISK` | unset | `yes` |
| `API_BIND` | `127.0.0.1:8080` | `127.0.0.1:8080` |
| `API_AUTH_TOKEN` | unset | unset (only needed if `API_BIND` is not loopback) |
| `EXECUTOR_ADDRESS` | optional | required |
