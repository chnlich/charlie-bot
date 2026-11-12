#!/usr/bin/env bash
# Switch the live CharlieBot server to an installed release and move the runtime clone to that release's commit.
#
#   scripts/switch-server.sh rust <full main commit> [--pane PANE]
#
# The preflight checks the release, clone, runs and config, confirms the user manager answers, then asks
# start-server.sh to test its slices and scope, and checks that every switched-on session-weight switch
# (server.enable_session_cpu_weight, server.enable_session_io_weight, read through the release's `charliebot
# switch-helper`) has its controller in the agents slice's cgroup.controllers. The check runs before the install
# below, so the target's units must already be installed: scripts/release.sh installs them ahead of its install
# stage. The no-model preflight checks finish while
# the old server still serves. It refuses a clone with local changes
# and a target commit that a fetch does not bring. Its scan refuses a restart while an improve loop's controller still
# runs in the server: the loop's active lock is present and the server pid its state names is live, whatever status the
# state file holds — `charliebot improve stop` writes `stopped` while the current iteration keeps running. Its last
# step runs each release's --check-config on an export of
# the commit that release starts on: the target's own commit, and the old release on OLD_SHA, the clone's commit, since
# the clone holds the old commit until the move. The switch then stops the old server, moves the clone with
# `git reset --keep <commit>` (forward or back), installs the target commit's slice units with
# scripts/install-slice-units.sh and reloads the user manager, starts the release, reads it back, probes a turn and
# checks a delegation. A refusal changes nothing and exits 1 with the refusing release's text, except that a missing
# helper binary exits 2 (below). The switch
# runs detached under setsid, because stopping the old server ends the master turn that started this command. The
# command prints the log path, waits for the verdict and exits with it: 0 when the readback, the probe turn and the
# delegation check passed; 2 when a check failed on the server's side and the switch switched back, so the previous
# implementation serves again; 3 when the switch back failed too, and the log ends with the state the server is in, and
# with the old release's refusal text when that is the cause; 4 when the switch completed but a recorded run died
# during it: the new server keeps serving, nothing is switched back, and the log names the run and its outcome; 5 when
# the switch back found that the old release refuses the home while the new release serves: the new release keeps
# serving, and the log names the failed step and the refusal text; 6 when the probe turn or the delegation check failed
# on the model's side after one retry: the new release keeps serving and the log names the check and the model's error.
#
# The checks that need a model run after the readback, on the new server and the production home, so the release path
# ends at server_ready: the probe turn and the delegation check. Each wait is bounded from the measured times (see
# PROBE_WAIT_S and DELEGATION_WAIT_S below). A check that fails because the model service or the gateway failed —
# their error text names a 5xx, a rate limit or an overloaded service — is a model-side failure: it is retried once,
# and a second such failure keeps the new release serving and exits 6, which leaves the decision to the release's
# operator. A check that fails any other way is a server-side failure, and the switch switches back. A check that hangs
# fails the same way, on its wait's bound.
#
# The switch back runs the forward path's own steps with the previous release as the target: it stops the new release,
# moves the clone and `current` back, installs the previous release's slice units, starts it, and reads back every run
# it recorded before the forward switch. It skips the disk-read gate, which guards a cold start's reads and is the one
# preflight step a switch back does not need. Before it stops the new release it runs the previous release's
# --check-config on the home as it stands; when the previous release refuses the home, the new release keeps serving and
# the switch exits 5.
#
# Every read of the home, of a run record and of an API answer goes through
# `charliebot switch-helper` (rust/crates/app/switch-helper). The binary is chosen once, when the switch starts: the
# target release's `charliebot` when it carries the reads the script calls, otherwise the `charliebot` of the release
# `current` names at that moment. The fallback lets a switch back to a release older than this script work: the
# release that serves the switch supplies the binary. When neither release has the reads, the preflight exits 2 before
# it changes anything.
#
# The probe turn runs after the readback, in a session of its own. It passes once the run records its first assistant
# output (a message or a tool call) or ends success. The archive then stops every run of the session that has not ended,
# and repeats the stop and the archive while the server answers 409: a follow-up run, such as the web reply nudge, can
# start after the turn. It reads run states and message roles only, so the model's text cannot fail a healthy server.
#
# The delegation check runs after the probe turn. The probe proves that the server answers. The check proves that the
# server creates a worktree on a repository, which the probe does not exercise. The check opens a session of its own
# with a user message, because the server delegates only from a task chain that holds one. Then it delegates one
# script-run task on the runtime clone, with base branch main. It waits up to CHARLIEBOT_SWITCH_DELEGATION_WAIT_S for a
# pass. The run's event log gives the first tool result: the check passes as soon as that result is a Bash command that
# did not error and ran in a worktree of the runtime clone, without waiting for the run to end. It also passes when the
# run ends success and the worktree the record names exists. A run that ends failed fails the check. When the wait ends
# first, a backend whose event log holds no init cwd or no tool result fails it, and the log names the backend. Pass or
# fail, it stops the run, removes the worktree and its branch, and archives its session.
#
# Environment: CHARLIEBOT_REPO_ROOT is the runtime clone this switch moves; unset means the script's parent
# directory, which only holds when the script sits inside the clone. A copy outside a checkout refuses an unset
# variable before any step. CHARLIEBOT_HOME (default ~/.charliebot),
# CHARLIEBOT_RELEASES_DIR (default ~/charliebot-releases),
# CHARLIEBOT_LOG_DIR (default /tmp/charliebot-logs), CHARLIEBOT_TMUX_SOCKET (a tmux -L socket; unset means the default
# server, where the production pane runs). CHARLIEBOT_SWITCH_CTRL_C_WAIT_S, CHARLIEBOT_SWITCH_STOP_WAIT_S and
# CHARLIEBOT_SWITCH_READY_WAIT_S replace the 60 s, 60 s and 15 min waits. CHARLIEBOT_SWITCH_PROBE_WAIT_S and
# CHARLIEBOT_SWITCH_PROBE_CANCEL_WAIT_S replace the 30 s and 60 s probe waits.
# CHARLIEBOT_SWITCH_DELEGATION_WAIT_S replaces the 80 s delegation wait. The tests shorten the stop, ready, probe and
# delegation waits. CHARLIEBOT_SWITCH_DISK_QUIET_MB, CHARLIEBOT_SWITCH_DISK_WAIT_S and
# CHARLIEBOT_SWITCH_DISK_SAMPLE_S replace the disk preflight's 40 MB/s threshold, 15 min bound and 10 s sample window;
# CHARLIEBOT_SWITCH_DISKSTATS, CHARLIEBOT_SWITCH_MOUNTINFO, CHARLIEBOT_SWITCH_CGROUP_ROOT and CHARLIEBOT_SWITCH_PROC
# replace the /proc and /sys/fs/cgroup paths the disk preflight reads, so its tests inject their own.
# CHARLIEBOT_SWITCH_LAB_MARKS=1 logs a lab_mark line at each stage boundary (the preflight, the disk gate, the stop and
# the start) for a lab that times the flow; a switch without the variable logs no such lab_mark line.
# CHARLIEBOT_SYSTEMD_USER_DIR replaces the user manager's unit directory the slice units install into.
#
# The host release lock: one lock file per host, outside the repository, named once by scripts/release/lock.sh. The
# lock covers the switch from its preflight to its exit, the switch-back included, so a release and a switch of one
# host run one at a time. A caller that finds the lock held exits 1 at once and prints the holder's record — script,
# commit, pid and start time — read from beside the lock; a holder that died left the lock free, because the lock is
# `flock` on an open descriptor.
#
# The disk preflight reads files only. It samples the root disk's read rate and waits while the rate stays above the
# threshold, up to the bound, logging why it waits. A quiet disk proceeds at once. On timeout the switch exits 1 with
# the old server untouched, and the log names the heaviest session cgroups and processes over the last sample with
# their bytes. Every switch also logs each charliebot-sess-* cgroup's read bytes before the old server stops and after
# the new server is ready. The threshold and bound are the 40 MB/s and 15 min the switch runbook has used live: the
# findings' 20 MB/s over 60 s sits inside the quiet tail of the 2026-10-29 incident (0.1-25 MB/s), where it would keep
# waiting after the reader ended, while 40 MB/s leaves at least 85 MB/s of the 125 MB/s volume for the boot's reads.
# A switch back skips the gate: the previous release starts from a page cache the new release's start has just filled,
# and the gate's bound would otherwise hold a switch back for up to its 15 minutes.
# A dry run of these readings, with no switch: source this file with any 40-hex commit and call `dry_run_disk`.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The clone this switch moves comes from the input, so a copy outside the clone runs it. Without the variable the
# script's parent directory is the clone, and that default holds only when the script sits inside a checkout. The
# checkout is the script's grandparent: a git directory, or the guide and the Rust workspace of an exported tree.
if [ -z "${CHARLIEBOT_REPO_ROOT:-}" ] && [ ! -e "$SCRIPT_DIR/../.git" ] \
  && { [ ! -f "$SCRIPT_DIR/../AGENTS.md" ] || [ ! -f "$SCRIPT_DIR/../rust/Cargo.toml" ]; }; then
  echo "switch-server.sh: CHARLIEBOT_REPO_ROOT is unset and the script is not inside a charlie-bot checkout:" \
    "$SCRIPT_DIR" >&2
  exit 1
