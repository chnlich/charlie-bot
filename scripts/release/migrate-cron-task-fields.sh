#!/usr/bin/env bash
# One-off migration: drop the top-level `backend` and `project` keys from the cron task files.
#
#   scripts/release/migrate-cron-task-fields.sh
#
# A task file no longer names a backend or a project: the bound session owns both, and the release that reads the files
# this way refuses any file that still carries either key. Run the migration detached, right after that release starts
# listening:
#
#   setsid scripts/release/migrate-cron-task-fields.sh > migrate.log 2>&1 < /dev/null &
#
# The script edits only the top-level `backend:` and `project:` lines of $CHARLIEBOT_HOME/config.d/cron.d/*.yaml. Every
# other line stays: `improve_host`, the key order, comments, and the nested step `backend:` values.
#
# Steps, in order:
#   1. Wait up to MIGRATE_READY_TIMEOUT_S seconds (default 60) for /api/internal/version to answer 200. On timeout it
#      exits 1 and changes no file.
#   2. Check every file: a top-level `backend:` or `project:` line must hold its value on the same line.
#   3. Copy config.d/cron.d to backups/cron.d-<UTC timestamp> under $CHARLIEBOT_HOME, then delete those lines.
#   4. Read back through the API. Every task row loads without a broken entry, and each row names the session its file
#      named before the edit. Every bound session's backend is a configured backends.options id or backends.pools id.
#      Each file differs from its backup only by deleted backend or project lines. It exits 0 when all of these hold.
#
# A readback failure exits non-zero and leaves the edited files in place. To undo the edit, copy the backup back:
#   cp -a "$CHARLIEBOT_HOME/backups/cron.d-<timestamp>/." "$CHARLIEBOT_HOME/config.d/cron.d/"
#
# Environment:
#   CHARLIEBOT_HOME           the profile home (default ~/.charliebot)
#   CHARLIEBOT_BIN            the charliebot binary that reads the home's port, access key and config (default
#                             charliebot on PATH)
#   MIGRATE_READY_TIMEOUT_S   the readiness wait in seconds (default 60)

set -euo pipefail

HOME_DIR="${CHARLIEBOT_HOME:-$HOME/.charliebot}"
BIN="${CHARLIEBOT_BIN:-charliebot}"
TIMEOUT_S="${MIGRATE_READY_TIMEOUT_S:-60}"
CRON_DIR="$HOME_DIR/config.d/cron.d"

log() { printf '%s %s\n' "$(date -u +%FT%TZ)" "$*"; }
fail() { log "migrate: $*"; exit 1; }

