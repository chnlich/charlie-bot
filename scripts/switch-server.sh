#!/usr/bin/env bash
# Switch the live CharlieBot server to a verified Rust release.
#
#   scripts/switch-server.sh rust <full rust-backend sha> [--pane PANE]
#
# The preflight checks release, clone, runs and config, then asks start-server.sh to test its slices and scope.
# A refusal changes nothing and exits 1. The switch then runs detached under setsid, because stopping the old server
# ends the master turn that started this command. The command prints the log
# path, waits for the verdict and exits with it: 0 when the readback and the probe turn passed; 2 when the switch
# failed and the previous implementation serves again; 3 when the restore failed too, and the log ends with the state
# the server is in; 4 when the switch completed but a recorded run died during it: the new server keeps serving, nothing
# is restored, and the log names the run and its outcome. When the pane stays busy after Ctrl-C, the restore sends
# SIGTERM, then SIGKILL, to the server processes under the pane.
#
# The probe turn runs last, in a session of its own. It passes once the run records its first assistant output (a
# message or a tool call) or ends success. The archive then stops every run of the session that has not ended, and
# repeats the stop and the archive while the server answers 409: a follow-up run, such as the web reply nudge, can start
# after the turn. It reads run states and message roles only, so the model's text cannot fail a healthy server.
#
# Environment: CHARLIEBOT_HOME (default ~/.charliebot), CHARLIEBOT_RELEASES_DIR (default ~/charliebot-releases),
# CHARLIEBOT_LOG_DIR (default /tmp/charliebot-logs), CHARLIEBOT_TMUX_SOCKET (a tmux -L socket; unset means the default
# server, where the production pane runs). CHARLIEBOT_SWITCH_CTRL_C_WAIT_S, CHARLIEBOT_SWITCH_STOP_WAIT_S and
# CHARLIEBOT_SWITCH_READY_WAIT_S replace the 60 s, 60 s and 15 min waits. CHARLIEBOT_SWITCH_PROBE_WAIT_S and
# CHARLIEBOT_SWITCH_PROBE_CANCEL_WAIT_S replace the 300 s and 60 s probe waits. The tests shorten the stop, ready and
# probe waits.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"   # the runtime clone that this switch moves
HOME_DIR="${CHARLIEBOT_HOME:-$HOME/.charliebot}"
RELEASES_DIR="${CHARLIEBOT_RELEASES_DIR:-$HOME/charliebot-releases}"
LOG_DIR="${CHARLIEBOT_LOG_DIR:-/tmp/charliebot-logs}"
CURRENT="$RELEASES_DIR/current"
CTRL_C_WAIT_S="${CHARLIEBOT_SWITCH_CTRL_C_WAIT_S:-60}"
STOP_WAIT_S="${CHARLIEBOT_SWITCH_STOP_WAIT_S:-60}"
READY_WAIT_S="${CHARLIEBOT_SWITCH_READY_WAIT_S:-900}"
PROBE_WAIT_S="${CHARLIEBOT_SWITCH_PROBE_WAIT_S:-300}"
PROBE_CANCEL_WAIT_S="${CHARLIEBOT_SWITCH_PROBE_CANCEL_WAIT_S:-60}"

usage() {
  echo "usage: switch-server.sh rust <full rust-backend sha> [--pane PANE]" >&2
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
  *) usage ;;
