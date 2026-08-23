#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

fail() { echo "deployment validation: $*" >&2; exit 1; }

# Host publication must stay loopback-only even though the bot binds the
# compose network internally.
grep -q '127.0.0.1:8080:8080' deploy/docker-compose.yml || fail "bot port is not loopback-only"
grep -q '127.0.0.1:3000:3000' deploy/docker-compose.yml || fail "console port is not loopback-only"
! grep -Eq 'FROM [^ ]+:(latest|stable)( |$)' deploy/Dockerfile.* || fail "container base image is not pinned"
grep -q '^User=jerseymikes$' deploy/systemd/mev-bot.service || fail "bot systemd unit is not unprivileged"
grep -q '^ProtectSystem=strict$' deploy/systemd/mev-bot.service || fail "bot systemd filesystem is not read-only"
grep -q '^ReadWritePaths=/var/lib/jerseymikes$' deploy/systemd/mev-bot.service || fail "bot state path is not explicit"

if command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
  API_AUTH_TOKEN=validation ETH_HTTP_URL=https://example.invalid \
    docker compose -f deploy/docker-compose.yml config --quiet
else
  echo "deployment validation: docker compose unavailable; static checks only" >&2
fi

if command -v systemd-analyze >/dev/null 2>&1; then
  tmpdir=$(mktemp -d)
  trap 'rm -rf "$tmpdir"' EXIT
  for unit in deploy/systemd/*.service; do
    sed -E \
      -e 's|^ExecStart=.*|ExecStart=/bin/true|' \
      -e 's|^WorkingDirectory=.*|WorkingDirectory=/tmp|' \
      -e 's|^EnvironmentFile=.*|EnvironmentFile=-/dev/null|' \
      "$unit" > "$tmpdir/$(basename "$unit")"
  done
  systemd-analyze verify "$tmpdir"/*.service
else
  echo "deployment validation: systemd-analyze unavailable; static checks only" >&2
fi

echo "deployment validation: ok"
