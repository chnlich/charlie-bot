#!/usr/bin/env bash
# Switch the live CharlieBot server between the Python server and a verified Rust release.
#
#   scripts/switch-server.sh rust <full rust-backend sha> [--pane PANE]
#   scripts/switch-server.sh python [--pane PANE]
#
# The preflight changes nothing and exits 1 with the reason when a check fails. The switch then runs detached under
# setsid, because stopping the old server ends the master turn that started this command. The command prints the log
# path, waits for the verdict and exits with it: 0 when the readback and the probe turn passed; 2 when the switch
# failed and the previous implementation serves again; 3 when the restore failed too, and the log ends with the state
# the server is in.
#
# Environment: CHARLIEBOT_HOME (default ~/.charliebot), CHARLIEBOT_RELEASES_DIR (default ~/charliebot-releases),
# CHARLIEBOT_LOG_DIR (default /tmp/charliebot-logs), CHARLIEBOT_TMUX_SOCKET (a tmux -L socket; unset means the default
# server, where the production pane runs). CHARLIEBOT_SWITCH_STOP_WAIT_S and CHARLIEBOT_SWITCH_READY_WAIT_S replace the
# 60 s and 15 min waits; the tests shorten them.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"   # the runtime clone that this switch moves
HOME_DIR="${CHARLIEBOT_HOME:-$HOME/.charliebot}"
RELEASES_DIR="${CHARLIEBOT_RELEASES_DIR:-$HOME/charliebot-releases}"
LOG_DIR="${CHARLIEBOT_LOG_DIR:-/tmp/charliebot-logs}"
CURRENT="$RELEASES_DIR/current"
STOP_WAIT_S="${CHARLIEBOT_SWITCH_STOP_WAIT_S:-60}"
READY_WAIT_S="${CHARLIEBOT_SWITCH_READY_WAIT_S:-900}"
PROBE_WAIT_S=300

usage() {
  echo "usage: switch-server.sh rust <full rust-backend sha> [--pane PANE]" >&2
  echo "       switch-server.sh python [--pane PANE]" >&2
  exit 1
}

ARGS=("$@")
[ $# -ge 1 ] || usage
MODE=$1
shift
case "$MODE" in
  rust)
    [ $# -ge 1 ] || usage
    TARGET_SHA=$1
    shift
    [[ "$TARGET_SHA" =~ ^[0-9a-f]{40}$ ]] || usage
    ;;
  python) TARGET_SHA="" ;;
  *) usage ;;
esac
PANE=""
DETACHED_LOG=""
while [ $# -gt 0 ]; do
  case "$1" in
    --pane) [ $# -ge 2 ] || usage; PANE=$2; shift 2 ;;
    --detached) [ $# -ge 2 ] || usage; DETACHED_LOG=$2; shift 2 ;;   # internal: the detached run's log
    *) usage ;;
  esac
done

log() { echo "$(date '+%Y-%m-%d %H:%M:%S.%3N') switch: $*"; }
fail() { log "preflight failed: $*"; exit 1; }

