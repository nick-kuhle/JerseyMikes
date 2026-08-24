# Day-0 runbook: production-identical environment → the Day-7 money switch

This is the operational checklist that takes the environment from "repo on a
laptop" to "100% production-identical on Day 0", so that when the 7-day soak
finishes and `GET /api/qualification` shows `PASS`, going live is **three env
vars, one restart, one console toggle** — zero code changes, zero contract
redeployments, zero database wipes.

> **Prerequisite: development is done before Day 0 starts.** This runbook is
> owned by operators and testers, and it assumes a released build: all four CI
> jobs green on the tagged commit, no open code items on the live path, and
> the deployment units in `deploy/` ready to install. Engineering does not
> "finish during the soak" — the whole point of the fixed 7-day window is that
> every hour of it measures the *same* binary.
>
> The clock is unforgiving by design. Any code change to the bot restarts the
> soak at zero, because qualification evidence is only evidence about the
> build that produced it. Deciding to ship a fix mid-soak is therefore
> deciding to spend another seven days, and that trade is the operator's to
> make deliberately, not to stumble into.

Companions: [`GO_LIVE.md`](GO_LIVE.md) (deploying `MevExecutor`),
[`SIM_TO_LIVE.md`](SIM_TO_LIVE.md) (what changes between lanes),
[`DEPLOYMENT.md`](DEPLOYMENT.md) (server hardening, backups),
[`W6_MEMO.md`](W6_MEMO.md) (the one open decoding decision).

> **⚠ Canonical env names — read this before writing `.env`.**
> The bot reads **wei-denominated** variables. These names do **not** exist
> and are **rejected at boot** if set (`run`/`api` refuse to start; `doctor`
> prints `✗ env names`): `MIN_NET_PROFIT_ETH`, `MAX_BASE_FEE_GWEI`,
> `MAX_DRAWDOWN_ETH`, `BUILDER_SHARE_BPS`. The real knobs are
> `MIN_NET_PROFIT_WEI`, `MAX_BASE_FEE_WEI`, `MAX_DRAWDOWN_WEI`, `BRIBE_BPS`.
> Audit any existing env file with:
>
> ```bash
> grep -nE '_(ETH|GWEI)=|BUILDER_SHARE' /etc/jerseymikes/env   # expect no output
> ```

## Phase 0 — preflight (minutes)

```bash
make bot-build                      # or: cd bot && cargo build --release
./mev-bot doctor                    # every line ✓ or a deliberate · / !
```

`doctor` now prints the whole Day-0 photograph: RPC reachability,
`eth_getRawTransactionByHash` support (required for sandwich/JIT replay),
per-relay reachability for every `BUNDLE_RELAY_URLS` entry, key separation,
on-chain executor posture (`eth_getCode`, `searchers(searcher)`, `owner()`),
database writability, unused ETH/GWEI/`BUILDER_SHARE` env aliases (boot
refuses these — they would silently no-op), a durable kill-switch trip
restored from SQLite, and the armed/broadcast/risk footer. Run it on the
production host, against the production `.env`.

## Phase 1 — on-chain deployment & key architecture

1. **Deploy `MevExecutor`** — follow [`GO_LIVE.md`](GO_LIVE.md) (console
   checklist or `forge script script/Deploy.s.sol --rpc-url $ETH_HTTP_URL
   --broadcast --verify`). The constructor binds mainnet Balancer V2
   (`0xBA12…2C8`) and WETH9 (`0xC02a…6Cc2`). Record the deployed address as
   `EXECUTOR_ADDRESS`.
2. **Three keys, three trust domains:**

   | Key | Env var | Funded? | Purpose |
   |---|---|---|---|
   | Owner / deployer (cold or hardware wallet) | `DEPLOYER_PRIVATE_KEY` (deploy only) | holds gas for deploy tx | Owns `MevExecutor`: `setSearcher`, `setOwner`, `sweep`, `ownerCall` |
   | Searcher hot key | `SEARCHER_PRIVATE_KEY` | ~0.05–0.1 ETH | Signs bundle transactions; must be allowlisted |
   | Flashbots signer | `FLASHBOTS_SIGNER_KEY` | never | `X-Flashbots-Signature` reputation header only |

   Never reuse the searcher key as the Flashbots signer (`doctor` warns).
