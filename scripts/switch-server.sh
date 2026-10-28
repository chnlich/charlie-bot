#!/usr/bin/env bash
# Switch the live CharlieBot server to an installed release and move the runtime clone to that release's commit.
#
#   scripts/switch-server.sh rust <full main commit> [--pane PANE]
#
# The preflight checks the release, the clone, the runs and the config, then asks start-server.sh to test its slices
# and scope. It refuses a clone with local changes and a target commit that a fetch does not bring. The switch then
# stops the old server, moves the clone with `git reset --keep <commit>` (forward or back), starts the release and
# reads it back. A refusal changes nothing and exits 1. The switch runs detached under setsid, because stopping the
# old server ends the master turn that started this command. The command prints the log path, waits for the verdict
# and exits with it: 0 when the readback, the probe turn and the delegation check passed; 2 when the switch failed
# and the previous implementation serves again; 3 when the restore failed too, and the log ends with the state
# the server is in; 4 when the switch completed but a recorded run died during it: the new server keeps serving, nothing
# is restored, and the log names the run and its outcome. When the pane stays busy after Ctrl-C, the restore sends
# SIGTERM, then SIGKILL, to the server processes under the pane.
#
# The probe turn runs after the readback, in a session of its own. It passes once the run records its first assistant
# output (a message or a tool call) or ends success. The archive then stops every run of the session that has not ended,
# and repeats the stop and the archive while the server answers 409: a follow-up run, such as the web reply nudge, can
# start after the turn. It reads run states and message roles only, so the model's text cannot fail a healthy server.
#
# The delegation check runs after the probe turn. The probe proves that the server answers. The check proves that the
# server creates a worktree on a repository, which the probe does not exercise. The check opens a session of its own
# with a user message, because the server delegates only from a task chain that holds one. Then it delegates one
# script-run task on the runtime clone, with base branch main. It waits up to CHARLIEBOT_SWITCH_DELEGATION_WAIT_S for
# the run to end. It passes when the run records success and the worktree the record names exists. It reads the run
# record and the worktree path only. Pass or fail, it stops the run, removes the worktree and its branch, and archives
# its session.
#
# Environment: CHARLIEBOT_HOME (default ~/.charliebot), CHARLIEBOT_RELEASES_DIR (default ~/charliebot-releases),
# CHARLIEBOT_LOG_DIR (default /tmp/charliebot-logs), CHARLIEBOT_TMUX_SOCKET (a tmux -L socket; unset means the default
# server, where the production pane runs). CHARLIEBOT_SWITCH_CTRL_C_WAIT_S, CHARLIEBOT_SWITCH_STOP_WAIT_S and
# CHARLIEBOT_SWITCH_READY_WAIT_S replace the 60 s, 60 s and 15 min waits. CHARLIEBOT_SWITCH_PROBE_WAIT_S and
# CHARLIEBOT_SWITCH_PROBE_CANCEL_WAIT_S replace the 300 s and 60 s probe waits.
# CHARLIEBOT_SWITCH_DELEGATION_WAIT_S replaces the 300 s delegation wait. The tests shorten the stop, ready, probe and
# delegation waits.
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
# The delegation check's wait for its run. A release rehearsal of 2026-10-27 on a clone of this repository passed the
# check in 14 s and 16 s; 300 s leaves room for a slower host.
DELEGATION_WAIT_S="${CHARLIEBOT_SWITCH_DELEGATION_WAIT_S:-300}"
usage() {
  echo "usage: switch-server.sh rust <full main commit> [--pane PANE]" >&2
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
# `scan` takes the running implementation (`rust <sha>`) and prints one line per live Run (live) and one per refusal
# (block); `alive` prints alive or ended for a recorded (pid, pid start); `outcome` reads the terminal fact so
# readback can distinguish a completed run from one the stop killed.
home_helper() {
  python3 -I - "$HOME_DIR" "$REPO_ROOT" "$@" "$TARGET_SHA" "$MODE" <<'PY'
import datetime
import json
import os
import pathlib
import re
import subprocess
import sys
import time

import yaml

home = pathlib.Path(sys.argv[1])
repo_root = pathlib.Path(sys.argv[2])
mode = sys.argv[3]
helper_args = sys.argv[4:-2]
target_sha = sys.argv[-2]
# Owner: ServerConfig.port in rust/crates/infra/core/src/config.rs.
DEFAULT_SERVER_PORT = 18498
# Each backend's registration has moved twice: rust/crates/charliebot-agents/src/backends/<backend>/mod.rs held it from
# 7b680fcbb until the crate grouping at 03459e0ca, rust/crates/runtime/agents/src/backends/<backend>/mod.rs from
# 03459e0ca until each backend's own crate at d1a07436f, and rust/crates/backends/<backend>/src/lib.rs from d1a07436f
# on. A build's tree says which layout it has, so each sha is read in its own.
BACKEND_LAYOUTS = (
    ("rust/crates/charliebot-agents/src/backends", "/mod.rs"),
    ("rust/crates/runtime/agents/src/backends", "/mod.rs"),
    ("rust/crates/backends", "/src/lib.rs"),
)
reattach_cache = {}


def backend_registration_files(sha):
  """The registration files of one build in the layout its tree has, and the error the tree read gave."""
  for prefix, suffix in BACKEND_LAYOUTS:
    listed = subprocess.run(
      ["git", "-C", str(repo_root), "ls-tree", "-r", "--name-only", sha, prefix],
      capture_output=True, text=True, check=False,
    )
    if listed.returncode:
      return None, f"cannot inspect build {sha}: {listed.stderr.strip()}"
    paths = [path for path in listed.stdout.splitlines() if path.endswith(suffix)]
    if paths:
      return paths, None
  return [], None


def rust_registration(sha, backend_type):
  """Read one build's backend registration without checking out or changing its source tree."""
  key = (sha, backend_type)
  if key in reattach_cache:
    return reattach_cache[key]
  paths, error = backend_registration_files(sha)
  if error is not None:
    result = (None, error)
    reattach_cache[key] = result
    return result
  for path in paths:
    shown = subprocess.run(
      ["git", "-C", str(repo_root), "show", f"{sha}:{path}"],
      capture_output=True, text=True, check=False,
    )
    if shown.returncode:
      result = (None, f"cannot read {path} from build {sha}: {shown.stderr.strip()}")
      reattach_cache[key] = result
      return result
    for registration in re.finditer(
      r"type_name:\s*(?P<name>\"[^\"]+\"|[A-Z_]+)\s*,\s*traits:\s*BackendTraits\s*\{(?P<body>.*?)\}",
      shown.stdout,
      re.DOTALL,
    ):
      name = registration.group("name")
      if name.startswith('"'):
        declared = name[1:-1]
      else:
        constant = re.search(rf"const\s+{re.escape(name)}\s*:\s*&str\s*=\s*\"([^\"]+)\"", shown.stdout)
        if constant is None:
          continue
        declared = constant.group(1)
      if declared != backend_type:
        continue
      enabled = re.search(r"^\s*restart_reattach:\s*(true|false)\s*,?\s*$", registration.group("body"), re.MULTILINE)
      if enabled is None:
        result = (None, f"{path} in build {sha} has no restart_reattach value")
      else:
        result = (enabled.group(1) == "true", path)
      reattach_cache[key] = result
      return result
  result = (None, f"build {sha} has no registration for backend type {backend_type}")
  reattach_cache[key] = result
  return result


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
  source_sha = helper_args[0].removeprefix("rust ")
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
      continue
    for build_label, sha in (("running build", source_sha), ("target build", target_sha)):
      enabled, detail = rust_registration(sha, backend_type)
      if enabled is not True:
        state = "false" if enabled is False else "unknown"
        print(f"block\t{backend} run {run['id']} (type {backend_type}, pid {run['pid']}) cannot be re-attached after a restart: "
              f"{build_label} {sha} marks restart_reattach = {state} ({detail})")
  # A loop's controller runs inside the server process; a restart never resumes it (rust/crates/runtime/tasks/src/recovery.rs).
  for path in sorted((home / "sessions").glob("*/loops/*/state.json")):
    state = json.loads(path.read_text(encoding="utf-8"))
    controller = state.get("server_pid")
    if state.get("status") == "running" and controller is not None and pathlib.Path(f"/proc/{controller}").exists():
      print(f"block\timprove loop {path.parent.name} is running under pid {controller}")
elif mode == "alive":
  pid, pid_start = helper_args
  print("alive" if process_instance(pid, pid_start) else "ended")
elif mode == "outcome":
  # The run store's fact history includes rotated segments, so read its archive files before the live event log.
  session_id, run_id = helper_args
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
# its backend, its pid and that pid's start time. It reads each live run's re-attachment declaration from the running
# build ($OLD_IMPL) and the target build, so the caller runs it only once both builds are known.
scan_home() {
  local scan
  scan="$(home_helper scan "$OLD_IMPL")" || return 1
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

# newest_run_fields FIELD...: the named fields of the newest Run in the runs response in $WORK/body (read with
# order=desc&limit=1), joined by the unit separator (0x1f). read splits on that separator without collapsing empty
# fields. A field is empty while the session has no run, and when the record holds it as null. One python start
# reads all the fields.
newest_run_fields() {
  python3 -I - "$WORK/body" "$@" <<'PY'
import json
import sys

items = json.load(open(sys.argv[1])).get("items") or []
values = [items[0][field] if items else "" for field in sys.argv[2:]]
print("\x1f".join("" if value is None else str(value) for value in values))
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

# check_clock: the wall clock in microseconds. The waits use it, because SECONDS is too coarse for the
# one-second waits the tests use.
check_clock() { echo "${EPOCHREALTIME/[.,]/}"; }

# check_api END METHOD PATH BODY: makes an archive-step request with the time left before END as its curl timeout.
check_api() {
  local end=$1 method=$2 path=$3 body=$4 remaining_us timeout_s
  remaining_us=$((end - $(check_clock)))
  if [ "$remaining_us" -le 0 ]; then
    API_STATUS=000
    return 0
  fi
  [ "$remaining_us" -le 15000000 ] || remaining_us=15000000
  printf -v timeout_s '%d.%06d' "$((remaining_us / 1000000))" "$((remaining_us % 1000000))"
  api "$method" "$path" "$body" "$timeout_s"
}

# check_sleep END: sleeps for one poll interval, shortened to the time left before END.
check_sleep() {
  local end=$1 remaining_us sleep_s
  remaining_us=$((end - $(check_clock)))
  [ "$remaining_us" -gt 0 ] || return 0
  if [ "$remaining_us" -gt 200000 ]; then
    sleep 0.2
  else
    printf -v sleep_s '%d.%06d' "$((remaining_us / 1000000))" "$((remaining_us % 1000000))"
    sleep "$sleep_s"
  fi
}

# check_log STAGE: one line for a stage of the check named by CHECK_LABEL, with the session, the run ("-" until one
# exists) and the elapsed seconds.
check_log() {
  log "$CHECK_LABEL: $1 session=$CHECK_SESSION run=${CHECK_RUN:--}" \
    "elapsed=$((($(check_clock) - CHECK_START) / 1000000))s"
}

# check_settle_failed STUCK: the report the archive step gives when its wait ends before the session settles. The run
# the step last waited for is named when one is known; with none, the archive itself is what did not finish.
check_settle_failed() {
  if [ -n "$1" ]; then
    CHECK_RUN=$1
    check_log "run did not settle within ${PROBE_CANCEL_WAIT_S}s"
  else
    CHECK_RUN=""
    check_log "archive did not finish within ${PROBE_CANCEL_WAIT_S}s"
  fi
}

# check_read_run: the session's newest Run into CHECK_RUN and CHECK_STATE; both are empty while the session has no run.
check_read_run() {
  api GET "/api/sessions/$CHECK_SESSION/runs?order=desc&limit=1"
  [ "$API_STATUS" = 200 ] || {
    log "$CHECK_LABEL: listing the runs answered $API_STATUS"
    return 1
  }
  local fields
  fields="$(newest_run_fields id state)" || return 1
  IFS=$'\x1f' read -r CHECK_RUN CHECK_STATE <<<"$fields"
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
  local end=$(($(check_clock) + PROBE_WAIT_S * 1000000)) outputs
  while :; do
    check_read_run || return 1
    if [ "$CHECK_STATE" = success ]; then
      check_log "run ended success"
      return 0
    fi
    api GET "/api/sessions/$CHECK_SESSION/bootstrap"
    [ "$API_STATUS" = 200 ] || {
      log "probe: the bootstrap answered $API_STATUS"
      return 1
    }
    outputs="$(assistant_messages)" || return 1
    if [ "$outputs" -gt 0 ]; then
      check_log "first output"
      return 0
    fi
    if run_ended "$CHECK_STATE"; then
      check_log "run ended $CHECK_STATE before its first output"
      return 1
    fi
    if [ "$(check_clock)" -ge "$end" ]; then
      check_log "no output within ${PROBE_WAIT_S}s"
      return 1
    fi
    sleep 0.2
  done
}

# probe_run_stages: sends the message and waits for the run's first output. check_archive stops the run if it is open.
probe_run_stages() {
  api POST "/api/chat/$CHECK_SESSION/message" "{\"content\":\"Reply with the single word HELLO.\",\"request_id\":\"switch-server-probe:$CHECK_SESSION\"}"
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
  raise SystemExit("the session has more than 500 runs")
for run in page["items"]:
  print(run["id"], run["state"])
PY
}

# check_cancel RUN END: asks RUN to stop. The request id names the run, so a repeated request is the same request. A
# wait that ends during the request reports the settle timeout; an answer the server gave before END reports its status.
check_cancel() {
  CHECK_RUN=$1
  check_api "$2" POST "/api/sessions/$CHECK_SESSION/runs/$CHECK_RUN/cancel" \
    "{\"request_id\":\"switch-stop-$CHECK_RUN\"}"
  if [ "$API_STATUS" != 200 ] && [ "$(check_clock)" -ge "$2" ]; then
    check_settle_failed "$CHECK_RUN"   # the wait ended during the stop request
    return 1
  fi
  [ "$API_STATUS" = 200 ] || {
    log "$CHECK_LABEL: the cancel answered $API_STATUS"
    return 1
  }
  check_log "stop requested"
}

# check_clear_runs END: stops each run of CHECK_SESSION that has not ended, then waits until none is left or the
# clock passes END. A run that starts meanwhile is stopped the same way. A request the wait ends during reports the
# settle timeout through check_settle_failed, as the clock check at the top of a pass does; an answer the server gave
# before END reports its status.
check_clear_runs() {
  local end=$1 requested=" " runs run_id state stuck="" issued
  while :; do
    if [ "$(check_clock)" -ge "$end" ]; then
      check_settle_failed "$stuck"
      return 1
    fi
    check_api "$end" GET "/api/sessions/$CHECK_SESSION/runs?limit=500" ""
    if [ "$API_STATUS" != 200 ] && [ "$(check_clock)" -ge "$end" ]; then
      check_settle_failed "$stuck"   # the wait ended during the listing; no answer came before END
      return 1
    fi
    [ "$API_STATUS" = 200 ] || {
      log "$CHECK_LABEL: listing the runs answered $API_STATUS"
      return 1
    }
    runs="$(run_states)" || return 1
    if [ -z "$runs" ]; then
      if [ "$(check_clock)" -ge "$end" ]; then
        check_settle_failed ""
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
          check_cancel "$run_id" "$end" || return 1
          ;;
      esac
    done <<<"$runs"
    [ -n "$stuck" ] || return 0
    if [ "$(check_clock)" -ge "$end" ]; then
      check_settle_failed "$stuck"
      return 1
    fi
    # A pass that just requested a stop reads the runs again at once; a pass that only waits sleeps one poll interval.
    [ "$issued" = 1 ] || check_sleep "$end"
  done
}

# check_archive: archives CHECK_SESSION. Each attempt first stops the session's open runs, because the server
# refuses an archive while a run is unfinished. A 409 means a run started after that stop, so the stop and the archive
# repeat until PROBE_CANCEL_WAIT_S passes. A request the wait ends during reports the settle timeout; an answer the
# server gave before END reports its status.
check_archive() {
  local end=$(($(check_clock) + PROBE_CANCEL_WAIT_S * 1000000))
  while :; do
    check_clear_runs "$end" || return 1
    check_api "$end" POST "/api/sessions/$CHECK_SESSION/archive" \
      "{\"request_id\":\"switch-archive:$CHECK_SESSION\"}"
    if [ "$API_STATUS" = 200 ]; then
      check_log "archived"
      return 0
    fi
    if [ "$(check_clock)" -ge "$end" ]; then
      check_settle_failed ""   # the wait ended during the archive request
      return 1
    fi
    check_log "archive answered $API_STATUS"
    [ "$API_STATUS" = 409 ] || return 1
    check_sleep "$end"
  done
}

# probe_turn: runs the stages in a session of its own and archives that session whatever the stages found, so the live
# home keeps no open probe session.
probe_turn() {
  local verdict=0
  CHECK_LABEL=probe
  CHECK_START="$(check_clock)"
  CHECK_SESSION=""
  CHECK_RUN=""
  CHECK_STATE=""
  api POST /api/sessions/ '{"name":"switch probe"}'
  [ "$API_STATUS" = 200 ] || {
    log "probe: creating a session answered $API_STATUS"
    return 1
  }
  CHECK_SESSION="$(json_field id)" || return 1
  check_log "created"
  probe_run_stages || verdict=1
  check_archive || verdict=1
  return "$verdict"
}

# The user message that opens the check's task chain. The server delegates only from a chain that holds a user message,
# and the message content holds no quote or backslash, so the request body needs no escaping. The model reads it, so
# each sentence states one instruction or the purpose of one.
DELEGATION_OPEN='Reply with the single word READY. This message opens the task chain that the check delegates from.'

# The goal of the check's task. The worker reads it, so each sentence states one instruction or the purpose of one.
DELEGATION_TASK='Run `git rev-parse --short HEAD` once in your worktree. This reads the checked-out commit and '\
'changes no file. Do not change or commit any file. The check needs the worktree as the server created it.'

# delegation_fail STEP MESSAGE: logs the step of the check that failed, with the server's words. The caller returns 1
# after it.
delegation_fail() {
  check_log "step $1 failed: $2"
}

# delegation_body: the delegate request that starts the task in CHECK_SESSION, as JSON. The base branch is main, and
# keep_worktree keeps the worktree after the run so the judgment can find it.
delegation_body() {
  python3 -I - "$CHECK_SESSION" "$REPO_ROOT" "$DELEGATION_TASK" <<'PY'
import json
import sys

session, repo, goal = sys.argv[1:]
print(json.dumps({
    "session_id": session,
    "description": goal,
    "repo_path": repo,
    "base_branch": "main",
    "task_type": "script-run",
    "keep_worktree": True,
    "request_id": f"switch-delegation:{session}",
}))
PY
}

# run_error: the message of the last error event in the check's run, read from the run's event log. Each line of that
# log is one JSON object. Empty when the run logged no error.
run_error() {
  python3 -I - "$HOME_DIR/sessions/$DELEG_CHILD/data/runs/$CHECK_RUN/events.jsonl" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
events = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()] if path.is_file() else []
errors = [event.get("message", "") for event in events if event.get("type") == "error"]
print(" ".join(errors[-1].split()) if errors else "")
PY
}

# delegation_open: creates the session that holds the task, then sends it the user message that opens the task chain.
# The archive closes the session.
delegation_open() {
  api POST /api/sessions/ '{"name":"switch delegation"}'
  [ "$API_STATUS" = 200 ] || {
    delegation_fail session "creating a session answered $API_STATUS"
    return 1
  }
  DELEG_PARENT="$(json_field id)" || return 1
  CHECK_SESSION=$DELEG_PARENT
  check_log "created"
  api POST "/api/chat/$CHECK_SESSION/message" \
    "{\"content\":\"$DELEGATION_OPEN\",\"request_id\":\"switch-open:$CHECK_SESSION\"}"
  [ "$API_STATUS" = 202 ] || {
    delegation_fail open "the message answered $API_STATUS: $(cat "$WORK/body")"
    return 1
  }
  check_log "opened the task chain"
}

# delegation_run: delegates the task, waits for its run to end within DELEGATION_WAIT_S, then judges the run. A run that
# ends failed reports the error text its event log holds.
delegation_run() {
  local end body error fields repo worktree
  end=$(($(check_clock) + DELEGATION_WAIT_S * 1000000))
  body="$(delegation_body)" || return 1
  check_api "$end" POST /api/internal/delegate "$body"
  if [ "$API_STATUS" != 200 ] && [ "$(check_clock)" -ge "$end" ]; then
    # The wait ended during the delegate request, so the answer that names the run never arrived.
    delegation_fail delegate "the run did not start within ${DELEGATION_WAIT_S}s"
    return 1
  fi
  [ "$API_STATUS" = 200 ] || {
    delegation_fail delegate "the server answered $API_STATUS: $(cat "$WORK/body")"
    return 1
  }
  DELEG_CHILD="$(json_field session_id)" || return 1
  CHECK_RUN="$(json_field run_id)" || return 1
  CHECK_SESSION=$DELEG_CHILD
  check_log "delegated the task to $REPO_ROOT"
  while :; do
    check_read_run || return 1
    run_ended "$CHECK_STATE" && break
    if [ "$(check_clock)" -ge "$end" ]; then
      delegation_fail run "run $CHECK_RUN did not end within ${DELEGATION_WAIT_S}s"
      return 1
    fi
    check_sleep "$end"
  done
  fields="$(newest_run_fields worktree_path repo_path)" || return 1
  IFS=$'\x1f' read -r worktree repo <<<"$fields"
  if [ "$CHECK_STATE" != success ]; then
    error="$(run_error)"
    delegation_fail run "run $CHECK_RUN ended $CHECK_STATE; the server's error: ${error:-none recorded}"
    return 1
  fi
  [ "$repo" = "$REPO_ROOT" ] || {
    delegation_fail record "run $CHECK_RUN names the repository $repo, want $REPO_ROOT"
    return 1
  }
  [ -n "$worktree" ] && [ -d "$worktree" ] || {
    delegation_fail worktree "run $CHECK_RUN ended success, but no worktree exists at '$worktree'"
    return 1
  }
  check_log "passed: run $CHECK_RUN ended success; worktree $worktree exists"
}

# delegation_remove_worktree: reads the run's record once the run has ended, then removes the worktree the record names
# and the branch it created. A path or a branch that is already gone is skipped.
delegation_remove_worktree() {
  local fields worktree branch
  check_read_run || return 1
  fields="$(newest_run_fields worktree_path branch_name)" || return 1
  IFS=$'\x1f' read -r worktree branch <<<"$fields"
  if [ -n "$worktree" ] && [ -e "$worktree" ]; then
    git -C "$REPO_ROOT" worktree remove --force "$worktree" || {
      check_log "step cleanup failed: git worktree remove $worktree"
      return 1
    }
  fi
  if [ -n "$branch" ] && git -C "$REPO_ROOT" show-ref --verify --quiet "refs/heads/$branch"; then
    git -C "$REPO_ROOT" branch -D "$branch" > /dev/null || {
      check_log "step cleanup failed: git branch -D $branch"
      return 1
    }
  fi
  check_log "removed the worktree and branch of the task"
}

# delegation_cleanup: stops the task's run, removes its worktree and branch, and archives the check's session. Each step
# runs whatever the verdict was. It returns 1 when a step leaves a run, a worktree, a branch or the session behind.
delegation_cleanup() {
  local verdict=0
  if [ -n "$DELEG_CHILD" ]; then
    CHECK_SESSION=$DELEG_CHILD
    check_clear_runs "$(($(check_clock) + PROBE_CANCEL_WAIT_S * 1000000))" && delegation_remove_worktree || verdict=1
  fi
  if [ -n "$DELEG_PARENT" ]; then
    CHECK_SESSION=$DELEG_PARENT
    CHECK_RUN=""
    check_archive || verdict=1
  fi
  return "$verdict"
}

# delegation_check: delegates one script-run task on the runtime clone, from a session of its own, and judges the
# task by its run record. The check passes when the run ended success, its record names the runtime clone, and the
# worktree it names exists. keep_worktree keeps that directory past the run, so its presence afterwards shows that it
# existed during the run. Pass or fail, the check then stops the run, removes the worktree and its branch, and archives
# its session.
delegation_check() {
  local verdict=0
  CHECK_LABEL=delegation
  CHECK_START="$(check_clock)"
  CHECK_SESSION=""
  CHECK_RUN=""
  CHECK_STATE=""
  DELEG_PARENT=""
  DELEG_CHILD=""
  delegation_open && delegation_run || verdict=1
  delegation_cleanup || verdict=1
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
  OLD_SHA="$(git -C "$REPO_ROOT" rev-parse HEAD)"
  [ -z "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=no)" ] \
    || fail "runtime clone $REPO_ROOT has local changes"
  git -C "$REPO_ROOT" fetch --quiet origin || fail "cannot fetch origin in $REPO_ROOT"
  git -C "$REPO_ROOT" cat-file -e "$TARGET_SHA^{commit}" 2> /dev/null \
    || fail "target commit $TARGET_SHA is not in the runtime clone after fetching origin"
  CHARLIEBOT_HOME="$HOME_DIR" CHARLIEBOT_REPO_ROOT="$REPO_ROOT" CHARLIEBOT_VENV_BIN="$REPO_ROOT/.venv/bin" \
    "$RELEASE_DIR/charliebot-server" --check-config \
    || fail "--check-config of $TARGET_SHA rejects the home $HOME_DIR"
  CONFIG="$(home_helper config)" || fail "cannot read $HOME_DIR/config.yaml and credentials.yaml"
  PORT="$(sed -n 1p <<<"$CONFIG")"
  KEY="$(sed -n 2p <<<"$CONFIG")"
  [ "$(listener_pids | grep -c .)" -eq 1 ] || fail "expected one server listening on port $PORT"
  LISTENER="$(listener_pids)"
  OLD_IMPL="$(impl_of_pid "$LISTENER")"
  [ "$OLD_IMPL" != unknown ] || fail "pid $LISTENER on port $PORT is not a release binary"
  scan_home || fail "cannot read the run records in $HOME_DIR"
  [ -z "$BLOCKERS" ] || fail "a restart would lose live work: ${BLOCKERS//$'\n'/; }"
  [ -n "$KEY" ] || fail "no charliebot.access_key in $HOME_DIR/credentials.yaml"
  "$REPO_ROOT/scripts/start-server.sh" --preflight || fail "launcher preflight failed"
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
  git -C "$REPO_ROOT" reset --keep -q "$TARGET_SHA" || die "git reset --keep $TARGET_SHA failed"
  (cd "$REPO_ROOT" && uv sync --frozen) || die "uv sync failed"
  set_current "$TARGET_SHA" || die "could not point current at $TARGET_SHA"
  start_in_pane || die "could not type the launcher into pane $PANE"
  wait_ready || die "the new server did not reach server_ready"
  serving "$WANT" || die "the new server failed its readback"
  adopted || die "a live run was not adopted"
  probe_turn || die "the probe turn failed"
  delegation_check || die "the delegation check failed"
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