fi
REPO_ROOT="${CHARLIEBOT_REPO_ROOT:-$SCRIPT_DIR/..}"
[ -d "$REPO_ROOT" ] || {
  echo "switch-server.sh: CHARLIEBOT_REPO_ROOT is not a directory: $REPO_ROOT" >&2
  exit 1
}
REPO_ROOT="$(cd "$REPO_ROOT" && pwd)"
export CHARLIEBOT_REPO_ROOT="$REPO_ROOT"
# The host release lock comes from scripts/release/lock.sh, the one definition: the detached run below takes the
# lock, and a sourced copy reaches none of it.
. "$SCRIPT_DIR/release/lock.sh"
# The slice-unit install this switch calls. switch_forward points this at a copy outside the checkout, because the
# clone moves to a switch-back target whose checkout may predate the owner.
SLICE_INSTALL="$SCRIPT_DIR/install-slice-units.sh"
SYSTEMD_USER="$SCRIPT_DIR/systemd-user.sh"
HOME_DIR="${CHARLIEBOT_HOME:-$HOME/.charliebot}"
RELEASES_DIR="${CHARLIEBOT_RELEASES_DIR:-$HOME/charliebot-releases}"
LOG_DIR="${CHARLIEBOT_LOG_DIR:-/tmp/charliebot-logs}"
CURRENT="$RELEASES_DIR/current"
CTRL_C_WAIT_S="${CHARLIEBOT_SWITCH_CTRL_C_WAIT_S:-60}"
STOP_WAIT_S="${CHARLIEBOT_SWITCH_STOP_WAIT_S:-60}"
READY_WAIT_S="${CHARLIEBOT_SWITCH_READY_WAIT_S:-900}"
# The probe turn's wait for its run's first output. It was 300 s, longer than any check the switch runs. The measured
# turns since 2026-10-30 took 8.7 s at the median and 10.0 s at p90 (findings.md §1); 30 s is three times the p90, so a
# slow model call passes and a hung turn fails the check.
PROBE_WAIT_S="${CHARLIEBOT_SWITCH_PROBE_WAIT_S:-30}"
PROBE_CANCEL_WAIT_S="${CHARLIEBOT_SWITCH_PROBE_CANCEL_WAIT_S:-60}"
# The delegation check's wait for its run. It was 300 s, which held a switch on a hung check for five minutes. The
# measured checks since 2026-10-30 took 9.3 s at the median, 13.4 s at p90 and 65.6 s at their longest (findings.md
# §1); 80 s leaves the longest about 14 s of room. With the probe's bound, a release whose checks both hang is switched
# back about 2 minutes after server_ready: 30 s + 80 s of waits and the switch back's stop, start and readback.
DELEGATION_WAIT_S="${CHARLIEBOT_SWITCH_DELEGATION_WAIT_S:-80}"
# The disk preflight. The threshold and bound are the values the switch runbook has used live; the sample window
# follows one boot's cold-read burst, about 36 s, so a sample responds within it.
DISK_QUIET_MB="${CHARLIEBOT_SWITCH_DISK_QUIET_MB:-40}"
DISK_WAIT_S="${CHARLIEBOT_SWITCH_DISK_WAIT_S:-900}"
DISK_SAMPLE_S="${CHARLIEBOT_SWITCH_DISK_SAMPLE_S:-10}"
DISK_REPORT_TOP="${CHARLIEBOT_SWITCH_DISK_REPORT_TOP:-10}"
DISKSTATS="${CHARLIEBOT_SWITCH_DISKSTATS:-/proc/diskstats}"
MOUNTINFO="${CHARLIEBOT_SWITCH_MOUNTINFO:-/proc/self/mountinfo}"
CGROUP_ROOT="${CHARLIEBOT_SWITCH_CGROUP_ROOT:-/sys/fs/cgroup}"
PROC_ROOT="${CHARLIEBOT_SWITCH_PROC:-/proc}"
ROOT_DISK_NAME=""
ROOT_PART_DEV=""
ROOT_DISK_DEV=""
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
DIED_RUNS=0   # no switch back can recover an ended run, so keep count until the switch exits 4
OLD_STOPPED=0 # the server that served the port before the switch has stopped
BACK=0        # the switch is running its switch-back path
while [ $# -gt 0 ]; do
  case "$1" in
    --pane) [ $# -ge 2 ] || usage; PANE=$2; shift 2 ;;
    --detached) [ $# -ge 2 ] || usage; DETACHED_LOG=$2; shift 2 ;;   # internal: the detached run's log
    *) usage ;;
  esac