3. **Allowlist the searcher** — from the owner wallet:
   `setSearcher(SEARCHER_ADDRESS, true)`. The bot self-rejects otherwise;
   `doctor` reads the mapping on-chain and reports it.
4. **Executor ETH** — WETH/Balancer flash-loan strategies need none. Only
   seed native ETH if a call-value strategy is enabled.
5. Re-run `./mev-bot doctor` — executor lines should now all read ✓.

## Phase 2 — high-availability infrastructure & DB persistence

1. **Dedicated mainnet RPC** (`ETH_HTTP_URL`, `ETH_WS_URL`): a paid,
   low-latency endpoint (QuickNode/Alchemy/Infura or own Reth/Erigon) that
   supports WebSockets, `eth_getLogs`, and `eth_getRawTransactionByHash`
   (public RPCs throttle or drop all three; `doctor` checks each).
2. **Multi-relay submission** — set `BUNDLE_RELAY_URLS` to the verified
   Aug-2026 list in `.env.example` (Titan ~55%, Quasar ~19%, Eureka ~9%,
   Flashbots for reputation). Re-verify shares on relayscan.io before
   relying on them; the market re-shuffles. Gone: builder0x69, Eden, rsync.
   Account-based (not configurable here without an account): bloXroute,
   BuilderNet.
3. **24/7 server + persistent DB** — install per
   [`DEPLOYMENT.md`](DEPLOYMENT.md) (systemd, hardened units, `Restart=
   on-failure`). `DB_PATH=/var/lib/jerseymikes/mev.sqlite` on a persistent
   volume. **Enable the backup timer** (`mev-db-backup.timer`, WAL-safe
   snapshots every 15 minutes) — if the database is wiped on Day 6 the
   qualification clock resets to Day 0.
4. **API hardening** — loopback bind (`API_BIND=127.0.0.1:8080`), or a
   non-loopback bind **requires** `API_AUTH_TOKEN=$(openssl rand -hex 32)`
   (the bot refuses to start otherwise). The console's server-side env
   carries `BOT_API_TOKEN`. Mutating endpoints always require the bearer.

## Phase 3 — risk parameter calibration

Real production guardrails (defaults are deliberately liberal; these are
the CTO-recommended baselines — tighten from data, not vibes):

| Canonical var | Example value | Meaning |
|---|---|---|
| `MIN_NET_PROFIT_WEI` | `5000000000000000` (0.005 ETH) | Minimum net profit **after** builder bribe |
| `MAX_BASE_FEE_WEI` | `100000000000` (100 gwei) | Skip bidding during gas spikes |
| `MAX_DRAWDOWN_WEI` | e.g. `250000000000000000` (0.25 ETH); `0` = disabled | Cumulative-drawdown kill switch |
| `BRIBE_BPS` | `9000` (90%) | Builder share of gross profit |
| `INVENTORY_GATE` | `true` (forced on when live anyway) | Verify searcher/executor balances pre-broadcast |
| `MAX_GAS_PER_BUNDLE` | default `3000000` | Clamped to `[21000, 16777216]` — the upper bound is the EIP-7825 per-tx protocol cap; a tx above it is invalid regardless of the 60M block limit |

Strategy funnel check: the live-eligible lanes are `sandwich`, `sandwich_v3`,
`atomic_arb`, and the four liquidation rows (`liquidation`,
`liquidation_compound`, `liquidation_morpho`, `liquidation_maker`). `jit`,
`sniper` and `oracle_frontrun` are shadow-only by design
(`Strategy::shadow_only_reason` names the reason for each). Live-eligible
means the row *may* earn a `PASS`; it still has to. `DECODE_UNIVERSAL_ROUTER`
stays `false` — that decision is made and recorded in `W6_MEMO.md`.

## Phase 3b — optional live-smoke burst (one or two real sends)