esac
PANE=""
DETACHED_LOG=""
DIED_RUNS=0   # no restore can recover an ended run, so keep count until the switch exits 4
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
# `scan` prints one line per live Run (live) and one per refusal (block); `alive` prints alive or ended for a recorded
# (pid, pid start); `outcome` reads the terminal fact so readback can distinguish a completed run from one the stop
# killed.
home_helper() {
  python3 -I - "$HOME_DIR" "$@" <<'PY'
import datetime
import json
import pathlib
import sys
import time

import yaml

home = pathlib.Path(sys.argv[1])
mode = sys.argv[2]
# Owner: ServerConfig.port in src/infra/config.py.
DEFAULT_SERVER_PORT = 18498
# Owner: the restart_reattach trait in src/runtime/hooks/backend_type_registration.py,
# set by register() in src/backends/*/__init__.py.
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


def process_instance(pid, pid_start):
  """(pid, pid_start) still names one live process instance; field 22 of /proc/<pid>/stat pins that instance, so a
  recycled pid never counts. A zombie has ended: its argv is gone and nothing can follow it."""
  stat = proc_stat(pid)
  return stat is not None and stat[0] == pid_start and stat[1] != "Z"


def run_alive(run, boot):
  """The run's recorded (pid, pid_start) still names a live process that started after this boot."""
  pid, pid_start, started_at = run.get("pid"), run.get("pid_start"), run.get("started_at")
  if pid is None or pid_start is None or started_at is None:
    return False
  if not process_instance(pid, pid_start):
    return False
  return datetime.datetime.fromisoformat(started_at).timestamp() > boot


if mode == "config":
  config = yaml.safe_load((home / "config.yaml").read_text(encoding="utf-8"))
  credentials = yaml.safe_load((home / "credentials.yaml").read_text(encoding="utf-8"))
  print(config.get("server", {}).get("port", DEFAULT_SERVER_PORT))
  print(credentials["charliebot"].get("access_key") or "")
elif mode == "scan":
  config = yaml.safe_load((home / "config.yaml").read_text(encoding="utf-8"))
  types = {option["id"]: option["type"] for option in config["backends"]["options"]}
  boot = boot_time()
  for path in sorted((home / "sessions").glob("*/data/runs/*/metadata.json")):
    run = json.loads(path.read_text(encoding="utf-8"))
    if run.get("ended_at") is not None or not run_alive(run, boot):
      continue
    print(f"live\t{run['session_id']}\t{run['id']}\t{run.get('backend')}\t{run['pid']}\t{run['pid_start']}")
    backend = run.get("backend")
    backend_type = types.get(backend)
    if backend_type is None:
      print(f"block\tbackend {backend!r} of run {run['id']} (pid {run['pid']}) is not in backends.options; "
            "restart re-attachment is unknown")
    elif backend_type in NO_REATTACH:
      print(f"block\t{backend} run {run['id']} (type {backend_type}, pid {run['pid']}) "
            "cannot be re-attached after a restart")
  # A loop's controller runs inside the server process; a restart never resumes it (rust/crates/charliebot-tasks/src/recovery.rs).
  for path in sorted((home / "sessions").glob("*/loops/*/state.json")):
    state = json.loads(path.read_text(encoding="utf-8"))
    controller = state.get("server_pid")
    if state.get("status") == "running" and controller is not None and pathlib.Path(f"/proc/{controller}").exists():
      print(f"block\timprove loop {path.parent.name} is running under pid {controller}")
elif mode == "alive":
  pid, pid_start = sys.argv[3], sys.argv[4]
  print("alive" if process_instance(pid, pid_start) else "ended")
elif mode == "outcome":
  # The run store's fact history includes rotated segments, so read its archive files before the live event log.
  session_id, run_id = sys.argv[3], sys.argv[4]
  session = home / "sessions" / session_id
  run = json.loads((session / "data" / "runs" / run_id / "metadata.json").read_text(encoding="utf-8"))
  data = session / "data"
  archives = sorted((data / "archives").glob("chat_events.*.jsonl"))
  events = session / "data" / "chat_events.jsonl"
  outcome = None
  outcome_recorded = False

  def read_outcome(line, number, path):
    try:
      event = json.loads(line)
    except json.JSONDecodeError:
      # The live log's own reader skips a line it cannot parse, so such a line is not a fact; the skip is logged.
      print(f"skipped line {number} of {path}: not JSON", file=sys.stderr)
      return False, None
    if event.get("type") == "run_finished" and event.get("run_id") == run_id:
      return True, event.get("outcome")
    return False, None

  for archive in archives:
    with archive.open(encoding="utf-8") as stream:
      for number, line in enumerate(stream, 1):
        found, recorded = read_outcome(line, number, archive)
        if found:
          outcome_recorded = True
          outcome = recorded
  if events.is_file():
    with events.open(encoding="utf-8") as stream:
      line_number = 0
      remainder = ""
      for line in stream:
        if not line.endswith("\n"):
          remainder = line
          break
        line_number += 1
        found, recorded = read_outcome(line, line_number, events)
        if found:
          outcome_recorded = True
          outcome = recorded
      if run.get("kind") == "manager_turn" and not outcome_recorded:
        # Startup queues manager-turn recovery in the background, so give its finish callback time to persist the fact.
        deadline = time.monotonic() + 10
        while not outcome_recorded and time.monotonic() < deadline:
          line = stream.readline()
          if not line:
            time.sleep(0.1)
            continue
          if not line.endswith("\n"):
            remainder += line
            continue
          line_number += 1
          found, recorded = read_outcome(remainder + line, line_number, events)
          remainder = ""
          if found:
            outcome_recorded = True
            outcome = recorded
  print(outcome or "")
else:
  raise SystemExit(f"unknown home_helper mode {mode}")
PY
}