done

log() { echo "$(date '+%Y-%m-%d %H:%M:%S.%3N') switch: $*"; }
fail() { log "preflight failed: $*"; exit 1; }

# lab_mark NAME: one timestamped stage boundary for the release lab, which sets CHARLIEBOT_SWITCH_LAB_MARKS to time the
# lab_mark points: the preflight, the disk gate, the stop and the start. A switch without the variable logs no lab_mark.
lab_mark() {
  [ "${CHARLIEBOT_SWITCH_LAB_MARKS:-0}" = 1 ] || return 0
  log "lab_mark=$1 at=$(date +%s%3N)"
}

RELEASE_DIR="$RELEASES_DIR/$TARGET_SHA"
# TARGET_RELEASE names the release directory of the target. A forward switch's target is its own commit; a switch back
# targets the directory `current` named before the switch, whose name need not be the commit it starts on.
TARGET_RELEASE="$TARGET_SHA"
HELPER_BIN=""   # the release's `charliebot`, chosen by helper_binary below

# supports_switch_helper BIN: the binary exists and carries the reads the script calls: the subcommand group, and
# `run-first-command`, the one read a release built between the group and this script lacks. The read is probed on a
# home path that does not exist: a binary that has it answers the metadata read failure with exit 1, and one that
# lacks it answers the unknown name with the usage exit 2.
supports_switch_helper() {
  [ -x "$1" ] || return 1
  "$1" switch-helper --help > /dev/null 2>&1 || return 1
  local rc
  "$1" switch-helper run-first-command "$1.probe-home" probe probe > /dev/null 2>&1
  rc=$?
  [ "$rc" -ne 2 ]
}

# helper_binary: choose the binary once, when the switch starts, and exit 2 when neither release has the reads.
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
  log "preflight failed: neither release $TARGET_SHA nor the release current names has the switch-helper reads this script calls"
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
}

# new_output: this launch's output so far.
new_output() { tail -c "+$((NEW_OFFSET + 1))" "$NEW_LOG"; }

# launch_has EVENT [KEY=VALUE...]: this launch's output has an EVENT line whose fields include each KEY=VALUE. It reads
# the server's log in either form `launch-has` reads; a line of another event never matches.
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
# logged and counted in DIED_RUNS without failing the readback: no switch back can adopt an ended run, so switch_forward
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

# check_note_model_error STATE: a check's run ended with STATE. When it failed and its last error names a model service
# failure, the check's failure is a model-side one: it records the error text in CHECK_ERROR and the class in
# CHECK_MODEL_ERROR, which run_check reads to retry the check once. Any other end leaves the failure to the server.
check_note_model_error() {
  [ "$1" = failed ] || return 0
  CHECK_ERROR="$(run_error "$CHECK_SESSION")"
  model_side_error "$CHECK_ERROR" && CHECK_MODEL_ERROR=1
  return 0
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
      check_note_model_error "$CHECK_STATE"
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

# run_error SESSION: the message of the last error event in the check's run, read from the run's event log. Each line of
# that log is one JSON object. Empty when the run logged no error.
run_error() {
  helper run-error "$HOME_DIR" "$1" "$CHECK_RUN"
}

# model_side_error TEXT: TEXT, a run's error message, names a failure of the model service rather than of the server.
# The measured false alarms since 2026-10-30 were the gateway's 5xx and the model service's errors (a BadGatewayError
# and Error code: 520, findings.md §2a), and the class the release decision keeps without a switch back is a gateway
# 5xx, a rate limit or a model-call error. A spawn failure, a cgroup failure and a git timeout name none of these, so
# they read as the server's.
model_side_error() {
  local text
  text="$(tr '[:upper:]' '[:lower:]' <<<"$1")"
  case "$text" in
    *badgatewayerror* | *"bad gateway"* | *internalservererror* | *"internal server error"* | \
      *ratelimiterror* | *"rate limit"* | *"too many requests"* | *"error code: 429"* | *"error code: 5"* | \
      *"service unavailable"* | *overloaded*)
      return 0
      ;;
  esac
  return 1
}

# delegation_read_first_command: reads the check's run first command from its event log into FIRST_CWD (the init cwd),
# FIRST_RESULT (command-ok, command-failed, not-command, or empty while the log holds no tool result) and RUN_BACKEND.
# A log or record that cannot be read fails the check.
delegation_read_first_command() {
  local fields
  fields="$(helper run-first-command "$HOME_DIR" "$DELEG_CHILD" "$DELEG_RUN")" || {
    delegation_fail first-command "cannot read the event log of run $DELEG_RUN"
    return 1
  }
  IFS=$'\x1f' read -r FIRST_CWD FIRST_RESULT RUN_BACKEND <<<"$fields"
}

# delegation_first_command_passed: the first tool result is a Bash command that did not error, and it ran in a worktree
# of the runtime clone. The clone's own directory is not a worktree the server made, so it does not pass.
delegation_first_command_passed() {
  [ "$FIRST_RESULT" = command-ok ] && [ -n "$FIRST_CWD" ] && [ "$FIRST_CWD" != "$REPO_ROOT" ] \
    && git -C "$REPO_ROOT" worktree list --porcelain | grep -Fxq "worktree $FIRST_CWD"
}