[ -d "$CRON_DIR" ] || fail "no cron directory at $CRON_DIR"
shopt -s nullglob
FILES=("$CRON_DIR"/*.yaml)
[ "${#FILES[@]}" -gt 0 ] || fail "no task files in $CRON_DIR"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
chmod 700 "$WORK"

# The port and access key come from the home, through the same read the switch makes.
CONFIG="$("$BIN" switch-helper home-helper config "$HOME_DIR")" || fail "cannot read the config and credentials of $HOME_DIR"
PORT="$(sed -n 1p <<<"$CONFIG")"
KEY="$(sed -n 2p <<<"$CONFIG")"
[ -n "$PORT" ] && [ -n "$KEY" ] || fail "the home names no port or access key"
printf 'Authorization: Bearer %s\n' "$KEY" > "$WORK/auth.header"

# api METHOD PATH: sets API_STATUS and leaves the response body in $WORK/body.
api() {
  API_STATUS="$(curl -s -m 10 -X "$1" -H @"$WORK/auth.header" -o "$WORK/body" -w '%{http_code}' \
    "http://127.0.0.1:$PORT$2")" || API_STATUS=000
}

# 1. Readiness.
deadline=$((SECONDS + TIMEOUT_S))
until api GET /api/internal/version && [ "$API_STATUS" = 200 ]; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    fail "the server on port $PORT did not answer /api/internal/version with 200 within ${TIMEOUT_S}s; no file changed"
  fi
  sleep 0.2
done
log "migrate: the server on port $PORT is ready"

# 2. Every removed key must hold its value on its own line, checked before any file changes.
for file in "${FILES[@]}"; do
  while IFS= read -r line; do
    case "$line" in
      backend:*|project:*)
        [[ "$line" =~ ^(backend|project):[[:space:]]+[^[:space:]] ]] \
          || fail "$(basename "$file"): the top-level key '${line%%:*}' has no value on its line; edit the file by hand"
        ;;
    esac
  done < "$file"
done

# 3. Backup, the session each task names before the edit, then the edit.
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
BACKUP="$HOME_DIR/backups/cron.d-$STAMP"
[ ! -e "$BACKUP" ] || fail "the backup $BACKUP already exists"
mkdir -p "$HOME_DIR/backups"
cp -a "$CRON_DIR" "$BACKUP"
log "migrate: backed up $CRON_DIR to $BACKUP"

: > "$WORK/sessions_before.txt"
for file in "${FILES[@]}"; do
  session="$(sed -n -E 's/^session_id:[[:space:]]*["'\'']?([^"'\'' ]*)["'\'']?[[:space:]]*$/\1/p' "$file" | head -n 1)"
  case "$session" in
    null|Null|NULL|~) session="" ;;
  esac
  printf '%s\t%s\n' "$(basename "$file" .yaml)" "$session" >> "$WORK/sessions_before.txt"
done

removed=0
for file in "${FILES[@]}"; do
  count="$(grep -c -E '^(backend|project):' "$file" || true)"
  cp "$file" "$WORK/edit.yaml"
  sed -i -E '/^(backend|project):/d' "$WORK/edit.yaml"
  cat "$WORK/edit.yaml" > "$file"
  removed=$((removed + count))
done
log "migrate: deleted $removed top-level backend or project line(s) from ${#FILES[@]} file(s)"

# 4. Readback: the rows, each bound session's backend, the configured ids, and the file diffs.
api GET /api/cron/tasks
[ "$API_STATUS" = 200 ] || fail "GET /api/cron/tasks answered $API_STATUS"
cp "$WORK/body" "$WORK/rows.json"

"$BIN" config get backends > "$WORK/backends.json" || fail "cannot read the backends config"

python3 -I -c '
import json, sys
rows = json.load(open(sys.argv[1]))
for session in sorted({row["session_id"] for row in rows if row.get("session_id")}):
    print(session)
' "$WORK/rows.json" > "$WORK/bound_sessions.txt"

: > "$WORK/session_backends.txt"
while IFS= read -r session; do
  api GET "/api/sessions/$session"
  [ "$API_STATUS" = 200 ] || fail "GET /api/sessions/$session answered $API_STATUS"
  python3 -I -c '
import json, sys
print(sys.argv[1], json.load(open(sys.argv[2]))["backend"])
' "$session" "$WORK/body" >> "$WORK/session_backends.txt"
done < "$WORK/bound_sessions.txt"

python3 -I -c '
import json, sys
backends = json.load(open(sys.argv[1]))
for entry in backends.get("options", []) + backends.get("pools", []):
    print(entry["id"])
' "$WORK/backends.json" > "$WORK/config_ids.txt"

python3 -I - "$WORK/rows.json" "$WORK/sessions_before.txt" "$WORK/session_backends.txt" "$WORK/config_ids.txt" \
  "${#FILES[@]}" <<'PY' || fail "the readback failed (see the message above)"
import json, sys

rows_path, before_path, backends_path, config_path, file_count = sys.argv[1:]
rows = json.load(open(rows_path))
broken = sorted(row["name"] for row in rows if row.get("broken"))
if broken:
    raise SystemExit(f"broken rows: {', '.join(broken)}")
if len(rows) != int(file_count):
    raise SystemExit(f"{len(rows)} rows load for {file_count} files")
before = sorted(tuple(line.rstrip("\n").split("\t", 1)) for line in open(before_path) if line.strip())
after = sorted((row["name"], row.get("session_id") or "") for row in rows)
if after != before:
    raise SystemExit("a row names a different session than its file did before the edit")
known = {line.strip() for line in open(config_path) if line.strip()}
for line in open(backends_path):
    session, backend = line.split()
    if backend not in known:
        raise SystemExit(f"session {session} names backend '{backend}', which the config does not hold")
print(f"rows: {len(rows)} load and none is broken; sessions as before; "
      f"{len(open(backends_path).readlines())} bound session(s) name configured backends")
PY

for file in "${FILES[@]}"; do
  base="$(basename "$file")"
  extra="$(diff "$BACKUP/$base" "$file" | grep -E '^[<>]' | grep -v -E '^< (backend|project):' || true)"
  [ -z "$extra" ] || fail "$base changed beyond its backend and project lines: $extra"
done
log "migrate: each file differs from its backup only by the deleted lines"
log "migrate: done"
