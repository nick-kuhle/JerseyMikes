#!/usr/bin/env bash
# JerseyMikes — WAL-safe hot backup of the qualification database.
#
# The 7-day qualification clock lives in the SQLite database (canonical block
# observations, relay comparisons, actual matches). If the volume is lost on
# Day 6 the clock resets to Day 0, so the database is snapshotted on a timer
# (deploy/systemd/mev-db-backup.timer, every 15 minutes by default).
#
# Uses sqlite3's online-backup API (".backup"), which is safe against a live
# WAL writer — never cp the db/wal files directly while the bot is running.
#
# Usage:   backup-db.sh [DB_PATH] [BACKUP_DIR]
# Env:     DB_PATH      (default /var/lib/jerseymikes/mev.sqlite)
#          BACKUP_DIR   (default /var/lib/jerseymikes/backups)
# Retention: backups/quarter/ keeps the newest 96 snapshots (24h at 15-min
#          cadence); one snapshot per day is promoted to backups/daily/,
#          keeping the newest 7. Pruning is by mtime, not name parsing.
#
# Exits non-zero on any failure so the systemd unit shows degraded.

set -euo pipefail

DB_PATH="${1:-${DB_PATH:-/var/lib/jerseymikes/mev.sqlite}}"
BACKUP_DIR="${2:-${BACKUP_DIR:-/var/lib/jerseymikes/backups}}"
KEEP_QUARTER=96   # 24h of 15-minute snapshots
KEEP_DAILY=7      # a week of daily restore points

log() { printf '[backup-db] %s\n' "$*" >&2; }

if ! command -v sqlite3 >/dev/null 2>&1; then
    log "sqlite3 not found — install it (apt install sqlite3) or the qualification clock has no backups"
    exit 1
fi
if [[ ! -f "$DB_PATH" ]]; then
    log "database not found at $DB_PATH — nothing to back up yet (fresh install?)"
    exit 0
fi

mkdir -p "$BACKUP_DIR/quarter" "$BACKUP_DIR/daily"

stamp="$(date -u +%Y%m%dT%H%M%S)"
dest="$BACKUP_DIR/quarter/mev-$stamp.sqlite"
tmp="$dest.partial"

# .backup acquires the right locks and produces a standalone, WAL-merged file.
sqlite3 "$DB_PATH" ".backup '$tmp'"
mv -f "$tmp" "$dest"

# Integrity-check the snapshot, not the live database.
check="$(sqlite3 "$dest" 'PRAGMA integrity_check;' 2>&1)"
if [[ "$check" != "ok" ]]; then
    log "integrity_check failed for $dest: $check — removing suspect snapshot"
    rm -f "$dest"
    exit 1
fi

# Promote the first snapshot of each UTC day into the daily tier.
day="$(date -u +%Y%m%d)"
if ! ls "$BACKUP_DIR/daily/mev-$day"-*.sqlite >/dev/null 2>&1; then
    cp -p "$dest" "$BACKUP_DIR/daily/mev-$day.sqlite"
fi

# Prune: newest N by modification time survive.
ls -t "$BACKUP_DIR/quarter" 2>/dev/null | tail -n +$((KEEP_QUARTER + 1)) \
    | while read -r f; do rm -f "$BACKUP_DIR/quarter/$f"; done
ls -t "$BACKUP_DIR/daily" 2>/dev/null | tail -n +$((KEEP_DAILY + 1)) \
    | while read -r f; do rm -f "$BACKUP_DIR/daily/$f"; done

log "backed up $DB_PATH -> $dest ($(du -h "$dest" | cut -f1))"