# delegation_timeout: the wait ended before the first command passed and before the run ended. A log that holds no init
# cwd or no tool result is named with its backend, since the check can read no first command from it. Otherwise the
# log names the first tool result and its cwd.
delegation_timeout() {
  if [ -z "$FIRST_CWD" ] || [ -z "$FIRST_RESULT" ]; then
    delegation_fail run "run $CHECK_RUN did not end within ${DELEGATION_WAIT_S}s; backend $RUN_BACKEND's event log "\
"holds no init cwd or no tool result, so no first command was read"
  else
    delegation_fail run "run $CHECK_RUN did not end within ${DELEGATION_WAIT_S}s; its first tool result was "\
"$FIRST_RESULT in $FIRST_CWD"
  fi
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
  DELEG_RUN=$CHECK_RUN
  CHECK_SESSION=$DELEG_CHILD
  check_log "delegated the task to $REPO_ROOT"
  while :; do
    check_read_run || return 1
    delegation_read_first_command || return 1
    if delegation_first_command_passed; then
      check_log "first command succeeded in $FIRST_CWD"
      return 0
    fi
    run_ended "$CHECK_STATE" && break
    if [ "$(check_clock)" -ge "$end" ]; then
      delegation_timeout
      return 1
    fi
    check_sleep "$end"
  done
  fields="$(newest_run_fields worktree_path repo_path)" || return 1
  IFS=$'\x1f' read -r worktree repo <<<"$fields"
  if [ "$CHECK_STATE" != success ]; then
    check_note_model_error "$CHECK_STATE"
    error="$(run_error "$CHECK_SESSION")"
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

# delegation_check: delegates one script-run task on the runtime clone, from a session of its own, and judges the task
# by its run's event log and record. The check passes when the first command succeeds in a worktree of the clone, or
# when the run ended success, its record names the runtime clone, and the worktree it names exists. keep_worktree keeps
# that directory past the run, so its presence afterwards shows that it existed during the run. Pass or fail, the check
# then stops the run, removes the worktree and its branch, and archives its session.
delegation_check() {
  local verdict=0
  CHECK_LABEL=delegation
  CHECK_START="$(check_clock)"
  CHECK_SESSION=""
  CHECK_RUN=""
  CHECK_STATE=""
  DELEG_PARENT=""
  DELEG_CHILD=""
  DELEG_RUN=""
  FIRST_CWD=""
  FIRST_RESULT=""
  RUN_BACKEND=""
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

# stop_serving: the step both directions start with. It stops the one server on the port, and a port with no server has
# nothing to stop: a new release that never bound leaves none, and the switch back still has to start the previous one.
# The pane must then be free, because the next launch types into it; a launcher that outlives its server gets Ctrl-C.
stop_serving() {
  local pids
  pids="$(listener_pids)"
  if [ -z "$pids" ]; then
    log "no server listens on port $PORT; nothing to stop"
    OLD_STOPPED=1
  else
    [ "$(grep -c . <<<"$pids")" -eq 1 ] || die "expected one server listening on port $PORT, found: $(tr '\n' ' ' <<<"$pids")"
    LISTENER=$pids
    OLD_IMPL="$(impl_of_pid "$LISTENER")"
    stop_old || die "the server on port $PORT did not stop"
  fi
  if pane_busy; then
    log "pane $PANE still runs a launcher; Ctrl-C, ${CTRL_C_WAIT_S}s budget"
    tm send-keys -t "$PANE" C-c
    wait_pane_idle "$CTRL_C_WAIT_S" || die "the pane's launcher did not exit"
  fi
}

# ---- disk preflight: a bounded wait for a quiet root disk, and per-session read records -----------------------------
#
# The 2026-10-29 boot took 225 s because the boot's cold reads queued behind a session's whole-home grep on the
# 125 MB/s root volume. These functions read /proc and the cgroup tree only; they change no cgroup setting.

# root_partition_dev: the major:minor of the filesystem mounted at /.
root_partition_dev() {
  awk '$5 == "/" { print $3; exit }' "$MOUNTINFO"
}

# diskstats_name DEV: the /proc/diskstats name of a major:minor device.
diskstats_name() {
  local name
  name="$(awk -v dev="$1" '$1 ":" $2 == dev { print $3; exit }' "$DISKSTATS")" || return 1
  [ -n "$name" ] || return 1
  printf '%s\n' "$name"
}

# diskstats_dev NAME: the major:minor of a /proc/diskstats name.
diskstats_dev() {
  local dev
  dev="$(awk -v name="$1" '$3 == name { print $1 ":" $2; exit }' "$DISKSTATS")" || return 1
  [ -n "$dev" ] || return 1
  printf '%s\n' "$dev"
}

# root_disk_name: the disk that carries /, read from /proc/diskstats. A partition gives its whole disk, because every
# partition shares the disk's read budget; a whole-disk mount gives itself.
root_disk_name() {
  if [ -n "$ROOT_DISK_NAME" ]; then
    printf '%s\n' "$ROOT_DISK_NAME"
    return 0
  fi
  local dev name candidate prefix rest best=""
  dev="$(root_partition_dev)" || return 1
  [ -n "$dev" ] || return 1
  name="$(diskstats_name "$dev")" || return 1
  while IFS= read -r candidate; do
    [ -n "$candidate" ] || continue
    # Linux names a partition "<disk>p<N>" when the disk name ends in a digit, and "<disk><N>" otherwise. Only one disk
    # matches a partition name under that rule, so the first match is the disk.
    case "$candidate" in
      *[0-9]) prefix="${candidate}p" ;;
      *) prefix=$candidate ;;
    esac
    rest="${name#"$prefix"}"
    if [ "$rest" != "$name" ] && [[ "$rest" =~ ^[0-9]+$ ]]; then
      best=$candidate
      break
    fi
  done < <(awk '{ print $3 }' "$DISKSTATS")
  ROOT_DISK_NAME="${best:-$name}"
  printf '%s\n' "$ROOT_DISK_NAME"
}

# root_devices: the major:minor identifiers of the device that carries /: its partition and its whole disk.
root_devices() {
  if [ -z "$ROOT_PART_DEV" ]; then
    ROOT_PART_DEV="$(root_partition_dev)"
    if ROOT_DISK_NAME="$(root_disk_name)"; then
      ROOT_DISK_DEV="$(diskstats_dev "$ROOT_DISK_NAME")" || ROOT_DISK_DEV=""
    else
      ROOT_DISK_DEV=""
    fi
  fi
  [ -n "$ROOT_PART_DEV" ] && printf '%s\n' "$ROOT_PART_DEV"
  if [ -n "$ROOT_DISK_DEV" ] && [ "$ROOT_DISK_DEV" != "$ROOT_PART_DEV" ]; then
    printf '%s\n' "$ROOT_DISK_DEV"
  fi
}

# root_read_sectors: the root disk's cumulative sectors-read counter from /proc/diskstats.
root_read_sectors() {
  local name
  name="$(root_disk_name)" || return 1
  awk -v name="$name" '$3 == name { print $6; found = 1; exit } END { exit !found }' "$DISKSTATS"
}