# scan_home writes the live runs to $RUNS_FILE and leaves the refusals in BLOCKERS. A live line is the session, the run,
# its backend, its pid and that pid's start time.
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

# impl_of_pid PID: rust <sha> or unknown, from the argv of the process. A release binary is named by its
# real path under the release root, so the current link resolves to the sha directory.
impl_of_pid() {
  local arg real
  while IFS= read -r -d '' arg; do
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

# listener_impl: what serves the port: rust <sha>, unknown, several, or none.
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

# api METHOD PATH [JSON] [TIMEOUT_S]: the body lands in $WORK/body, the HTTP status in API_STATUS. The key reaches
# curl through a file descriptor, so it stays out of argv.
api() {
  local timeout_s=15 with_body=0
  if [ "$#" -ge 3 ]; then with_body=1; fi
  if [ "$#" -ge 4 ]; then timeout_s=$4; fi
  if [ "$#" -ge 4 ] && [ -z "$3" ]; then with_body=0; fi
  if [ "$with_body" = 1 ]; then
    API_STATUS="$(curl -s -m "$timeout_s" -X "$1" -H @<(printf 'Authorization: Bearer %s\n' "$KEY") \
      -H 'Content-Type: application/json' --data-binary "$3" -o "$WORK/body" -w '%{http_code}' \
      "http://127.0.0.1:$PORT$2")"
  else
    API_STATUS="$(curl -s -m "$timeout_s" -X "$1" -H @<(printf 'Authorization: Bearer %s\n' "$KEY") \
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

# newest_run_field FIELD: a field of the newest Run in the runs response in $WORK/body (read with order=desc&limit=1);
# empty while the session has no run.
newest_run_field() {
  python3 -I - "$WORK/body" "$1" <<'PY'
import json
import sys

items = json.load(open(sys.argv[1])).get("items") or []
print(items[0][sys.argv[2]] if items else "")
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

# launch_has EVENT [KEY=VALUE...]: this launch's output has an EVENT line whose fields include each KEY=VALUE. It reads
# the Rust server's JSON-lines log; a line of another event never matches.
launch_has() {
  new_output > "$WORK/launch.log"
  python3 -I - "$WORK/launch.log" "$@" <<'PY'
import json
import pathlib
import sys

path, event, *pairs = sys.argv[1:]
wanted = dict(pair.split("=", 1) for pair in pairs)
# Only complete lines: a line the server is still writing has no newline yet.
for line in pathlib.Path(path).read_text(encoding="utf-8", errors="replace").split("\n")[:-1]:
  if not line.startswith('{"'):
    continue
  record = {key: str(value) for key, value in json.loads(line).items()}
  if record.get("event") == event and all(record.get(key) == value for key, value in wanted.items()):
    sys.exit(0)
sys.exit(1)
PY
}

new_launch_ready() {
  NEW_LOG="$(readlink "$LOG_DIR/server-latest.log" 2> /dev/null || true)"
  [ -n "$NEW_LOG" ] || return 1
  if [ "$NEW_LOG" = "$OLD_LOG" ]; then NEW_OFFSET=$OLD_SIZE; else NEW_OFFSET=0; fi
  launch_has server_ready
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

# run_process_ended PID PID_START: the process instance the run recorded has ended, so its pid may be free or belong to
# another process now.
run_process_ended() {
  [ "$(home_helper alive "$1" "$2")" = ended ]
}

# adopted: every live run recorded before the stop is accounted for by this launch's recovery. A run whose process still
# runs must have a task_recovery_resume line. A run whose process has ended needs its recorded outcome: a run's process
# survives a server stop by design, so an ended run died during the switch unless it completed on its own. A death is
# logged and counted in DIED_RUNS without failing the readback: no restore can adopt an ended run, so switch_forward
# keeps the new server and exits 4.
adopted() {
  local run_id pid pid_start session_id outcome
  while IFS=$'\t' read -r _ session_id run_id _ pid pid_start; do
    if launch_has task_recovery_resume "run_id=$run_id"; then
      continue
    fi
    if run_process_ended "$pid" "$pid_start"; then
      outcome="$(home_helper outcome "$session_id" "$run_id")" || {
        log "readback: cannot read the outcome of run $run_id"
        return 1
      }
      if [ "$outcome" = success ]; then
        log "readback: run $run_id ended during the switch: outcome success"
      else
        log "readback: run $run_id died during the switch: outcome ${outcome:-none recorded}"
        DIED_RUNS=$((DIED_RUNS + 1))
      fi
      continue
    fi
    log "readback: run $run_id was not adopted"
    return 1
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

# probe_clock: the wall clock in microseconds. The probe's waits use it, because SECONDS is too coarse for the
# one-second waits the tests use.
probe_clock() { echo "${EPOCHREALTIME/[.,]/}"; }

# probe_api END METHOD PATH BODY: makes an archive-step request with the time left before END as its curl timeout.
probe_api() {
  local end=$1 method=$2 path=$3 body=$4 remaining_us timeout_s
  remaining_us=$((end - $(probe_clock)))
  if [ "$remaining_us" -le 0 ]; then
    API_STATUS=000
    return 0
  fi
  [ "$remaining_us" -le 15000000 ] || remaining_us=15000000
  printf -v timeout_s '%d.%06d' "$((remaining_us / 1000000))" "$((remaining_us % 1000000))"
  api "$method" "$path" "$body" "$timeout_s"
}

# probe_sleep END: sleeps for one poll interval, shortened to the time left before END.
probe_sleep() {
  local end=$1 remaining_us sleep_s
  remaining_us=$((end - $(probe_clock)))
  [ "$remaining_us" -gt 0 ] || return 0
  if [ "$remaining_us" -gt 200000 ]; then
    sleep 0.2
  else
    printf -v sleep_s '%d.%06d' "$((remaining_us / 1000000))" "$((remaining_us % 1000000))"
    sleep "$sleep_s"
  fi
}

# probe_log STAGE: one line for a probe stage, with the session, the run ("-" until one exists) and the elapsed
# seconds.
probe_log() {
  log "probe: $1 session=$PROBE_SESSION run=${PROBE_RUN:--} elapsed=$((($(probe_clock) - PROBE_START) / 1000000))s"
}

# probe_read_run: the session's newest Run into PROBE_RUN and PROBE_STATE; both are empty while the session has no run.
probe_read_run() {
  api GET "/api/sessions/$PROBE_SESSION/runs?order=desc&limit=1"
  [ "$API_STATUS" = 200 ] || {
    log "probe: listing the runs answered $API_STATUS"
    return 1
  }
  PROBE_RUN="$(newest_run_field id)" || return 1
  PROBE_STATE="$(newest_run_field state)" || return 1
}

# run_ended STATE: the run has ended. A stopped run never launched, so it never will.
run_ended() {
  case "$1" in
    success | failed | interrupted | stopped) return 0 ;;
    *) return 1 ;;
  esac
}

# assistant_messages: how many assistant messages the bootstrap in $WORK/body holds, the open draft included. A tool
# call is part of its turn's assistant message, so a tool call counts here too.
assistant_messages() {
  python3 -I - "$WORK/body" <<'PY'
import json
import sys

body = json.load(open(sys.argv[1]))
drafts = [body["pending_draft"]] if body["pending_draft"] else []
print(sum(message["role"] == "assistant" for message in body["messages"] + drafts))
PY
}

# probe_wait_output: succeeds once the run records its first assistant output or ends success. It fails when the run
# ends without output, or when PROBE_WAIT_S passes first. The run is read before the messages, so output recorded
# before a failure still counts.
probe_wait_output() {
  local end=$(($(probe_clock) + PROBE_WAIT_S * 1000000)) outputs
  while :; do
    probe_read_run || return 1
    if [ "$PROBE_STATE" = success ]; then
      probe_log "run ended success"
      return 0
    fi
    api GET "/api/sessions/$PROBE_SESSION/bootstrap"
    [ "$API_STATUS" = 200 ] || {
      log "probe: the bootstrap answered $API_STATUS"
      return 1
    }
    outputs="$(assistant_messages)" || return 1
    if [ "$outputs" -gt 0 ]; then
      probe_log "first output"
      return 0
    fi
    if run_ended "$PROBE_STATE"; then
      probe_log "run ended $PROBE_STATE before its first output"
      return 1
    fi
    if [ "$(probe_clock)" -ge "$end" ]; then
      probe_log "no output within ${PROBE_WAIT_S}s"
      return 1
    fi
    sleep 0.2
  done
}

# probe_run_stages: sends the message and waits for the run's first output. probe_archive stops the run if it is open.
probe_run_stages() {
  api POST "/api/chat/$PROBE_SESSION/message" "{\"content\":\"Reply with the single word HELLO.\",\"request_id\":\"switch-server-probe:$PROBE_SESSION\"}"
  [ "$API_STATUS" = 202 ] || {
    log "probe: the message answered $API_STATUS"
    return 1
  }
  probe_wait_output
}

# run_states: one "RUN_ID STATE" line per run in the runs page in $WORK/body. The page asks for 500 runs, the largest
# page both servers serve; a next cursor means runs the stop would miss, so it is an error.
run_states() {
  python3 -I - "$WORK/body" <<'PY'
import json
import sys

page = json.load(open(sys.argv[1]))
if page["next_cursor"] is not None:
  raise SystemExit("probe: the session has more than 500 runs")
for run in page["items"]:
  print(run["id"], run["state"])
PY
}

# probe_cancel RUN END: asks RUN to stop. The request id names the run, so a repeated request is the same request.
probe_cancel() {
  PROBE_RUN=$1
  probe_api "$2" POST "/api/sessions/$PROBE_SESSION/runs/$PROBE_RUN/cancel" \
    "{\"request_id\":\"switch-probe-$PROBE_RUN\"}"
  [ "$API_STATUS" = 200 ] || {
    log "probe: the cancel answered $API_STATUS"
    return 1
  }
  probe_log "stop requested"
}

# probe_clear_runs END: stops each run of the probe session that has not ended, then waits until none is left or the
# clock passes END. A run that starts meanwhile is stopped the same way.
probe_clear_runs() {
  local end=$1 requested=" " runs run_id state stuck="" issued
  while :; do
    if [ "$(probe_clock)" -ge "$end" ]; then
      if [ -n "$stuck" ]; then
        PROBE_RUN=$stuck
        probe_log "run did not settle within ${PROBE_CANCEL_WAIT_S}s"
      else
        probe_log "archive did not finish within ${PROBE_CANCEL_WAIT_S}s"
      fi
      return 1
    fi
    probe_api "$end" GET "/api/sessions/$PROBE_SESSION/runs?limit=500" ""
    [ "$API_STATUS" = 200 ] || {
      log "probe: listing the runs answered $API_STATUS"
      return 1
    }
    runs="$(run_states)" || return 1
    if [ -z "$runs" ]; then
      if [ "$(probe_clock)" -ge "$end" ]; then
        probe_log "archive did not finish within ${PROBE_CANCEL_WAIT_S}s"
        return 1
      fi
      return 0
    fi
    stuck=""
    issued=0
    while read -r run_id state; do
      run_ended "$state" && continue
      [ -n "$stuck" ] || stuck=$run_id
      case "$requested" in
        *" $run_id "*) ;;
        *)
          requested+="$run_id "
          issued=1
          probe_cancel "$run_id" "$end" || return 1
          ;;
      esac
    done <<<"$runs"
    [ -n "$stuck" ] || return 0
    if [ "$(probe_clock)" -ge "$end" ]; then
      PROBE_RUN=$stuck
      probe_log "run did not settle within ${PROBE_CANCEL_WAIT_S}s"
      return 1
    fi
    # A pass that just requested a stop reads the runs again at once; a pass that only waits sleeps one poll interval.
    [ "$issued" = 1 ] || probe_sleep "$end"
  done
}

# probe_archive: archives the probe session. Each attempt first stops the session's open runs, because the server
# refuses an archive while a run is unfinished. A 409 means a run started after that stop, so the stop and the archive
# repeat until PROBE_CANCEL_WAIT_S passes.
probe_archive() {
  local end=$(($(probe_clock) + PROBE_CANCEL_WAIT_S * 1000000))
  while :; do
    probe_clear_runs "$end" || return 1
    probe_api "$end" POST "/api/sessions/$PROBE_SESSION/archive" \
      "{\"request_id\":\"switch-probe-archive:$PROBE_SESSION\"}"
    if [ "$API_STATUS" = 200 ]; then
      probe_log "archived"
      return 0
    fi
    probe_log "archive answered $API_STATUS"
    [ "$API_STATUS" = 409 ] || return 1
    [ "$(probe_clock)" -lt "$end" ] || return 1
    probe_sleep "$end"
  done
}

# probe_turn: runs the stages in a session of its own and archives that session whatever the stages found, so the live
# home keeps no open probe session.
probe_turn() {
  local verdict=0
  PROBE_START="$(probe_clock)"
  PROBE_SESSION=""
  PROBE_RUN=""
  PROBE_STATE=""
  api POST /api/sessions/ '{"name":"switch probe"}'
  [ "$API_STATUS" = 200 ] || {
    log "probe: creating a session answered $API_STATUS"
    return 1
  }
  PROBE_SESSION="$(json_field id)" || return 1
  probe_log "created"
  probe_run_stages || verdict=1
  probe_archive || verdict=1
  return "$verdict"
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
  RELEASE_DIR="$RELEASES_DIR/$TARGET_SHA"
  [ -d "$RELEASE_DIR" ] || fail "no release $TARGET_SHA in $RELEASES_DIR"
  (cd "$RELEASE_DIR" && sha256sum -c sha256sums.txt) || fail "$RELEASE_DIR does not match its sha256sums.txt"
  BASE="$(tr -d '[:space:]' < "$RELEASE_DIR/PORT_BASELINE")"
  [[ "$BASE" =~ ^[0-9a-f]{40}$ ]] || fail "PORT_BASELINE of $TARGET_SHA is not a full commit sha"
  OLD_SHA="$(git -C "$REPO_ROOT" rev-parse HEAD)"
  [ -z "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=no)" ] \
    || fail "runtime clone $REPO_ROOT has local changes"
  git -C "$REPO_ROOT" cat-file -e "$BASE^{commit}" 2> /dev/null \
    || fail "baseline $BASE is not in the runtime clone; fetch origin first"
  git -C "$REPO_ROOT" merge-base --is-ancestor "$OLD_SHA" "$BASE" \
    || fail "runtime clone HEAD $OLD_SHA is not an ancestor of baseline $BASE"
  CHARLIEBOT_HOME="$HOME_DIR" CHARLIEBOT_REPO_ROOT="$REPO_ROOT" CHARLIEBOT_VENV_BIN="$REPO_ROOT/.venv/bin" \
    "$RELEASE_DIR/charliebot-server" --check-config \
    || fail "--check-config of $TARGET_SHA rejects the home $HOME_DIR"
  scan_home || fail "cannot read the run records in $HOME_DIR"
  [ -z "$BLOCKERS" ] || fail "a restart would lose live work: ${BLOCKERS//$'\n'/; }"
  CONFIG="$(home_helper config)" || fail "cannot read $HOME_DIR/config.yaml and credentials.yaml"
  PORT="$(sed -n 1p <<<"$CONFIG")"
  KEY="$(sed -n 2p <<<"$CONFIG")"
  [ -n "$KEY" ] || fail "no charliebot.access_key in $HOME_DIR/credentials.yaml"
  "$REPO_ROOT/scripts/start-server.sh" --preflight || fail "launcher preflight failed"
  [ "$(listener_pids | grep -c .)" -eq 1 ] || fail "expected one server listening on port $PORT"
  LISTENER="$(listener_pids)"
  OLD_IMPL="$(impl_of_pid "$LISTENER")"
  [ "$OLD_IMPL" != unknown ] || fail "pid $LISTENER on port $PORT is not a release binary"
  if [ -z "$PANE" ]; then
    PANE="$(pane_of_pid "$LISTENER")" || fail "no tmux pane runs the server on port $PORT; pass --pane"
  fi
  PANE_PID="$(tm display-message -p -t "$PANE" '#{pane_pid}' 2> /dev/null)" || fail "no tmux pane $PANE"
  WANT="rust $TARGET_SHA"
  log "preflight passed: $OLD_IMPL -> $WANT; clone at $OLD_SHA; pane $PANE; port $PORT"
}

# server_running PID: PID runs a server, as impl_of_pid names it. A zombie has no argv, so it counts as stopped.
server_running() {
  local impl
  impl="$(impl_of_pid "$1")"
  [[ "$impl" == rust* ]]
}

# servers_gone PID...: succeeds once no PID runs a server.
servers_gone() {
  local pid
  for pid in "$@"; do
    server_running "$pid" && return 1
  done
  return 0
}

# alive_servers PID...: prints the PIDs that still run a server.
alive_servers() {
  local pid
  for pid in "$@"; do
    if server_running "$pid"; then echo "$pid"; fi
  done
}

# signal_servers SIGNAL PID...: sends SIGNAL to each PID that still runs a server, so a recycled PID is never signaled.
signal_servers() {
  local sig=$1 pid
  shift
  for pid in "$@"; do
    if server_running "$pid"; then kill "-$sig" "$pid" 2> /dev/null; fi
  done
}

# server_pids_under PID: the server processes under PID, one per line, parents before children.
server_pids_under() {
  local child
  for child in $(pgrep -P "$1"); do
    if server_running "$child"; then echo "$child"; fi
    server_pids_under "$child"
  done
}

# escalate_pane: the pane is busy after Ctrl-C. Each server process under the pane gets SIGTERM and STOP_WAIT_S to exit;
# each survivor gets SIGKILL and 10 s; the pane must then go idle within 10 s.
escalate_pane() {
  local -a pids survivors
  mapfile -t pids < <(server_pids_under "$PANE_PID")
  log "pane $PANE is busy after Ctrl-C; server pid(s) under it: ${pids[*]:-none}"
  log "SIGTERM to pid(s) ${pids[*]:-none}; ${STOP_WAIT_S}s budget"
  signal_servers TERM "${pids[@]}"
  if ! poll "$STOP_WAIT_S" servers_gone "${pids[@]}"; then
    mapfile -t survivors < <(alive_servers "${pids[@]}")
    log "pid(s) ${survivors[*]} survived SIGTERM; SIGKILL, 10s budget"
    signal_servers KILL "${survivors[@]}"
    poll 10 servers_gone "${survivors[@]}"
  fi
  wait_pane_idle 10 || return 1
  log "pane $PANE is idle"
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
    if ! wait_pane_idle "$CTRL_C_WAIT_S"; then
      escalate_pane || restore_failed "the pane's launcher did not exit after Ctrl-C, SIGTERM or SIGKILL"
    fi
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
  git -C "$REPO_ROOT" merge --ff-only -q "$BASE" || die "fast-forward to $BASE failed"
  (cd "$REPO_ROOT" && uv sync --frozen) || die "uv sync failed"
  set_current "$TARGET_SHA" || die "could not point current at $TARGET_SHA"
  start_in_pane || die "could not type the launcher into pane $PANE"
  wait_ready || die "the new server did not reach server_ready"
  serving "$WANT" || die "the new server failed its readback"
  adopted || die "a live run was not adopted"
  probe_turn || die "the probe turn failed"
  local head
  head="$(git -C "$REPO_ROOT" rev-parse --short HEAD)"
  if [ "$DIED_RUNS" -gt 0 ]; then
    log "SWITCH COMPLETE, BUT A RECORDED RUN DIED DURING THE SWITCH: $WANT serves port $PORT; clone at $head"
    exit 4
  fi
  log "SWITCH COMPLETE: $WANT serves port $PORT; clone at $head"
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

# tests/test_switch_server.py sources this file for the probe functions. A sourced copy stops here and runs no switch.
if [ "${BASH_SOURCE[0]}" != "$0" ]; then
  return 0
fi

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
