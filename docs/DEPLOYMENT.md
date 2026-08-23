# Deployment & alerting

Two ways to run the stack unattended — systemd (bare metal / VM) or Docker
Compose. Both leave logging to the platform (journald / the Docker logging
driver) instead of shipping a second log system inside the app; both expose
the same operational surface:

| Surface | Where |
| --- | --- |
| Health | `GET /api/health` (used by the Docker healthcheck) |
| Metrics | `GET /api/metrics` — Prometheus text: funnel per strategy/lane, latency percentiles, stats, risk envelope, kill switch, inventory |
| Alerts | `GET /api/alerts` — active set + transition history; also on the SSE feed (`alert` events) and, if `ALERT_WEBHOOK_URL` is set, POSTed there |
| Qualification | `GET /api/qualification` — continuity, tolerances, and per-strategy verdicts |
| Own settlement | `GET /api/executions` — pending/final/partial state and exact finalized economics |

## Securing the API

The API is **split into reads and writes**, and only the writes are guarded:

| | Endpoints | Auth |
| --- | --- | --- |
| Reads | `/api/health`, `/api/status`, `/api/metrics`, `/api/alerts`, `/api/stream`, `GET /api/mode`, `GET /api/risk`, … | none |
| Writes | `POST /api/mode`, `POST /api/risk`, `POST /api/risk/reset` | `Authorization: Bearer $API_AUTH_TOKEN` |

Those three writes are not cosmetic: they trip and clear the kill switch,
disable strategies, and set the risk envelope — including `bribeBps`, which at
10000 hands 100% of gross profit to the block builder. Treat the token like a
production credential.

Two rules enforce this, so a dangerous deployment fails loudly instead of
quietly:

1. **`API_BIND` defaults to `127.0.0.1:8080`.** The console reaches the bot
   *server-side* through its own `/api/bot/*` proxy, so a loopback bind is
   fully functional for the normal single-host setup.
2. **A non-loopback `API_BIND` requires `API_AUTH_TOKEN`.** The bot refuses to
   start otherwise, with a message naming both fixes.

```bash
# generate once, put it in .env
API_AUTH_TOKEN=$(openssl rand -hex 32)
```

Upgrading an existing deployment, or moving from simulation to live? The
`.env` migration and the full switch-over order are in
[`SIM_TO_LIVE.md`](SIM_TO_LIVE.md).

The console forwards the token on the operator's behalf when `BOT_API_TOKEN`
is set (see `.env.example`). It is a server-only variable — deliberately not
`NEXT_PUBLIC_`, so it never reaches the browser bundle.

Browser CORS is closed by default: `API_ALLOWED_ORIGINS` is empty, meaning no
cross-origin page can call the API at all. Only set it if some *other* browser
app must talk to the bot directly; the bundled console does not.

To reach a loopback-bound API from your laptop, tunnel rather than rebind:

```bash
ssh -N -L 8080:127.0.0.1:8080 user@host   # then use http://127.0.0.1:8080
```

## systemd

```bash
sudo useradd --system --home /nonexistent --shell /usr/sbin/nologin jerseymikes
sudo install -d -o jerseymikes -g jerseymikes -m 0750 \
  /opt/jerseymikes /opt/jerseymikes/frontend /var/lib/jerseymikes
sudo install -d -o root -g jerseymikes -m 0750 /etc/jerseymikes

# Build first: make bot-build front-build. Then install the bot binary and the
# complete production frontend (package files, node_modules and .next).
sudo install -o root -g root -m 0755 bot/target/release/mev-bot /opt/jerseymikes/mev-bot
sudo cp -a frontend/. /opt/jerseymikes/frontend/
sudo chown -R root:root /opt/jerseymikes/frontend
sudo install -o root -g jerseymikes -m 0640 .env /etc/jerseymikes/env
# In /etc/jerseymikes/env use DB_PATH=/var/lib/jerseymikes/mev.sqlite.
sudo install -o root -g root -m 0755 "$(command -v anvil)" /usr/local/bin/anvil
sudo install -o root -g root -m 0755 deploy/backup-db.sh /opt/jerseymikes/backup-db.sh
sudo cp deploy/systemd/*.service deploy/systemd/*.timer /etc/systemd/system/
sudo systemd-analyze verify /etc/systemd/system/mev-bot*.service \
  /etc/systemd/system/mev-db-backup.service
sudo systemctl daemon-reload
sudo systemctl enable --now mev-bot mev-bot-console mev-db-backup.timer
journalctl -u mev-bot -f
```

The units run as the unprivileged `jerseymikes` user with a read-only system,
private temporary/devices namespaces, no new privileges, and only
`/var/lib/jerseymikes` writable by the bot. Keep the environment file mode
`0640` because it holds both signing keys. `Restart=on-failure` does not bypass
startup nonce recovery; unresolved private bundles are cancelled or keep nonce
reuse blocked through target expiry.

## Multi-chain layout (Ethereum + Base)

One `mev-bot` process per chain — the engine is single-chain by design (one
config, one fork, one store, one nonce lane), and process isolation means a
Base crash can never touch the mainnet soak. The instance-name is the chain
slug:

| Chain | Unit | Env file | Database | Port |
| --- | --- | --- | --- | --- |
| Ethereum | `mev-bot@ethereum` (or the legacy non-templated `mev-bot`) | `/etc/jerseymikes/ethereum.env` | `/var/lib/jerseymikes/ethereum.sqlite` | `:8080` |
| Base | `mev-bot@base` | `/etc/jerseymikes/base.env` | `/var/lib/jerseymikes/base.sqlite` | `:8081` |