# disk_read_rate_mb: the root disk's read rate over DISK_SAMPLE_S seconds, in MB/s. The tests replace this function to
# inject the sampler, so they never read the live disk.
disk_read_rate_mb() {
  local before after window
  before="$(root_read_sectors)" || return 1
  window=$DISK_SAMPLE_S
  [ "$window" -gt 0 ] || window=1
  sleep "$DISK_SAMPLE_S"
  after="$(root_read_sectors)" || return 1
  printf '%s\n' $(( (after - before) * 512 / 1000000 / window ))
}

# session_cgroups: the charliebot-sess-* cgroup directories under CGROUP_ROOT, one per line.
session_cgroups() {
  [ -d "$CGROUP_ROOT" ] || return 1
  find "$CGROUP_ROOT" -type d -name 'charliebot-sess-*' 2> /dev/null | sort
}

# proc_read_bytes PID: the process's read_bytes from /proc/<pid>/io.
proc_read_bytes() {
  local file="$PROC_ROOT/$1/io"
  [ -r "$file" ] || return 1
  awk '$1 == "read_bytes:" { print $2; found = 1; exit } END { exit !found }' "$file" 2> /dev/null
}

# cgroup_proc_bytes DIR: the sum of read_bytes of the processes the cgroup holds now. It answers a cgroup whose
# io.stat is missing with a source that exists, so no row prints a zero for a file that was never read.
cgroup_proc_bytes() {
  local pid value total=0
  while IFS= read -r pid; do
    [ -n "$pid" ] || continue
    if value="$(proc_read_bytes "$pid")"; then
      total=$((total + value))
    fi
  done < "$1/cgroup.procs" 2> /dev/null
  printf '%s\n' "$total"
}

# session_rbytes_table: one line per charliebot-sess-* cgroup, "name<TAB>source<TAB>bytes<TAB>detail". The source is
# io.stat, whose rbytes for the root device is the cgroup's device read accounting. A cgroup without io.stat (the io
# controller is not enabled on it) gets the sum of its processes' /proc/<pid>/io read_bytes, and its detail names the
# missing io.stat path and the controllers the cgroup does expose. The function returns 1 when any io.stat was
# missing, so a caller never mistakes the proc sum for the cgroup's device accounting. This one function serves both
# the timeout error and the switch records.
session_rbytes_table() {
  local dir name file devices dirs bytes rc=0 controllers
  devices="$(root_devices | paste -sd' ' -)"
  if [ -z "$devices" ]; then
    printf 'unavailable\tunavailable\t-\tcannot resolve the root device from %s\n' "$MOUNTINFO"
    return 1
  fi
  dirs="$(session_cgroups)" || {
    printf 'unavailable\tunavailable\t-\tcannot list the session cgroups under %s\n' "$CGROUP_ROOT"
    return 1
  }
  while IFS= read -r dir; do
    [ -n "$dir" ] || continue
    name="${dir##*/}"
    file="$dir/io.stat"
    if [ ! -r "$file" ]; then
      if controllers="$(cat "$dir/cgroup.controllers" 2> /dev/null)"; then
        [ -n "$controllers" ] || controllers="(empty)"
      else
        controllers="unreadable"
      fi
      bytes="$(cgroup_proc_bytes "$dir")"
      printf '%s\tproc\t%s\tio.stat missing: %s; cgroup.controllers reads: %s\n' \
        "$name" "$bytes" "$file" "$controllers"
      rc=1
      continue
    fi
    bytes="$(awk -v devices=" $devices " '
      index(devices, " " $1 " ") && $2 ~ /^rbytes=/ { sub(/^rbytes=/, "", $2); total += $2 }
      END { printf "%d\n", total }
    ' "$file")"
    printf '%s\tio.stat\t%s\t\n' "$name" "$bytes"
  done <<< "$dirs"
  return "$rc"
}

# process_io_table: one line per process, "pid<TAB>comm<TAB>read_bytes". A process that ends while the table is built
# is skipped, which is expected churn rather than a read failure.
process_io_table() {
  local dir pid comm bytes
  for dir in "$PROC_ROOT"/[0-9]*; do
    [ -d "$dir" ] || continue
    pid="${dir##*/}"
    bytes="$(proc_read_bytes "$pid")" || continue
    comm="$(cat "$dir/comm" 2> /dev/null)" || comm="?"
    printf '%s\t%s\t%s\n' "$pid" "$comm" "$bytes"
  done
}

# read_deltas BEFORE AFTER: the positive AFTER-minus-BEFORE delta of the third column for each key in the first column,
# heaviest first, as "key<TAB>label<TAB>bytes<TAB>detail" (detail is whatever the row's fourth column holds). A key
# missing from BEFORE started inside the sample, so its whole count is its delta. A key missing from AFTER ended inside
# the sample, and is skipped. An empty snapshot gives no report: it may be a failed read, and a delta from it would
# count lifetime totals as the sample's.
read_deltas() {
  [ -s "$1" ] && [ -s "$2" ] || return 0
  awk -F'\t' '
    NR == FNR { before[$1] = $3; next }
    { delta = $3 - ($1 in before ? before[$1] : 0); if (delta > 0) printf "%s\t%s\t%d\t%s\n", $1, $2, delta, $4 }
  ' "$1" "$2" | sort -t $'\t' -k3,3nr
}

# report_heavy_readers DIR: the heaviest session cgroups and processes over the sample whose snapshots DIR holds. A
# cgroup row carries the source and detail of the accounting that answered, so a proc sum is never read as an io.stat
# rbytes and a missing io.stat shows its path.
report_heavy_readers() {
  local dir=$1 name source bytes detail pid comm
  read_deltas "$dir/cgroups.before" "$dir/cgroups.after" | head -n "$DISK_REPORT_TOP" > "$dir/cgroups.deltas"
  log "heaviest session cgroups over the last ${DISK_SAMPLE_S}s:"
  if [ -s "$dir/cgroups.deltas" ]; then
    while IFS=$'\t' read -r name source bytes detail; do
      if [ -n "$detail" ]; then
        log "  cgroup $name $bytes bytes ($source; $detail)"
      else
        log "  cgroup $name $bytes bytes ($source)"
      fi
    done < "$dir/cgroups.deltas"
  else
    log "  none"
  fi
  read_deltas "$dir/procs.before" "$dir/procs.after" | head -n "$DISK_REPORT_TOP" > "$dir/procs.deltas"
  log "heaviest processes over the last ${DISK_SAMPLE_S}s:"
  if [ -s "$dir/procs.deltas" ]; then
    while IFS=$'\t' read -r pid comm bytes; do
      log "  process $pid ($comm) $bytes bytes"
    done < "$dir/procs.deltas"
  else
    log "  none"
  fi
}