The 7-day soak still cannot start counting *live* evidence until the
signing → relay → nonce → executor path has been proven once. Qualification
cannot be env-knobbed away (`QUALIFICATION_HOURS` is `.max(1)` and still
needs high-confidence matches). For a bounded proving burst, set these in
the **operator** `.env` — they are not repository defaults:

```ini
LIVE_SMOKE_MAX=2
BROADCAST_ENABLED=true
LIVE_EXECUTION=true
I_UNDERSTAND_LIVE_RISK=yes
MIN_NET_PROFIT_WEI=1
```

`LIVE_SMOKE_MAX` is hard-capped at 5 and counted in SQLite, so a restart
cannot refill it. Every other gate still applies: shadow-only strategies
are never sent. Prefer `atomic_arb` (Balancer flash loan, no executor
WETH). Sandwich needs WETH in the executor. Fund the searcher with
~0.05–0.1 ETH for gas and point `BUNDLE_RELAY_URLS` at Titan / Quasar /
Eureka / Flashbots.

After one or two `eth_sendBundle` attempts (`GET /api/status` →
`liveSmoke.used`):

```ini
LIVE_SMOKE_MAX=0
BROADCAST_ENABLED=false
LIVE_EXECUTION=false
I_UNDERSTAND_LIVE_RISK=no
```

Keep the SQLite file. Then continue Phase 4.

## Phase 4 — start the 7-day soak (fail-closed)

`/etc/jerseymikes/env`:

```ini
BROADCAST_ENABLED=false
LIVE_EXECUTION=false
I_UNDERSTAND_LIVE_RISK=no
```

```bash
sudo systemctl enable --now mev-bot mev-bot-console mev-db-backup.timer
```

Verify daily against the API:

- `GET /api/status` → `persistence.dropped == 0`, `liveArmed == false`,
  `broadcastEnabled == false`.
- `GET /api/qualification` → accumulating toward `QUALIFICATION_HOURS=168`
  with `≥30` relay comparisons, `≥30` actual matches, and accuracy inside
  `QUALIFICATION_MAX_ERROR_BPS=2000` for `≥QUALIFICATION_MIN_ACCURACY_BPS=8000`
  of matches.
- `journalctl -u mev-db-backup.service` → snapshots landing every 15 min.

A restart does **not** reset the clock — the qualification window is computed
from canonical block records in SQLite, not from process uptime. Two things do
reset it: losing the database, and **shipping a new bot build**. The second is
the reason development has to be finished before Day 0.

## Day 7 — the money switch

When `GET /api/qualification` reports `PASS` for the target strategies:

```bash
# 1. flip the three fail-closed flags in /etc/jerseymikes/env
BROADCAST_ENABLED=true
LIVE_EXECUTION=true
I_UNDERSTAND_LIVE_RISK=yes

# 2. restart the bot
sudo systemctl restart mev-bot
```

3. In the console verify `liveArmed=true`, then "Switch to LIVE"
   (or `POST /api/mode` `{"live": true}` with the bearer token).

The broadcast lane engages only when **every** independent gate passes —
strategy lane, engineering live-candidate, risk/drawdown/gas/inventory,
boot arming, broadcast capability, authenticated live mode, that strategy's
own `PASS` verdict (or a remaining `LIVE_SMOKE_MAX` slot), no unresolved
nonce recovery, and a reserved-nonce fork simulation of the exact bundle.
Miss one and nothing is sent.

**Rollback (disarm) at any time:** set `LIVE_EXECUTION=false` (or
`BROADCAST_ENABLED=false`), restart, and/or `POST /api/mode` `{"live": false}`.
Broadcast halts at the next gate check; the shadow lanes keep running.

## Open decisions (data-gated, do not guess)

- **W6 — decided, no operator action.** `DECODE_UNIVERSAL_ROUTER` stays
  `false`; the rationale is written up in `W6_MEMO.md`. The console's W6 gap
  card still renders the reading, so a contradicting observation can reopen
  it — report one if you see it, but do not flip the flag during a soak.
- **W4** — `ARB_MAX_CYCLE_LEN` stays `3`; raise to 4–5 only when
  `atomic_arb.candidatesEmitted` is saturated at 3.