# The home is read, never written, by this helper. `config` prints the port and the access key (one per line);
# `scan` prints one line per live Run (live) and one per refusal (block).
home_helper() {
  python3 -I - "$HOME_DIR" "$1" <<'PY'
import datetime
import json
import pathlib
import sys

import yaml

home = pathlib.Path(sys.argv[1])
mode = sys.argv[2]
# Backends whose runs cannot be followed after a restart: restart_reattach=False in src/backends/*/__init__.py.
NO_REATTACH = {"opencode", "antigravity"}


def proc_stat(pid):
  """(start time, state) from /proc/<pid>/stat; field 22 pins one process instance. None once the process is gone."""
  try:
    text = pathlib.Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
  except OSError:
    return None
  fields = text.rsplit(")", 1)[1].split()
  return fields[19], fields[0]


def boot_time():
  for line in pathlib.Path("/proc/stat").read_text(encoding="utf-8").splitlines():
    if line.startswith("btime "):
      return int(line.split()[1])
  raise RuntimeError("/proc/stat has no btime line")


def run_alive(run, boot):
  """The run's recorded (pid, pid_start) still names a live process that started after this boot."""
  pid, pid_start, started_at = run.get("pid"), run.get("pid_start"), run.get("started_at")
  if pid is None or pid_start is None or started_at is None:
    return False
  stat = proc_stat(pid)
  if stat is None or stat[0] != pid_start or stat[1] == "Z":
    return False
  return datetime.datetime.fromisoformat(started_at).timestamp() > boot


if mode == "config":
  config = yaml.safe_load((home / "config.yaml").read_text(encoding="utf-8"))
  credentials = yaml.safe_load((home / "credentials.yaml").read_text(encoding="utf-8"))
  print(config["server"]["port"])
  print(credentials["charliebot"].get("access_key") or "")
elif mode == "scan":
  boot = boot_time()
  for path in sorted((home / "sessions").glob("*/data/runs/*/metadata.json")):
    run = json.loads(path.read_text(encoding="utf-8"))
    if run.get("ended_at") is not None or not run_alive(run, boot):
      continue
    print(f"live\t{run['session_id']}\t{run['id']}\t{run.get('backend')}\t{run['pid']}")
    if run.get("backend") in NO_REATTACH:
      print(f"block\t{run['backend']} run {run['id']} (pid {run['pid']}) cannot be re-attached after a restart")
  # A loop's controller runs inside the server process; a restart never resumes it (src/runtime/task_recovery.py:8-10).
  for path in sorted((home / "sessions").glob("*/loops/*/state.json")):
    state = json.loads(path.read_text(encoding="utf-8"))
    controller = state.get("server_pid")
    if state.get("status") == "running" and controller is not None and pathlib.Path(f"/proc/{controller}").exists():
      print(f"block\timprove loop {path.parent.name} is running under pid {controller}")
else:
  raise SystemExit(f"unknown home_helper mode {mode}")
PY
}

# scan_home writes the live runs to $RUNS_FILE and leaves the refusals in BLOCKERS.
scan_home() {
  local scan
  scan="$(home_helper scan)" || return 1
  awk -F'\t' '$1 == "live"' <<<"$scan" > "$RUNS_FILE"
  BLOCKERS="$(awk -F'\t' '$1 == "block" { print $2 }' <<<"$scan")"
}

# tm runs tmux on the switch's server: the named private socket, or the default server.
tm() {
  if [ -n "${CHARLIEBOT_TMUX_SOCKET:-}" ]; then tmux -L "$CHARLIEBOT_TMUX_SOCKET" "$@"; else tmux "$@"; fi
}

pane_busy() { [ -n "$PANE_PID" ] && pgrep -P "$PANE_PID" > /dev/null; }

# poll SECONDS COMMAND...: succeeds as soon as COMMAND does, and fails once SECONDS have passed.
poll() {
  local end=$((SECONDS + $1))
  shift
  until "$@"; do
    [ "$SECONDS" -lt "$end" ] || return 1
    sleep 0.2
  done
}

pane_idle() { ! pane_busy; }
wait_pane_idle() { poll "$1" pane_idle; }

process_gone() { ! kill -0 "$1" 2> /dev/null; }

# wait_gone PID SECONDS: succeeds once PID has exited.
wait_gone() { poll "$2" process_gone "$1"; }

# The tmux pane whose process is an ancestor of PID: the launcher runs under the pane's shell.
pane_of_pid() {
  local pid=$1 panes pane
  panes="$(tm list-panes -a -F '#{pane_pid} #{pane_id}' 2> /dev/null)"
  while [ "${pid:-0}" -gt 1 ]; do
    pane="$(awk -v p="$pid" '$1 == p { print $2 }' <<<"$panes")"
    if [ -n "$pane" ]; then
      echo "$pane"
      return 0
    fi
    pid="$(ps -o ppid= -p "$pid" | tr -d ' ')"
  done
  return 1
}