# wait_for_quiet_disk: samples the root disk's read rate and returns once it is at or below DISK_QUIET_MB, up to
# DISK_WAIT_S. It logs every sample, so the operator sees why a switch waits. The /proc and session-cgroup walks serve
# only the heavy-reader report, so they run on the timeout path alone: a gate that finds the disk quiet costs one disk
# sample and no walk. On timeout it takes one more sample window with the walks around it, reports the heaviest session
# cgroups and processes over that window, and returns 1; no server is touched here.
wait_for_quiet_disk() {
  local dir end rate
  dir="$(mktemp -d)" || {
    log "cannot make a work directory for the disk preflight"
    return 1
  }
  end=$((SECONDS + DISK_WAIT_S))
  while :; do
    rate="$(disk_read_rate_mb)"
    if [ "$?" -ne 0 ]; then
      log "cannot read the root disk's read rate from $DISKSTATS"
      rm -rf "$dir"
      return 1
    fi
    if [ "$rate" -le "$DISK_QUIET_MB" ]; then
      log "root disk read ${rate} MB/s over ${DISK_SAMPLE_S}s, at or below ${DISK_QUIET_MB} MB/s; proceeding"
      rm -rf "$dir"
      return 0
    fi
    log "waiting for the root disk: ${rate} MB/s read over ${DISK_SAMPLE_S}s, above ${DISK_QUIET_MB} MB/s;" \
      "up to ${DISK_WAIT_S}s"
    if [ "$SECONDS" -ge "$end" ]; then
      log "the root disk read above ${DISK_QUIET_MB} MB/s for ${DISK_WAIT_S}s;" \
        "one more sample window names the heaviest readers"
      session_rbytes_table > "$dir/cgroups.before" || true
      process_io_table > "$dir/procs.before" || true
      if ! disk_read_rate_mb > /dev/null; then
        log "cannot read the root disk's read rate from $DISKSTATS"
        rm -rf "$dir"
        return 1
      fi
      session_rbytes_table > "$dir/cgroups.after" || true
      process_io_table > "$dir/procs.after" || true
      report_heavy_readers "$dir"
      rm -rf "$dir"
      return 1
    fi
  done
}

# record_session_reads LABEL: logs each charliebot-sess-* cgroup's read bytes at one moment of the switch. A cgroup
# without io.stat is logged with the missing path and its source instead, and the final line says the accounting was
# degraded. The return status reports that degradation; a caller records it without failing the switch.
record_session_reads() {
  local label=$1 dir name source bytes detail rc=0
  dir="$(mktemp -d)" || {
    log "session reads $label: cannot make a work directory"
    return 1
  }
  session_rbytes_table > "$dir/table" || rc=1
  if [ -s "$dir/table" ]; then
    while IFS=$'\t' read -r name source bytes detail; do
      if [ -n "$detail" ]; then
        log "session reads $label: $name $bytes bytes from $source ($detail)"
      else
        log "session reads $label: $name $bytes bytes from $source"
      fi
    done < "$dir/table"
  else
    log "session reads $label: no charliebot-sess-* cgroups"
  fi
  [ "$rc" -eq 0 ] \
    || log "session reads $label:" \
      "io.stat is missing for at least one cgroup; each row names the path and the source read instead"
  rm -rf "$dir"
  return "$rc"
}

# dry_run_disk: the disk preflight's readings without a switch: the sampled root-disk read rate and the per-session
# rbytes table. It stops nothing and changes nothing.
dry_run_disk() {
  local rate rc
  rate="$(disk_read_rate_mb)" || {
    log "cannot read the root disk's read rate from $DISKSTATS"
    return 1
  }
  log "root disk read ${rate} MB/s over ${DISK_SAMPLE_S}s; threshold ${DISK_QUIET_MB} MB/s, bound ${DISK_WAIT_S}s"
  session_rbytes_table
  rc=$?
  [ "$rc" -eq 0 ] \
    || log "io.stat is missing for at least one cgroup; each row names the path and the source read instead"
  return "$rc"
}

# install_slice_units COMMIT: installs COMMIT's slice units through install-slice-units.sh, the one owner of the
# install. The launcher's preflight compares the running slice with the release's unit files, so the installed copy
# must be the new server's text before it starts. The copy run_detached keeps outside the checkout is the one called:
# after the clone moves to an older commit, that commit's checkout may hold no owner.
install_slice_units() {
  "$SLICE_INSTALL" "$REPO_ROOT" "$1"
}

# weight_controller_check: each switched-on session-weight switch names a controller that must have reached the
# agents slice by switch time, because the new server writes that controller's weight into every session cgroup and
# a slice without the controller fails every spawn. The switches are read through the release's `charliebot`
# switch-helper; a helper older than the switches serves no switch that writes weights, so its usage refusal skips
# the check. A missing controller stops the switch before the old server does anything.
weight_controller_check() {
  local slice_dir controllers switches rc
  slice_dir="$CGROUP_ROOT/user.slice/user-$(id -u).slice/user@$(id -u).service/charliebot.slice/charliebot-agents.slice"
  controllers="$(cat "$slice_dir/cgroup.controllers" 2> /dev/null)" \
    || fail "cannot read $slice_dir/cgroup.controllers; run scripts/setup.sh"
  switches="$(helper home-helper switches "$HOME_DIR")"
  rc=$?
  if [ "$rc" -ne 0 ]; then
    if [ "$rc" -eq 2 ]; then
      log "the release's charliebot reads no session-weight switches; the weight-controller check does not apply"
      return 0
    fi
    fail "cannot read the session-weight switches from $HOME_DIR"
  fi
  local controller switch
  for pair in "cpu enable_session_cpu_weight" "io enable_session_io_weight"; do
    set -- $pair
    controller=$1
    switch=$2
    case "$switches" in
      *"$controller=true"*) ;;
      *) continue ;;
    esac
    case " $controllers " in
      *" $controller "*) ;;
      *)
        if [ "$controller" = io ]; then
          fail "the io weight cannot reach the sessions: 'io' is missing from" \
            "$slice_dir/cgroup.controllers (have: $controllers). Give io to the user manager with" \
            "scripts/systemd/setup-io-share.sh, install the release's slice units with" \
            "scripts/install-slice-units.sh $REPO_ROOT $TARGET_SHA," \
            "or set server.$switch = false to turn the io weight off"
        else
          fail "the cpu weight cannot reach the sessions: 'cpu' is missing from" \
            "$slice_dir/cgroup.controllers (have: $controllers). Install the release's slice units with" \
            "scripts/install-slice-units.sh $REPO_ROOT $TARGET_SHA," \
            "or set server.$switch = false to turn the cpu weight off"
        fi
        ;;
    esac
  done
  log "weight controllers ready: controllers='$controllers' switches='$(tr '\n' ' ' <<<"$switches")'"
}

