#!/usr/bin/env bash
# CharlieBot server launcher — tees server logs to /tmp so they persist on disk.
# Start/stop stays manual: runs in the foreground, Ctrl-C to stop.
set -uo pipefail

# Resolve repo root from the script location — no absolute paths (shared repo).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

# Tests drive the real launcher through this variable; the default stays the
# live log directory.
LOG_DIR="${CHARLIEBOT_LOG_DIR:-/tmp/charliebot-logs}"
mkdir -p "$LOG_DIR"
TS="$(date +%Y%m%d_%H%M%S)"
LOG="$LOG_DIR/server_${TS}.log"
ln -sfn "$LOG" "$LOG_DIR/server-latest.log"   # stable pointer to the newest log

echo "CharlieBot server starting (repo: $REPO_ROOT)"
echo "  log    -> $LOG"
echo "  latest -> $LOG_DIR/server-latest.log"

# PYTHONUNBUFFERED: avoid block buffering when piped, so log lines flush promptly.
# tee keeps console output and writes the file; PIPESTATUS preserves the real exit code.
# -i makes tee ignore SIGINT: Ctrl-C reaches the whole foreground process group, and
# tee must outlive it to record the server's shutdown lines, then ends on EOF when
# the server exits.
PYTHONUNBUFFERED=1 uv run python3 server.py 2>&1 | tee -i -a "$LOG"
SERVER_RC="${PIPESTATUS[0]}"
EXIT_LINE="$(date '+%Y-%m-%d %H:%M:%S.%3N') launcher: server exited rc=${SERVER_RC}"
echo "$EXIT_LINE" >> "$LOG"
echo "$EXIT_LINE"
exit "$SERVER_RC"
