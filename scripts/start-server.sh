#!/usr/bin/env bash
# CharlieBot server launcher — tees server logs to /tmp so they persist on disk.
# Start/stop stays manual: runs in the foreground, Ctrl-C to stop.
# A current link in the release root selects the Rust release; without it the Python server runs.
set -uo pipefail

# Resolve repo root from the script location — no absolute paths (shared repo).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

# Tests drive the real launcher through these variables; the defaults stay the
# live log directory and release root.
LOG_DIR="${CHARLIEBOT_LOG_DIR:-/tmp/charliebot-logs}"
RELEASES_DIR="${CHARLIEBOT_RELEASES_DIR:-$HOME/charliebot-releases}"
CURRENT="$RELEASES_DIR/current"
mkdir -p "$LOG_DIR"
TS="$(date +%Y%m%d_%H%M%S)"
LOG="$LOG_DIR/server_${TS}.log"
ln -sfn "$LOG" "$LOG_DIR/server-latest.log"   # stable pointer to the newest log

# -e or -L: a dangling current still selects Rust, so a broken release fails here instead of starting Python.
if [ -e "$CURRENT" ] || [ -L "$CURRENT" ]; then
  IMPL=rust
  RELEASE_SHA="$(basename "$(readlink -f "$CURRENT")")"
  echo "$(date '+%Y-%m-%d %H:%M:%S.%3N') launcher: implementation=rust release=${RELEASE_SHA}" | tee -a "$LOG"
else
  IMPL=python
fi

echo "CharlieBot server starting (repo: $REPO_ROOT)"
echo "  log    -> $LOG"
echo "  latest -> $LOG_DIR/server-latest.log"

# tee keeps console output and writes the file; PIPESTATUS preserves the real exit code.
# -i makes tee ignore SIGINT: Ctrl-C reaches the whole foreground process group, and
# tee must outlive it to record the server's shutdown lines, then ends on EOF when
# the server exits.
if [ "$IMPL" = rust ]; then
  # The release reads its configuration and venv from these two variables. --check-config validates the home without
  # taking its writer lock or starting the server; a failure stops here with the binary's error and the rollback.
  export CHARLIEBOT_REPO_ROOT="$REPO_ROOT" CHARLIEBOT_VENV_BIN="$REPO_ROOT/.venv/bin"
  "$CURRENT/charliebot-server" --check-config
  CHECK_RC=$?
  if [ "$CHECK_RC" -ne 0 ]; then
    echo "launcher: --check-config failed (rc=${CHECK_RC}); roll back with: scripts/switch-server.sh python"
    exit "$CHECK_RC"
  fi
  "$CURRENT/charliebot-server" 2>&1 | tee -i -a "$LOG"
else
  # PYTHONUNBUFFERED: avoid block buffering when piped, so log lines flush promptly.
  PYTHONUNBUFFERED=1 uv run python3 server.py 2>&1 | tee -i -a "$LOG"
fi
SERVER_RC="${PIPESTATUS[0]}"
EXIT_LINE="$(date '+%Y-%m-%d %H:%M:%S.%3N') launcher: server exited rc=${SERVER_RC}"
echo "$EXIT_LINE" >> "$LOG"
echo "$EXIT_LINE"
exit "$SERVER_RC"
