#!/usr/bin/env bash
# Switch the live CharlieBot server to an installed release and move the runtime clone to that release's commit.
#
#   scripts/switch-server.sh rust <full main commit> [--pane PANE]
#
# The preflight checks the release, the clone, the runs and the config, then asks start-server.sh to test its slices
# and scope. It refuses a clone with local changes and a target commit that a fetch does not bring. The switch then
# stops the old server, moves the clone with `git reset --keep <commit>` (forward or back), starts the release and
# reads it back. A refusal changes nothing and exits 1, except that a missing helper binary exits 2 (below). The switch
# runs detached under setsid, because stopping the old server ends the master turn that started this command. The
# command prints the log path, waits for the verdict and exits with it: 0 when the readback, the probe turn and the
# delegation check passed; 2 when the switch failed and the previous implementation serves again; 3 when the restore
# failed too, and the log ends with the state the server is in; 4 when the switch completed but a recorded run died
# during it: the new server keeps serving, nothing is restored, and the log names the run and its outcome. When the
# pane stays busy after Ctrl-C, the restore sends SIGTERM, then SIGKILL, to the server processes under the pane.
#
# This script starts no python3: every read of the home, of a run record and of an API answer goes through
# `charliebot switch-helper` (rust/crates/app/switch-helper). The binary is chosen once, when the switch starts: the
# target release's `charliebot` when it has the subcommand, otherwise the `charliebot` of the release `current` names
# at that moment. The fallback lets a switch back to a release older than this script work: the release that serves
# the switch supplies the binary. When neither release has it, the preflight exits 2 before it changes anything.
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

RELEASE_DIR="$RELEASES_DIR/$TARGET_SHA"
HELPER_BIN=""   # the release's `charliebot`, chosen by helper_binary below

# supports_switch_helper BIN: the binary exists and parses the subcommand group. A release older than the group exits
# nonzero on it, which is the answer.
supports_switch_helper() {
  [ -x "$1" ] && "$1" switch-helper --help > /dev/null 2>&1
}

# helper_binary: choose the binary once, when the switch starts, and exit 2 when neither release has the subcommand.
helper_binary() {
  if [ -n "$HELPER_BIN" ]; then return 0; fi
  if supports_switch_helper "$RELEASE_DIR/charliebot"; then
    HELPER_BIN="$RELEASE_DIR/charliebot"
    return 0
  fi
  local current
  current="$(readlink "$CURRENT" 2> /dev/null || true)"
  if [ -n "$current" ] && supports_switch_helper "$RELEASES_DIR/$current/charliebot"; then
    HELPER_BIN="$RELEASES_DIR/$current/charliebot"
    return 0
  fi
  log "preflight failed: neither release $TARGET_SHA nor the release current names has charliebot switch-helper"
  exit 2
}

# helper NAME ARG...: one `charliebot switch-helper` subcommand of the chosen binary.
helper() {
  helper_binary || return 1
  "$HELPER_BIN" switch-helper "$@"
}

# The home is read, never written, by this helper. `config` prints the port and the access key (one per line);
# `scan` takes the running implementation (`rust <sha>`) and prints one line per live Run (live) and one per refusal
# (block); `alive` prints alive or ended for a recorded (pid, pid start); `outcome` reads the terminal fact so
# readback can distinguish a completed run from one the stop killed.
home_helper() {
  local mode=$1
  shift
  case "$mode" in
    config) helper home-helper config "$HOME_DIR" ;;
    scan) helper home-helper scan "$HOME_DIR" "$REPO_ROOT" "$1" "$TARGET_SHA" ;;
    alive) helper home-helper alive "$1" "$2" ;;
    outcome) helper home-helper outcome "$HOME_DIR" "$1" "$2" ;;
    *)
      log "unknown home helper mode $mode"
      return 1
      ;;
  esac
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
  helper json-field "$WORK/body" "$1"
}

# newest_run_fields FIELD...: the named fields of the newest Run in the runs response in $WORK/body (read with
# order=desc&limit=1), joined by the unit separator (0x1f). read splits on that separator without collapsing empty
# fields. A field is empty while the session has no run, and when the record holds it as null. One call reads all the
# fields.
newest_run_fields() {
  helper newest-run-fields "$WORK/body" "$@"
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
  helper launch-has "$WORK/launch.log" "$@"
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
  helper assistant-messages "$WORK/body"
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
  helper run-states "$WORK/body"
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
  helper delegation-body "$CHECK_SESSION" "$REPO_ROOT" "$DELEGATION_TASK"
}

# run_error: the message of the last error event in the check's run, read from the run's event log. Each line of that
# log is one JSON object. Empty when the run logged no error.
run_error() {
  helper run-error "$HOME_DIR" "$DELEG_CHILD" "$CHECK_RUN"
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
  [ -d "$RELEASE_DIR" ] || fail "no release $TARGET_SHA in $RELEASES_DIR"
  (cd "$RELEASE_DIR" && sha256sum -c sha256sums.txt) || fail "$RELEASE_DIR does not match its sha256sums.txt"
  helper_binary   # exits 2 when neither release carries the subcommand, before anything changes
  OLD_SHA="$(git -C "$REPO_ROOT" rev-parse HEAD)"
  [ -z "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=no)" ] \
    || fail "runtime clone $REPO_ROOT has local changes"
  git -C "$REPO_ROOT" fetch --quiet origin || fail "cannot fetch origin in $REPO_ROOT"
  git -C "$REPO_ROOT" cat-file -e "$TARGET_SHA^{commit}" 2> /dev/null \
    || fail "target commit $TARGET_SHA is not in the runtime clone after fetching origin"
  # A rollback target from before the venv removal requires CHARLIEBOT_VENV_BIN to name the runtime clone's existing
  # .venv/bin. Task final-delete removes this export with that .venv.
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