# impl_of_pid PID: python, rust <sha>, or unknown, from the argv of the process. A release binary is named by its
# real path under the release root, so the current link resolves to the sha directory.
impl_of_pid() {
  local arg real
  while IFS= read -r -d '' arg; do
    if [ "$(basename "$arg")" = server.py ]; then
      echo python
      return 0
    fi
    real="$(readlink -f "$arg")"
    case "$real" in
      "$RELEASES_REAL"/*/charliebot-server)
        real="${real#"$RELEASES_REAL"/}"
        echo "rust ${real%/charliebot-server}"
        return 0
        ;;
    esac
  done 2> /dev/null < "/proc/$1/cmdline"
  echo unknown
}

listener_pids() { ss -Hltnp "sport = :$PORT" | grep -o 'pid=[0-9]*' | cut -d= -f2 | sort -u; }

# listener_impl: what serves the port: python, rust <sha>, unknown, several, or none.
listener_impl() {
  local pids
  pids="$(listener_pids)"
  if [ -z "$pids" ]; then
    echo none
  elif [ "$(grep -c . <<<"$pids")" -ne 1 ]; then
    echo several
  else
    impl_of_pid "$pids"
  fi
}

listener_is() { [ "$(listener_impl)" = "$1" ]; }

# api METHOD PATH [JSON]: the body lands in $WORK/body, the HTTP status in API_STATUS. The key reaches curl through a
# file descriptor, so it stays out of argv.
api() {
  if [ $# -ge 3 ]; then
    API_STATUS="$(curl -s -m 15 -X "$1" -H @<(printf 'Authorization: Bearer %s\n' "$KEY") \
      -H 'Content-Type: application/json' --data-binary "$3" -o "$WORK/body" -w '%{http_code}' \
      "http://127.0.0.1:$PORT$2")"
  else
    API_STATUS="$(curl -s -m 15 -X "$1" -H @<(printf 'Authorization: Bearer %s\n' "$KEY") \
      -o "$WORK/body" -w '%{http_code}' "http://127.0.0.1:$PORT$2")"
  fi
}

version_ok() {
  api GET /api/internal/version
  [ "$API_STATUS" = 200 ]
}

# json_field NAME: a top-level field of the response body in $WORK/body.
json_field() {
  python3 -I - "$WORK/body" "$1" <<'PY'
import json
import sys

print(json.load(open(sys.argv[1]))[sys.argv[2]])
PY
}

# last_run_state: the state of the newest Run in the runs response in $WORK/body; empty while there is none.
last_run_state() {
  python3 -I - "$WORK/body" <<'PY'
import json
import sys

items = json.load(open(sys.argv[1])).get("items") or []
print(items[-1]["state"] if items else "")
PY
}

# launch_command: the pane's command. The pane's shell does not inherit this switch's environment, so the three
# paths the launcher reads travel in the command itself.
launch_command() {
  printf 'cd %s && env CHARLIEBOT_HOME=%s CHARLIEBOT_RELEASES_DIR=%s CHARLIEBOT_LOG_DIR=%s ./scripts/start-server.sh' \
    "$(printf %q "$REPO_ROOT")" "$(printf %q "$HOME_DIR")" "$(printf %q "$RELEASES_DIR")" "$(printf %q "$LOG_DIR")"
}

# start_in_pane types the launcher into the pane. Output written after this point belongs to this launch.
start_in_pane() {
  OLD_LOG="$(readlink "$LOG_DIR/server-latest.log" 2> /dev/null || true)"
  OLD_SIZE="$(stat -c %s "$OLD_LOG" 2> /dev/null || echo 0)"
  tm send-keys -t "$PANE" -X cancel 2> /dev/null || true   # copy mode would swallow the command
  tm send-keys -t "$PANE" C-u "$(launch_command)" Enter || return 1
  NEW_STARTED=1
}

# new_output: this launch's output so far.
new_output() { tail -c "+$((NEW_OFFSET + 1))" "$NEW_LOG"; }

new_launch_ready() {
  NEW_LOG="$(readlink "$LOG_DIR/server-latest.log" 2> /dev/null || true)"
  [ -n "$NEW_LOG" ] || return 1
  if [ "$NEW_LOG" = "$OLD_LOG" ]; then NEW_OFFSET=$OLD_SIZE; else NEW_OFFSET=0; fi
  grep -q server_ready <<<"$(new_output)"
}

# wait_ready: server_ready in this launch. A launcher that exits first (a failed --check-config, a crash) fails now.
wait_ready() {
  local ready_end=$((SECONDS + READY_WAIT_S)) start_end=$((SECONDS + 30)) started=0
  while :; do
    if new_launch_ready; then
      return 0
    fi
    if pane_busy; then
      started=1
    elif [ "$started" = 1 ]; then
      log "the launcher exited before server_ready"
      return 1
    elif [ "$SECONDS" -ge "$start_end" ]; then
      log "the launcher did not start in pane $PANE"
      return 1
    fi
    if [ "$SECONDS" -ge "$ready_end" ]; then
      log "no server_ready within ${READY_WAIT_S}s"
      return 1
    fi
    sleep 0.2
  done
}

# adopted: every live run recorded before the stop has a recovery line in this launch's output.
adopted() {
  local run_id
  while IFS=$'\t' read -r _ _ run_id _ _; do
    if ! grep -q "task_recovery_resume.*run_id=$run_id" <<<"$(new_output)"; then
      log "readback: run $run_id was not adopted"
      return 1
    fi
  done < "$RUNS_FILE"
}

# serving IMPL: the listener on the port is IMPL and answers /api/internal/version with the key.
serving() {
  poll 15 listener_is "$1" || {
    log "readback: the listener is $(listener_impl), want $1"
    return 1
  }
  poll 15 version_ok || {
    log "readback: /api/internal/version did not answer 200 (got $API_STATUS)"
    return 1
  }
}

# probe_turn: a manager turn on the host's default backend, in a session of its own, must end in success.
probe_turn() {
  local session state end=$((SECONDS + PROBE_WAIT_S))
  api POST /api/sessions/ '{"name":"switch probe"}'
  [ "$API_STATUS" = 200 ] || {
    log "probe: creating a session answered $API_STATUS"
    return 1
  }
  session="$(json_field id)"
  api POST "/api/chat/$session/message" '{"content":"Reply with the single word HELLO."}'
  [ "$API_STATUS" = 202 ] || {
    log "probe: the message answered $API_STATUS"
    return 1
  }
  while :; do
    api GET "/api/sessions/$session/runs?order=asc&limit=100"
    state="$(last_run_state)"
    case "$state" in
      success)
        log "probe: the turn in session $session succeeded"
        return 0
        ;;
      failed | interrupted)
        log "probe: the turn in session $session ended $state"
        return 1
        ;;
    esac
    if [ "$SECONDS" -ge "$end" ]; then
      log "probe: no terminal state within ${PROBE_WAIT_S}s"
      return 1
    fi
    sleep 0.5
  done
}

# set_current RELEASE: point current at a release with one rename; with no release, remove it.
set_current() {
  if [ -n "$1" ]; then
    ln -sfn "$1" "$CURRENT.tmp" && mv -T "$CURRENT.tmp" "$CURRENT"
  else
    rm -f "$CURRENT"
  fi
}

stop_old() {
  log "stopping the old server, pid $LISTENER ($OLD_IMPL): SIGTERM, ${STOP_WAIT_S}s budget"
  kill -TERM "$LISTENER" 2> /dev/null
  if ! wait_gone "$LISTENER" "$STOP_WAIT_S"; then
    log "pid $LISTENER survived SIGTERM; SIGKILL"
    kill -KILL "$LISTENER" 2> /dev/null
    wait_gone "$LISTENER" 10 || return 1
  fi
  OLD_STOPPED=1
  wait_pane_idle 30 || return 1
  log "old server stopped; pane $PANE is free"
}

preflight() {
  PREV_CURRENT="$(readlink "$CURRENT" 2> /dev/null || true)"
  if [ "$MODE" = rust ]; then
    RELEASE_DIR="$RELEASES_DIR/$TARGET_SHA"
    [ -d "$RELEASE_DIR" ] || fail "no release $TARGET_SHA in $RELEASES_DIR"
    (cd "$RELEASE_DIR" && sha256sum -c sha256sums.txt) || fail "$RELEASE_DIR does not match its sha256sums.txt"
    BASE="$(tr -d '[:space:]' < "$RELEASE_DIR/PORT_BASELINE")"
    [[ "$BASE" =~ ^[0-9a-f]{40}$ ]] || fail "PORT_BASELINE of $TARGET_SHA is not a full commit sha"
  fi
  OLD_SHA="$(git -C "$REPO_ROOT" rev-parse HEAD)"
  [ -z "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=no)" ] \
    || fail "runtime clone $REPO_ROOT has local changes"
  if [ "$MODE" = rust ]; then
    git -C "$REPO_ROOT" cat-file -e "$BASE^{commit}" 2> /dev/null \
      || fail "baseline $BASE is not in the runtime clone; fetch origin first"
    git -C "$REPO_ROOT" merge-base --is-ancestor "$OLD_SHA" "$BASE" \
      || fail "runtime clone HEAD $OLD_SHA is not an ancestor of baseline $BASE"
    CHARLIEBOT_HOME="$HOME_DIR" CHARLIEBOT_REPO_ROOT="$REPO_ROOT" CHARLIEBOT_VENV_BIN="$REPO_ROOT/.venv/bin" \
      "$RELEASE_DIR/charliebot-server" --check-config \
      || fail "--check-config of $TARGET_SHA rejects the home $HOME_DIR"
  fi
  scan_home || fail "cannot read the run records in $HOME_DIR"
  [ -z "$BLOCKERS" ] || fail "a restart would lose live work: ${BLOCKERS//$'\n'/; }"
  CONFIG="$(home_helper config)" || fail "cannot read $HOME_DIR/config.yaml and credentials.yaml"
  PORT="$(sed -n 1p <<<"$CONFIG")"
  KEY="$(sed -n 2p <<<"$CONFIG")"
  [ -n "$KEY" ] || fail "no charliebot.access_key in $HOME_DIR/credentials.yaml"
  [ "$(listener_pids | grep -c .)" -eq 1 ] || fail "expected one server listening on port $PORT"
  LISTENER="$(listener_pids)"
  OLD_IMPL="$(impl_of_pid "$LISTENER")"
  [ "$OLD_IMPL" != unknown ] || fail "pid $LISTENER on port $PORT is neither the Python server nor a release binary"
  if [ -z "$PANE" ]; then
    PANE="$(pane_of_pid "$LISTENER")" || fail "no tmux pane runs the server on port $PORT; pass --pane"
  fi
  PANE_PID="$(tm display-message -p -t "$PANE" '#{pane_pid}' 2> /dev/null)" || fail "no tmux pane $PANE"
  if [ "$MODE" = rust ]; then
    WANT="rust $TARGET_SHA"
  else
    WANT=python
  fi
  log "preflight passed: $OLD_IMPL -> $WANT; clone at $OLD_SHA; pane $PANE; port $PORT"
}

# restore_failed: the state the server is in, then exit 3.
restore_failed() {
  local head
  head="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2> /dev/null)"
  log "RESTORE FAILED: $*"
  log "state: listener on port $PORT is $(listener_impl); pane $PANE busy: $(pane_busy && echo yes || echo no)"
  log "state: current -> $(readlink "$CURRENT" 2> /dev/null || echo none); clone HEAD $head"
  log "state: newest server log, last lines:"
  tail -n 12 "$(readlink "$LOG_DIR/server-latest.log" 2> /dev/null)" 2> /dev/null
  exit 3
}

# restore: put back the clone, the current link and the previous implementation, then exit 2.
restore() {
  log "restoring $OLD_IMPL at $OLD_SHA"
  if { [ "$OLD_STOPPED" = 1 ] || [ "$NEW_STARTED" = 1 ]; } && pane_busy; then
    tm send-keys -t "$PANE" C-c
    wait_pane_idle 60 || restore_failed "the pane's launcher did not exit after Ctrl-C"
  fi
  git -C "$REPO_ROOT" reset --keep -q "$OLD_SHA" || restore_failed "git reset --keep $OLD_SHA failed"
  (cd "$REPO_ROOT" && uv sync --frozen) || restore_failed "uv sync failed at $OLD_SHA"
  set_current "$PREV_CURRENT" || restore_failed "could not put current back"
  if [ "$OLD_STOPPED" = 1 ]; then
    start_in_pane || restore_failed "could not type the launcher into pane $PANE"
    wait_ready || restore_failed "the previous implementation did not reach server_ready"
    serving "$OLD_IMPL" || restore_failed "the previous implementation failed its readback"
    adopted || restore_failed "the previous implementation did not adopt the live runs"
  else
    serving "$OLD_IMPL" || restore_failed "the previous implementation is not serving"
  fi
  log "restored: $OLD_IMPL serves port $PORT; clone at $(git -C "$REPO_ROOT" rev-parse --short HEAD)"
  exit 2
}

# die: a step after the switch began failed; the previous state comes back.
die() {
  log "step failed: $*"
  restore
}

switch_forward() {
  scan_home || die "cannot read the run records in $HOME_DIR"
  [ -z "$BLOCKERS" ] || die "live work appeared after the preflight: ${BLOCKERS//$'\n'/; }"
  log "recorded $(wc -l < "$RUNS_FILE") live run(s) in $RUNS_FILE"
  stop_old || die "the old server did not stop"
  if [ "$MODE" = rust ]; then
    git -C "$REPO_ROOT" merge --ff-only -q "$BASE" || die "fast-forward to $BASE failed"
  fi
  (cd "$REPO_ROOT" && uv sync --frozen) || die "uv sync failed"
  set_current "$TARGET_SHA" || die "could not point current at $TARGET_SHA"
  start_in_pane || die "could not type the launcher into pane $PANE"
  wait_ready || die "the new server did not reach server_ready"
  serving "$WANT" || die "the new server failed its readback"
  adopted || die "a live run was not adopted"
  probe_turn || die "the probe turn failed"
  log "SWITCH COMPLETE: $WANT serves port $PORT; clone at $(git -C "$REPO_ROOT" rev-parse --short HEAD)"
  exit 0
}

run_detached() {
  LOG=$DETACHED_LOG
  RUNS_FILE="${LOG%.log}.runs"
  WORK="$(mktemp -d)"
  trap 'rm -rf "$WORK"' EXIT
  exec 9> "$LOG_DIR/switch.lock"
  flock -n 9 || {
    log "another switch holds $LOG_DIR/switch.lock"
    exit 1
  }
  RELEASES_REAL="$(readlink -f "$RELEASES_DIR")"
  OLD_STOPPED=0
  NEW_STARTED=0
  preflight
  switch_forward
}

if [ -n "$DETACHED_LOG" ]; then
  run_detached
fi

# Foreground: detach the switch from this process, and return its verdict if this process survives to see it.
mkdir -p "$LOG_DIR"
LOG="$LOG_DIR/switch_$(date +%Y%m%d_%H%M%S).log"
echo "switch log: $LOG"
setsid -w nohup "$SCRIPT_DIR/switch-server.sh" "${ARGS[@]}" --detached "$LOG" >> "$LOG" 2>&1 < /dev/null
RC=$?
tail -n 40 "$LOG"
exit "$RC"