# config_check BINARY COMMIT: runs BINARY's --check-config on an export of COMMIT's files, with the home as
# CHARLIEBOT_HOME. It returns 0 when the release accepts the home. It returns 1 and prints the text on stdout when the
# release refuses the home, or when the export cannot be made or removed. Each release reads the files of the commit it
# starts on (web/templates, the agent gh wrapper), and the clone holds the old commit until switch_forward moves it.
# `git archive` writes nothing into the clone, so its HEAD, index, files and git metadata stay as they are. The check
# writes no config. The export is removed before the function returns, whether the check accepts or refuses. The
# callers decide what a refusal means: the preflight stops, and a switch back keeps a serving new release.
config_check() {
  local binary=$1 commit=$2 tree text rc
  tree="$(mktemp -d)" || {
    echo "cannot create a directory for the export of $commit"
    return 1
  }
  if ! git -C "$REPO_ROOT" archive --format=tar "$commit" | tar -x -C "$tree"; then
    rm -rf "$tree"
    echo "cannot export $commit from $REPO_ROOT"
    return 1
  fi
  text="$(CHARLIEBOT_HOME="$HOME_DIR" CHARLIEBOT_REPO_ROOT="$tree" "$binary" --check-config 2>&1)"
  rc=$?
  rm -rf "$tree" || {
    echo "cannot remove the export $tree"
    return 1
  }
  if [ "$rc" -ne 0 ]; then
    printf '%s\n' "$text"
    return 1
  fi
  return 0
}

preflight() {
  PREV_CURRENT="$(readlink "$CURRENT" 2> /dev/null || true)"
  [ -d "$RELEASE_DIR" ] || fail "no release $TARGET_SHA in $RELEASES_DIR"
  (cd "$RELEASE_DIR" && sha256sum -c sha256sums.txt) || fail "$RELEASE_DIR does not match its sha256sums.txt"
  helper_binary   # exits 2 when neither release carries the subcommand, before anything changes
  "$SYSTEMD_USER" --check || fail "the user manager did not answer"
  OLD_SHA="$(git -C "$REPO_ROOT" rev-parse HEAD)"
  [ -z "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=no)" ] \
    || fail "runtime clone $REPO_ROOT has local changes"
  git -C "$REPO_ROOT" fetch --quiet origin || fail "cannot fetch origin in $REPO_ROOT"
  git -C "$REPO_ROOT" cat-file -e "$TARGET_SHA^{commit}" 2> /dev/null \
    || fail "target commit $TARGET_SHA is not in the runtime clone after fetching origin"
  CONFIG="$(home_helper config)" || fail "cannot read $HOME_DIR/config.yaml and credentials.yaml"
  PORT="$(sed -n 1p <<<"$CONFIG")"
  KEY="$(sed -n 2p <<<"$CONFIG")"
  [ "$(listener_pids | grep -c .)" -eq 1 ] || fail "expected one server listening on port $PORT"
  LISTENER="$(listener_pids)"
  OLD_IMPL="$(impl_of_pid "$LISTENER")"
  PREV_IMPL=$OLD_IMPL   # the release a switch back starts, kept after the forward move renames OLD_IMPL
  [ "$OLD_IMPL" != unknown ] || fail "pid $LISTENER on port $PORT is not a release binary"
  scan_home || fail "cannot read the run records in $HOME_DIR"
  [ -z "$BLOCKERS" ] || fail "a restart would lose live work: ${BLOCKERS//$'\n'/; }"
  [ -n "$KEY" ] || fail "no charliebot.access_key in $HOME_DIR/credentials.yaml"
  "$REPO_ROOT/scripts/start-server.sh" --preflight || fail "launcher preflight failed"
  weight_controller_check
  if [ -z "$PANE" ]; then
    PANE="$(pane_of_pid "$LISTENER")" || fail "no tmux pane runs the server on port $PORT; pass --pane"
  fi
  PANE_PID="$(tm display-message -p -t "$PANE" '#{pane_pid}' 2> /dev/null)" || fail "no tmux pane $PANE"
  WANT="rust $TARGET_RELEASE"
  lab_mark gate_start
  wait_for_quiet_disk \
    || fail "the root disk read stayed above ${DISK_QUIET_MB} MB/s within ${DISK_WAIT_S}s;" \
      "the heaviest readers are the lines above"
  lab_mark gate_end
  # The last step: the target and the old release must both accept the home as it stands, because a switch back starts
  # the old release, and the config may have changed since the old server started.
  local refusal
  refusal="$(config_check "$RELEASE_DIR/charliebot-server" "$TARGET_SHA")" \
    || fail "--check-config of $TARGET_SHA rejects the home $HOME_DIR: ${refusal//$'\n'/; }"
  log "config check: rust $TARGET_SHA accepts $HOME_DIR"
  refusal="$(config_check "$RELEASES_DIR/$PREV_CURRENT/charliebot-server" "$OLD_SHA")" \
    || fail "$OLD_IMPL --check-config of $OLD_SHA rejects the home $HOME_DIR: ${refusal//$'\n'/; }"
  log "config check: $OLD_IMPL accepts $HOME_DIR at $OLD_SHA"
  log "preflight passed: $OLD_IMPL -> $WANT; clone at $OLD_SHA; pane $PANE; port $PORT"
}

# back_failed: the switch back failed: the state the server is in, then exit 3.
back_failed() {
  local head
  head="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2> /dev/null)"
  log "SWITCH BACK FAILED: $*"
  log "state: listener on port $PORT is $(listener_impl); pane $PANE busy: $(pane_busy && echo yes || echo no)"
  log "state: current -> $(readlink "$CURRENT" 2> /dev/null || echo none); clone HEAD $head"
  log "state: newest server log, last lines:"
  tail -n 12 "$(readlink "$LOG_DIR/server-latest.log" 2> /dev/null)" 2> /dev/null
  exit 3
}

# new_serving: the new release is the listener on the port and answers the version route. It does not poll: the switch
# back decides from the state the switch left.
new_serving() {
  listener_is "$WANT" && version_ok
}