```bash
# per-chain env files — base.env starts from .env.example.base
sudo install -o root -g jerseymikes -m 0640 .env /etc/jerseymikes/ethereum.env
sudo install -o root -g jerseymikes -m 0640 .env.base /etc/jerseymikes/base.env

# templated units: one unit + one backup timer per chain
sudo systemctl daemon-reload
sudo systemctl enable --now mev-bot@ethereum mev-bot@base
sudo systemctl enable --now mev-db-backup@ethereum.timer mev-db-backup@base.timer
```

Per-chain isolation is **by construction**: the qualification clock, the
`LIVE_SMOKE_MAX` budget, the drawdown kill switch and the nonce lane all live
in each instance's own database / process, so arming Base can never affect
Ethereum and vice versa. Backups land per chain under
`/var/lib/jerseymikes/backups/<chain>/`.

Sequencer chains differ from mainnet in exactly three env rows (see
`.env.example.base`):

```ini
SUBMISSION_MODE=raw            # no relay market: signed txs go straight to the RPC
QUALIFICATION_BACKEND=sequencer  # the second opinion is the included block, not a relay
BRIBE_BPS=0                    # coinbase has no auction; priority fee is the ordering currency
```

The console multiplexes across the instances. Server-side env (`.env.local`
or the console service):

```ini
CHAINS="ethereum|http://127.0.0.1:8080,base|http://127.0.0.1:8081"
# optional third field = per-chain RPC for the /api/eth contract panel:
# CHAINS="ethereum|http://127.0.0.1:8080|https://eth-rpc,base|http://127.0.0.1:8081|https://base-rpc"
BOT_API_TOKEN_ETHERIUM=...   # per-chain tokens; fall back to the shared BOT_API_TOKEN
```

With `CHAINS` unset the console is single-chain on `BOT_API_URL`
(back-compat). The header switcher selects the active chain (persisted in the
browser); every panel re-keys on the switch, so a panel can never show
another chain's data, and an unreachable bot falls back to the flagged
DEMO state for that chain only.

## Database backups

The qualification clock (168 h of canonical block observations) lives in the
SQLite database. If the volume is lost or the file is corrupted on Day 6, the
clock resets to Day 0 — so `mev-db-backup.timer` snapshots it every 15 minutes
with sqlite3's online-backup API (`.backup`), which is safe against a live WAL
writer. Never `cp` the `mev.sqlite`/`-wal` files of a running bot.

* `sqlite3` must be installed on the host (`apt install sqlite3`); the backup
  unit fails loudly when it is missing.
* Snapshots land in `/var/lib/jerseymikes/backups/quarter/` (newest 96 kept —
  24 h at 15-minute cadence) with one per UTC day promoted to
  `backups/daily/` (newest 7 kept). Every snapshot passes
  `PRAGMA integrity_check` before it is trusted.
* Restore: stop the bot (`systemctl stop mev-bot`), copy the chosen snapshot
  over `DB_PATH` (and remove any stale `-wal`/`-shm` siblings), restart.
  The qualification clock continues from the observations still present in
  the restored file.
* Watch it fire: `journalctl -u mev-db-backup.service -f` (first run at the
  next quarter hour; `Persistent=true` catches up missed runs immediately).

## Docker Compose

```bash
cd deploy && docker compose --env-file ../.env up -d --build
```

The bot image carries pinned Rust/Foundry toolchains and `anvil`; the console
uses pinned Node 22. Both run unprivileged with read-only root filesystems,
dropped capabilities, health checks, and loopback-only host publishing. SQLite
persists on the `bot-data` volume. Back it up before upgrades: it contains
qualification evidence, nonce reservations, submitted payloads and finalized
execution outcomes—not just dashboard history.

Compose sets `API_BIND=0.0.0.0:8080` inside the bot container — the console
container has to reach it over the compose network — and therefore **requires
`API_AUTH_TOKEN` in `../.env`**; `docker compose` errors out naming the
variable if it is missing. The published port is bound to host loopback
(`127.0.0.1:8080:8080`), so the API is not exposed to your network even though
it is `0.0.0.0` inside the container.

Point your Prometheus at `http://127.0.0.1:8080/api/metrics` (or scrape the
container directly over the compose network).

## Alert rules (evaluated every `ALERT_EVAL_SECS`, default 30s)

| Rule | Fires when | Severity |
| --- | --- | --- |
| `kill_switch` | drawdown kill switch tripped | critical |
| `drawdown_approaching` | cumulative net below −50% of the drawdown limit | warning |
| `head_stalled` | no new head for `ALERT_HEAD_STALL_SECS` (default 60s) — endpoint/node down | critical |
| `pending_stalled` | no mempool tx for `ALERT_PENDING_STALL_SECS` (default 180s) while a WS feed is configured | warning |
| `no_mempool_feed` | `ETH_WS_URL` unset — the pending path is dark by configuration | info |
| `conversion_collapsed` | a strategy with ≥ `ALERT_MIN_CANDIDATES` live candidates converts < `ALERT_MIN_CONVERSION_PCT` (default 100 @ 2%) | warning |
| `reorg_observed` | a re-org since the last pass | warning |

Alerts are level-based: a rule stays *active* while its condition holds and
emits a *resolved* transition when it clears (both land in the history and
the webhook). Tunables live in `.env` — see the `ALERTING` section of
`.env.example`.

## Prometheus scrape config

```yaml
scrape_configs:
  - job_name: jerseymikes
    scrape_interval: 15s
    static_configs:
      - targets: ["your-host:8080"]
```

Useful starting rules: `mev_kill_switch_tripped == 1`,
`mev_alerts_active > 0`, `increase(mev_funnel_submittable{lane="live"}[1h]) == 0`
after a warm-up.
