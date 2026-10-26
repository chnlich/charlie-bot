#!/usr/bin/env bash
# CharlieBot server launcher — tees server logs to /tmp so they persist on disk.
# Start/stop stays manual: runs in the foreground, Ctrl-C to stop.
# A current link in the release root selects the Rust release; without it the Python server runs.
#
# Each server start has a timestamp-and-PID scope so earlier runs can keep their processes.
# Before starting, the launcher checks both leaf slices against their unit files; a missing slice or different limit
# stops the start visibly. --preflight checks the slices and creates a trial scope without writing a server log.
set -uo pipefail

# Resolve repo root from the script location — no absolute paths (shared repo).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
UNIT_DIR="$SCRIPT_DIR/systemd"

# Server and trial scopes use the same slice and swap limit so both starts test the server's resource settings.
run_in_scope() {
  local unit_name="$1"
  shift
  systemd-run --user --scope --unit="$unit_name" --slice=charliebot.slice -p MemorySwapMax=0 -- "$@"
}

# One setting's value from a unit file: the last assignment wins in systemd, so the last
# assignment printed here is the effective one; a missing key reads as infinity, the value
# systemd reports for an unset limit.
_unit_setting() {
  local file="$1" key="$2" value=""
  while IFS='=' read -r name raw || [ -n "$name" ]; do
    if [ "$name" = "$key" ]; then
      value="$raw"
    fi
  done < "$file"
  printf '%s' "${value:-infinity}"
}

# A systemd size (34G, 8M, 0, infinity) in bytes, the form `systemctl show` reports.
_systemd_bytes() {
  local value="$1" number unit
  case "$value" in
    infinity|max)
      printf 'infinity'
      return 0
      ;;
  esac
  number="${value%[KMGTPE]}"
  unit="${value:${#number}}"
  case "$unit" in
    "") ;;
    K) number=$((number * 1024)) ;;
    M) number=$((number * 1024 * 1024)) ;;
    G) number=$((number * 1024 * 1024 * 1024)) ;;
    T) number=$((number * 1024 * 1024 * 1024 * 1024)) ;;
    P) number=$((number * 1024 * 1024 * 1024 * 1024 * 1024)) ;;
    E) number=$((number * 1024 * 1024 * 1024 * 1024 * 1024 * 1024)) ;;
    *)
      echo "preflight: unsupported size unit in '$value'" >&2
      return 1
      ;;
  esac
  printf '%s' "$number"
}

# Assert both leaf slices are active and carry exactly their unit files' limits. Prints every
# difference and returns non-zero on any; a missing limit must stop the start, not be inferred.
preflight_slices() {
  local rc=0 unit file shown active key expected actual raw
  for unit in charliebot-agents.slice charliebot-repair.slice; do
    file="$UNIT_DIR/$unit"
    if [ ! -f "$file" ]; then
      echo "preflight: $unit has no unit file at $file" >&2
      rc=1
      continue
    fi
    if ! shown="$(systemctl --user show "$unit" -p ActiveState -p MemoryHigh -p MemoryMax -p MemorySwapMax)"; then
      echo "preflight: systemctl --user show $unit failed" >&2
      rc=1
      continue
    fi
    active="$(printf '%s\n' "$shown" | sed -n 's/^ActiveState=//p')"
    if [ "$active" != "active" ]; then
      echo "preflight: $unit is '${active:-unknown}', expected 'active' (run scripts/setup.sh)" >&2
      rc=1
    fi
    for key in MemoryHigh MemoryMax MemorySwapMax; do
      raw="$(_unit_setting "$file" "$key")"
      case "$unit:$key:$raw" in
        charliebot-agents.slice:MemoryHigh:infinity|\
        charliebot-agents.slice:MemoryMax:infinity|\
        charliebot-agents.slice:MemorySwapMax:infinity|\
        charliebot-repair.slice:MemoryMax:infinity|\
        charliebot-repair.slice:MemorySwapMax:infinity)
          echo "preflight: $unit unit file has no finite $key" >&2
          rc=1
          continue
          ;;
      esac
      if ! expected="$(_systemd_bytes "$raw")"; then
        rc=1
        continue
      fi
      actual="$(printf '%s\n' "$shown" | sed -n "s/^$key=//p")"
      if [ "$actual" != "$expected" ]; then
        echo "preflight: $unit $key is '${actual:-missing}', unit file says '$raw' ($expected bytes)" >&2
        rc=1
      fi
    done
  done
  return "$rc"
}

main() {
  TS="$(date +%Y%m%d_%H%M%S)"

  if [ "${1:-}" = "--preflight" ]; then
    if ! preflight_slices; then
      exit 1
    fi
    PREFLIGHT_SCOPE_NAME="charliebot-preflight-${TS}-$$"
    if ! run_in_scope "$PREFLIGHT_SCOPE_NAME" true; then
      echo "launcher: systemd scope preflight failed for $PREFLIGHT_SCOPE_NAME" >&2
      exit 1
    fi
    exit 0
  fi

  cd "$REPO_ROOT"

  # Tests drive the real launcher through these variables; the defaults stay the
  # live log directory and release root.
  LOG_DIR="${CHARLIEBOT_LOG_DIR:-/tmp/charliebot-logs}"
  RELEASES_DIR="${CHARLIEBOT_RELEASES_DIR:-$HOME/charliebot-releases}"
  CURRENT="$RELEASES_DIR/current"
  mkdir -p "$LOG_DIR"
  LOG="$LOG_DIR/server_${TS}.log"
  SERVER_SCOPE_NAME="charliebot-server-${TS}-$$"
  ln -sfn "$LOG" "$LOG_DIR/server-latest.log"   # stable pointer to the newest log

  # -e or -L: a dangling current still selects Rust, so a broken release fails here instead of
  # starting Python.
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
  echo "  scope  -> $SERVER_SCOPE_NAME"

  if ! preflight_slices; then
    echo "launcher: the memory slices do not match scripts/systemd/; not starting the server" >&2
    exit 1
  fi

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
    run_in_scope "$SERVER_SCOPE_NAME" "$CURRENT/charliebot-server" 2>&1 | tee -i -a "$LOG"
  else
    # PYTHONUNBUFFERED: avoid block buffering when piped, so log lines flush promptly.
    PYTHONUNBUFFERED=1 run_in_scope "$SERVER_SCOPE_NAME" uv run python3 server.py 2>&1 | tee -i -a "$LOG"
  fi
  SERVER_RC="${PIPESTATUS[0]}"
  EXIT_LINE="$(date '+%Y-%m-%d %H:%M:%S.%3N') launcher: server exited rc=${SERVER_RC}"
  echo "$EXIT_LINE" >> "$LOG"
  echo "$EXIT_LINE"
  exit "$SERVER_RC"
}

# Sourcing the script exposes preflight_slices to the tests without starting anything.
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  case "$#" in
    0) main ;;
    1)
      [ "$1" = "--preflight" ] || {
        echo "usage: start-server.sh [--preflight]" >&2
        exit 1
      }
      main "$1"
      ;;
    *)
      echo "usage: start-server.sh [--preflight]" >&2
      exit 1
      ;;
  esac
fi