# back_config_refused REFUSAL: the previous release refuses the home, so the switch back cannot start it. While the
# new release still serves, nothing is stopped and the switch exits 5. Otherwise the port has no server: the switch
# back exits 3.
back_config_refused() {
  log "switch back: $PREV_IMPL --check-config refuses the home: $1"
  if new_serving; then
    log "SWITCH CHECK FAILED, NEW RELEASE KEEPS SERVING: $FAILURE; $WANT serves port $PORT"
    exit 5
  fi
  back_failed "$PREV_IMPL --check-config refuses the home: $1"
}

# keep_new CHECK: the model side failed the check on both attempts. The new release keeps serving and the switch exits
# 6: only the release's operator can tell a bad release from a bad gateway, and a switch back on a false alarm costs a
# second restart of every live session.
keep_new() {
  log "SWITCH KEPT, MODEL CHECK FAILED: $1 failed on both attempts (${CHECK_ERROR:-no error text});" \
    "$WANT serves port $PORT; clone at $(git -C "$REPO_ROOT" rev-parse --short HEAD)"
  exit 6
}

# run_check CHECK: one post-ready check, retried once when its failure is a model-side error. It returns 0 on a pass,
# and 1 when the check failed on the server's side, which the caller answers with a switch back. A model-side failure
# gets one retry: the check runs again, and when the retry fails the same way the new release keeps serving (exit 6).
run_check() {
  local check=$1 attempt=1
  while :; do
    CHECK_MODEL_ERROR=0
    CHECK_ERROR=""
    if "$check"; then
      return 0
    fi
    if [ "$CHECK_MODEL_ERROR" != 1 ]; then
      log "$check: the failure is not a model-side error, so the switch switches back"
      return 1
    fi
    if [ "$attempt" -ge 2 ]; then
      keep_new "$check"
    fi
    log "$check: model-side error on attempt $attempt: ${CHECK_ERROR:-no error text}; retrying it once"
    attempt=$((attempt + 1))
  done
}

# post_ready_checks: the checks that need a model, after the readback and so after the release path's end. Each failure
# the server owns switches back here; a model-side failure that survives its retry keeps the new release.
post_ready_checks() {
  run_check probe_turn || switch_back "the probe turn failed"
  run_check delegation_check || switch_back "the delegation check failed"
}

# switch_move: the steps from a serving server to the target release at a ready server. Both directions run them: stop
# the serving server, move the clone and `current`, install the target commit's slice units, start it and wait for
# server_ready. The target's commit is TARGET_SHA and the release directory it starts from is TARGET_RELEASE.
switch_move() {
  lab_mark stop_start
  stop_serving
  lab_mark stop_end
  git -C "$REPO_ROOT" reset --keep -q "$TARGET_SHA" || die "git reset --keep $TARGET_SHA failed"
  set_current "$TARGET_RELEASE" || die "could not point current at $TARGET_RELEASE"
  install_slice_units "$TARGET_SHA" || die "the slice units of $TARGET_SHA did not install"
  lab_mark start_start
  start_in_pane || die "could not type the launcher into pane $PANE"
  wait_ready || die "the new server did not reach server_ready"
  lab_mark start_end
}

# die: a step after the switch began failed. The switch back is the answer, except inside a switch back, whose own
# failure the switch back reports.
die() {
  FAILURE="$*"
  log "step failed: $*"
  if [ "$BACK" = 1 ]; then
    back_failed "$*"
  fi
  switch_back "$*"
}

# switch_back STEP: the failure path. It runs the forward path's own steps to the release `current` named before the
# switch, at OLD_SHA, and skips the disk-read gate: the gate guards a start's cold reads, and the previous release's
# start follows the new one's with the page cache warm. Before the new release stops, the previous release must accept
# the home as it stands, which is the check a release that would break the switch back fails. The recorded runs are the
# forward scan's list, and the previous release must adopt every one of them, as the forward direction must.
switch_back() {
  local step=$* prev=$PREV_CURRENT prev_sha=$OLD_SHA refusal
  BACK=1
  FAILURE=$step
  log "switch back: $step"
  if [ "$OLD_STOPPED" = 0 ]; then
    # The new release never took the port, so the previous one still serves and nothing is switched.
    log "switch back: nothing changed; $OLD_IMPL still serves port $PORT"
    exit 2
  fi
  [ -n "$prev" ] || back_failed "no release current named before the switch, so there is no release to switch back to"
  refusal="$(config_check "$RELEASES_DIR/$prev/charliebot-server" "$prev_sha")" \
    || back_config_refused "${refusal//$'\n'/; }"
  log "switch back: $PREV_IMPL accepts $HOME_DIR at $prev_sha"
  TARGET_SHA=$prev_sha
  TARGET_RELEASE=$prev
  RELEASE_DIR="$RELEASES_DIR/$prev"
  WANT="rust $prev"
  switch_move
  record_session_reads "after the switch back is ready" || true
  serving "$WANT" || back_failed "the previous implementation failed its readback"
  adopted || back_failed "the previous implementation did not adopt the live runs"
  log "switched back: $WANT serves port $PORT; clone at $(git -C "$REPO_ROOT" rev-parse --short HEAD)"
  exit 2
}

switch_forward() {
  # The clone moves to each install's target first, and a switch-back target may predate the owner in its checkout, so
  # every install below runs from a copy outside the checkout.
  cp "$SLICE_INSTALL" "$WORK/install-slice-units.sh" || die "copy the slice-unit install to $WORK"
  cp "$SYSTEMD_USER" "$WORK/systemd-user.sh" || die "copy the user-manager wrapper to $WORK"
  SLICE_INSTALL="$WORK/install-slice-units.sh"
  scan_home || die "cannot read the run records in $HOME_DIR"
  [ -z "$BLOCKERS" ] || die "live work appeared after the preflight: ${BLOCKERS//$'\n'/; }"
  log "recorded $(wc -l < "$RUNS_FILE") live run(s) in $RUNS_FILE"
  record_session_reads "before the old server stops" || true
  switch_move
  record_session_reads "after the new server is ready" || true
  serving "$WANT" || die "the new server failed its readback"
  adopted || die "a live run was not adopted"
  post_ready_checks
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
  trap 'rm -rf "$WORK"; release_lock_release' EXIT
  release_lock_take switch-server.sh "$TARGET_SHA" || exit 1
  RELEASES_REAL="$(readlink -f "$RELEASES_DIR")"
  lab_mark switch_start
  preflight
  lab_mark preflight_end
  switch_forward
}

# The switch tests (rust/crates/app/script-tests/tests/switch_server/) source this file for the probe functions. A
# sourced copy stops here and runs no switch.
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
