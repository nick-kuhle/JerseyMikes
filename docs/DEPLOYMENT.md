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

## systemd

```bash
sudo mkdir -p /opt/jerseymikes /etc/jerseymikes
# build + install the bot binary and the console (see Makefile: bot-build, front-build)
sudo cp deploy/systemd/*.service /etc/systemd/system/
sudo cp .env /etc/jerseymikes/env            # the bot's own .env, minus secrets you keep local-only
sudo systemctl daemon-reload
sudo systemctl enable --now mev-bot mev-bot-console
journalctl -u mev-bot -f                      # logs
```

The bot unit expects `/opt/jerseymikes/mev-bot` and **anvil on PATH**
(`Environment=PATH` is a stub — extend it if Foundry lives elsewhere, e.g.
`/root/.foundry/bin`). `Restart=always` covers crashes; alerting covers
everything that crashes *quietly*.

## Docker Compose

```bash
cd deploy && docker compose --env-file ../.env up -d --build
```

The bot image carries `anvil` (copied from the official Foundry image);
SQLite persists on the `bot-data` volume. Point your Prometheus at
`http://<host>:8080/api/metrics`.

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
