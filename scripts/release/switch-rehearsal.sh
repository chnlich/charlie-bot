#!/usr/bin/env bash
# Rehearse a release switch on an isolated home, a private tmux server and a port in 18600-18699: the release that
# $CHARLIEBOT_RELEASES_DIR/current selects, then the new release, then the old release again, through a copy of the
# repository's scripts/switch-server.sh. After each switch the rehearsal checkout's HEAD must be the target commit.
#
#   scripts/release/switch-rehearsal.sh <new commit> <new release dir>
#
# The old release is the one `$CHARLIEBOT_RELEASES_DIR/current` selects (default ~/charliebot-releases). <new release
# dir> holds the new release's charliebot-server and sha256sums.txt, as scripts/release.sh downloaded them.
#
# The rehearsal checkout is a clone whose origin is this repository, checked out at the old release's commit. The
# switch copy runs from .cutover-switch/ beside it, so the switch moves the rehearsal checkout and never this
# repository. Every server starts from the rehearsal home's own config, credentials and releases.
#
# Exit: 0 every step passed; 1 a step failed; 2 the preflight failed, nothing started.
# Evidence goes to $CHARLIEBOT_RELEASE_RUN_DIR/rehearsal-<UTC>/ (or under $CHARLIEBOT_RELEASE_LOG_DIR) and is kept
# after a failure.
set -uo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SOURCE_REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"
readonly SWITCH_SOURCE="$SOURCE_REPO/scripts/switch-server.sh"
readonly RELEASES_DIR="${CHARLIEBOT_RELEASES_DIR:-$HOME/charliebot-releases}"
readonly LOG_ROOT="${CHARLIEBOT_RELEASE_LOG_DIR:-$HOME/charliebot-release-logs}"
readonly LIVE_HOME="${CHARLIEBOT_HOME:-$HOME/.charliebot}"
readonly CGROUP_BASE="/sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/charliebot.slice/charliebot-agents.slice"
readonly PORT_MIN=18600
readonly PORT_MAX=18699
readonly READY_WAIT_S=300
readonly STOP_WAIT_S=60
readonly ORIGINAL_PATH=$PATH
readonly STAMP="$(date -u +%Y%m%dT%H%M%SZ)"

TREE=
SOCK=
PORT_PID=
LOGS=
CB_HOME=
SESSIONS_DIR=
HELPER=
EVIDENCE=
REPORT=

usage() {
  echo "usage: switch-rehearsal.sh <new commit> <new release dir>" >&2
  exit 2
}

preflight_fail() {
  echo "PREFLIGHT FAILED: $*" >&2
  exit 2
}

log() { echo "$(date -u +%H:%M:%S) $*" | tee -a "$REPORT" >&2; }

[ $# -eq 2 ] || usage
NEW_COMMIT=${1,,}
NEW_RELEASE=$2
[[ $NEW_COMMIT =~ ^[0-9a-f]{40}$ ]] || usage
[ -d "$NEW_RELEASE" ] || preflight_fail "the new release directory exists: $NEW_RELEASE"

# The evidence directory holds the stage report and the logs; a preflight refusal leaves it with only its report.
EVIDENCE="$LOG_ROOT/rehearsal-$STAMP-${NEW_COMMIT:0:8}"
mkdir -p -m 700 "$EVIDENCE" || preflight_fail "cannot create the evidence directory $EVIDENCE"
REPORT="$EVIDENCE/report.txt"
readonly EVIDENCE REPORT

[ -e "$RELEASES_DIR/current" ] || preflight_fail "no current release under $RELEASES_DIR"
OLD_COMMIT="$(basename "$(readlink -f "$RELEASES_DIR/current")")"
[[ $OLD_COMMIT =~ ^[0-9a-f]{40}$ ]] || preflight_fail "current does not select a release named by a full commit sha"
[ "$OLD_COMMIT" != "$NEW_COMMIT" ] || preflight_fail "the new commit is the release that current already selects"
OLD_RELEASE="$RELEASES_DIR/$OLD_COMMIT"
[ -d "$OLD_RELEASE" ] || preflight_fail "the old release directory exists: $OLD_RELEASE"
(cd "$OLD_RELEASE" && sha256sum -c sha256sums.txt) > "$EVIDENCE/old-release-checksums.txt" 2>&1 \
  || preflight_fail "the old release does not match its sha256sums.txt (see old-release-checksums.txt)"
(cd "$NEW_RELEASE" && sha256sum -c sha256sums.txt) > "$EVIDENCE/new-release-checksums.txt" 2>&1 \
  || preflight_fail "the new release does not match its sha256sums.txt (see new-release-checksums.txt)"

for tool in git tmux ss uv curl setsid flock sha256sum; do
  command -v "$tool" > /dev/null || preflight_fail "$tool is on PATH"
done
[ -x "$SWITCH_SOURCE" ] || preflight_fail "the switch script exists: $SWITCH_SOURCE"

# The rehearsal's backend entry comes from the live config: the first option whose api_base and credential are set,
# so the rehearsal runs the operator's own model endpoint and the repository names no host and no credential.
LIVE_BACKEND="$(python3 -I - "$LIVE_HOME/config.yaml" <<'PY'
import sys

import yaml

with open(sys.argv[1], encoding="utf-8") as handle:
    options = (yaml.safe_load(handle).get("backends") or {}).get("options") or []
for option in options:
    if option.get("api_base") and option.get("credential"):
        print(option["api_base"])
        print(option["credential"])
        break
else:
    raise SystemExit(1)
PY
)" || preflight_fail "the live config has a backends.options entry with api_base and credential"
BACKEND_API_BASE="$(sed -n 1p <<<"$LIVE_BACKEND")"
BACKEND_CREDENTIAL="$(sed -n 2p <<<"$LIVE_BACKEND")"
BACKEND_KEY="$(python3 -I - "$LIVE_HOME/credentials.yaml" "$BACKEND_CREDENTIAL" <<'PY'
import sys

import yaml

with open(sys.argv[1], encoding="utf-8") as handle:
    print(yaml.safe_load(handle)[sys.argv[2]]["api_key"])
PY
)" || preflight_fail "the live credentials file holds api_key for $BACKEND_CREDENTIAL"
readonly BACKEND_API_BASE BACKEND_CREDENTIAL

PORT="$(python3 -I - "$PORT_MIN" "$PORT_MAX" <<'PY'
import socket
import sys

start, end = (int(value) for value in sys.argv[1:])
for port in range(start, end + 1):
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        sock.bind(("127.0.0.1", port))
    except OSError:
        sock.close()
        continue
    sock.close()
    print(port)
    break
else:
    raise SystemExit(1)
PY
)" || preflight_fail "a free 127.0.0.1 port exists in $PORT_MIN-$PORT_MAX"

TREE="/tmp/charliebot-release-rehearsal-${NEW_COMMIT:0:12}-$$"
[ ! -e "$TREE" ] || preflight_fail "$TREE already exists"
mkdir -m 700 "$TREE" || preflight_fail "create the rehearsal tree $TREE"
readonly TRIAL_HOME="$TREE/home"
readonly CB_HOME="$TRIAL_HOME/.charliebot"
readonly REPO="$TREE/repo"
readonly RELEASES="$TREE/releases"
readonly LOGS="$TREE/logs"
readonly HTTP="$TREE/http"
readonly WORKSPACE="$TREE/workspace"
readonly TMUX_ROOT="$TREE/tmux"
readonly TMP_ROOT="$TREE/tmp"
readonly AUTH_CONFIG="$TREE/auth.curl"
readonly SESSIONS_DIR="$CB_HOME/sessions"
readonly HELPER="$TREE/helper.py"
readonly SWITCH_DIR="$REPO/.cutover-switch"
readonly SWITCH_SCRIPT="$SWITCH_DIR/switch-server.sh"

cat > "$HELPER" <<'PY'
import json
import os
import pathlib
import re
import signal
import subprocess
import sys
import time

CONSOLE = re.compile(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}(?:\.\d{3})? \[\w+\s*\] (\S+)\s*(.*)")


def complete_lines(path):
    file_path = pathlib.Path(path)
    if not file_path.is_file():
        return []
    return file_path.read_text(encoding="utf-8", errors="replace").split("\n")[:-1]


def event_name(line):
    text = line.strip()
    if text.startswith("{"):
        return json.loads(text).get("event")
    match = CONSOLE.match(text)
    return match[1] if match else None


def cmd_event(path, event):
    return 0 if any(event_name(line) == event for line in complete_lines(path)) else 1


def cmd_version_sha(path):
    value = json.loads(pathlib.Path(path).read_text(encoding="utf-8")).get("sha")
    if not isinstance(value, str) or not value:
        raise SystemExit("the version response has no sha")
    print(value)
    return 0


def environ_has(pid, needle):
    for name in ("environ", "cmdline"):
        try:
            data = pathlib.Path(f"/proc/{pid}/{name}").read_bytes()
        except OSError:
            continue
        if needle.encode() in data:
            return True
    return False


def cmd_listener_pid(port, tree):
    """The one pid listening on the port whose environment or argv names the tree; signals nothing else."""
    output = subprocess.run(["ss", "-Hltnp", f"sport = :{port}"], capture_output=True, text=True, check=True).stdout
    pids = sorted({int(pid) for pid in re.findall(r"pid=(\d+)", output)})
    owned = [pid for pid in pids if environ_has(pid, tree)]
    if len(owned) == 1:
        print(owned[0])
    return 0


def cmd_leftovers(tree, self_pid):
    """Processes whose environment or argv names the tree, except this helper, its parent and the rehearsal shell."""
    skip = {os.getpid(), os.getppid(), int(self_pid)}
    found = []
    for entry in pathlib.Path("/proc").iterdir():
        if not entry.name.isdigit() or int(entry.name) in skip:
            continue
        if environ_has(entry.name, tree):
            try:
                command = pathlib.Path(f"/proc/{entry.name}/comm").read_text(encoding="utf-8").strip()
            except OSError:
                command = "?"
            found.append(f"{entry.name} {command}")
    for row in found:
        print(row)
    return 1 if found else 0


def cmd_cgroup_clean(path, tree):
    """Removes one rehearsal session cgroup; processes in it are signalled only when they name the tree."""
    directory = pathlib.Path(path)
    if not directory.is_dir():
        return 0
    procs_file = directory / "cgroup.procs"
    for pid in procs_file.read_text(encoding="utf-8").split():
        if environ_has(pid, tree):
            try:
                os.kill(int(pid), signal.SIGKILL)
            except ProcessLookupError:
                pass
    deadline = time.monotonic() + 10
    while procs_file.read_text(encoding="utf-8").strip() and time.monotonic() < deadline:
        time.sleep(0.2)
    if procs_file.read_text(encoding="utf-8").strip():
        print(f"populated by processes outside the tree: {directory.name}")
        return 1
    directory.rmdir()
    return 0


COMMANDS = {
    "event": (cmd_event, 2, 2),
    "version-sha": (cmd_version_sha, 1, 1),
    "listener-pid": (cmd_listener_pid, 2, 2),
    "leftovers": (cmd_leftovers, 2, 2),
    "cgroup-clean": (cmd_cgroup_clean, 2, 2),
}

if __name__ == "__main__":
    name, args = sys.argv[1], sys.argv[2:]
    function, low, high = COMMANDS[name]
    if not low <= len(args) <= high:
        raise SystemExit(f"{name}: wrong number of arguments")
    raise SystemExit(function(*args))
PY
chmod 600 "$HELPER"
helper() { python3 -I "$HELPER" "$@"; }

# stop_pid PID: SIGTERM with the stop budget, then SIGKILL.
stop_pid() {
  local pid=$1
  [ -n "$pid" ] || return 0
  kill -TERM "$pid" 2> /dev/null || return 0
  for _ in $(seq 1 $((STOP_WAIT_S * 10))); do
    kill -0 "$pid" 2> /dev/null || break
    sleep 0.1
  done
  kill -KILL "$pid" 2> /dev/null || true
  log "CLEANUP stopped the rehearsal server pid $pid"
}

cleanup() {
  local rc=$? pid dir id8 leftovers log_file evidence_copy_ok=1
  trap - EXIT INT TERM
  if [ -f "$HELPER" ]; then
    stop_pid "$PORT_PID"
    pid="$(helper listener-pid "$PORT" "$TREE")"
    [ "$pid" = "$PORT_PID" ] || stop_pid "$pid"
  fi
  [ -n "$SOCK" ] && tmux -L "$SOCK" kill-server 2> /dev/null
  if [ -f "$HELPER" ] && [ -d "$SESSIONS_DIR" ]; then
    for dir in "$SESSIONS_DIR"/*/; do
      [ -d "$dir" ] || continue
      id8="$(basename "$dir")"
      helper cgroup-clean "$CGROUP_BASE/charliebot-sess-${id8:0:8}" "$TREE" > /dev/null 2>&1 || rc=1
    done
  fi
  if [ -f "$HELPER" ] && [ -d "$TREE" ]; then
    if leftovers="$(helper leftovers "$TREE" "$$")"; then
      log "CLEANUP leftovers under the tree: none"
    else
      log "CLEANUP leftovers under the tree: $leftovers"
      rc=1
    fi
  fi
  if [ -d "$LOGS" ]; then
    if mkdir -p "$EVIDENCE/logs"; then
      shopt -s nullglob
      for log_file in "$LOGS"/switch_*.log "$LOGS"/switch_*.runs "$LOGS"/server_*.log "$LOGS"/server-latest.log; do
        if ! cp -p "$log_file" "$EVIDENCE/logs/"; then
          log "CLEANUP could not copy $log_file to $EVIDENCE/logs"
          evidence_copy_ok=0
          rc=1
        fi
      done
      shopt -u nullglob
    else
      log "CLEANUP could not create $EVIDENCE/logs"
      evidence_copy_ok=0
      rc=1
    fi
    if [ -d "$CB_HOME/sessions" ]; then
      mkdir -p "$EVIDENCE/rehearsal-home"
      (cd "$CB_HOME" && find sessions \( -path '*/data/runs/*/metadata.json' -o -name chat_events.jsonl \) -print0 \
        | xargs -0 -r cp --parents -t "$EVIDENCE/rehearsal-home") \
        || {
          log "CLEANUP run records of the rehearsal home could not be copied"
          evidence_copy_ok=0
          rc=1
        }
    fi
  fi
  if [[ $TREE == /tmp/charliebot-release-rehearsal-* ]]; then
    if [ "$evidence_copy_ok" = 1 ]; then
      rm -rf -- "$TREE" || rc=1
      log "CLEANUP tree $TREE removed: $([ -e "$TREE" ] && echo no || echo yes)"
    else
      log "CLEANUP retained tree $TREE because evidence could not be copied"
    fi
  fi
  log "EXIT $rc"
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

log "REHEARSAL old $OLD_COMMIT -> new $NEW_COMMIT -> old $OLD_COMMIT; port $PORT; evidence $EVIDENCE"

# The rehearsal tree: a clone of this repository at the old release's commit. The switch's own fetch reads this
# repository as origin, so the release commits are present without network access.
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL="$TREE/gitconfig"
printf '[user]\n  name = Switch rehearsal\n  email = rehearsal@example.invalid\n[init]\n  defaultBranch = main\n' \
  > "$GIT_CONFIG_GLOBAL"
mkdir -m 700 -p "$CB_HOME" "$REPO" "$RELEASES" "$LOGS" "$HTTP" "$WORKSPACE" "$TMUX_ROOT" "$TMP_ROOT" \
  || preflight_fail "create the rehearsal directories inside $TREE"
git init -q -b main "$REPO" || preflight_fail "initialize the rehearsal checkout"
git -C "$REPO" remote add origin "$SOURCE_REPO" || preflight_fail "set the rehearsal checkout's origin"
git -C "$REPO" fetch -q --no-tags origin "+refs/heads/main:refs/remotes/origin/main" \
  || preflight_fail "fetch main into the rehearsal checkout"
git -C "$REPO" fetch -q --no-tags origin "+refs/remotes/origin/main:refs/remotes/origin/main" || true
for commit in "$OLD_COMMIT" "$NEW_COMMIT"; do
  git -C "$REPO" cat-file -e "$commit^{commit}" 2> /dev/null \
    || preflight_fail "commit $commit is not in $SOURCE_REPO; fetch it first"
done
git -C "$REPO" checkout -q -B main "$OLD_COMMIT" || preflight_fail "check out the old release's commit"

# The switch under test runs from .cutover-switch/ beside the checkout, as a first live switch runs it before the
# checkout itself carries the new script.
mkdir -m 700 "$SWITCH_DIR" && cp "$SWITCH_SOURCE" "$SWITCH_SCRIPT" && chmod 700 "$SWITCH_SCRIPT" \
  || preflight_fail "copy $SWITCH_SOURCE to $SWITCH_SCRIPT"
log "SWITCH SCRIPT copied to $SWITCH_SCRIPT"

# The isolated releases: a copy of the old release and the new release, both verified, with current on the old one.
mkdir -m 700 "$RELEASES/$OLD_COMMIT" "$RELEASES/$NEW_COMMIT"
cp -a "$OLD_RELEASE/." "$RELEASES/$OLD_COMMIT/"
cp -a "$NEW_RELEASE/." "$RELEASES/$NEW_COMMIT/"
rm -f "$RELEASES/$OLD_COMMIT/PORT_BASELINE" "$RELEASES/$NEW_COMMIT/PORT_BASELINE"
(cd "$RELEASES/$OLD_COMMIT" && sha256sum -c sha256sums.txt) > "$EVIDENCE/isolated-old-checksums.txt" 2>&1 \
  || preflight_fail "the isolated old release does not match its sha256sums.txt"
(cd "$RELEASES/$NEW_COMMIT" && sha256sum -c sha256sums.txt) > "$EVIDENCE/isolated-new-checksums.txt" 2>&1 \
  || preflight_fail "the isolated new release does not match its sha256sums.txt"
ln -s "$OLD_COMMIT" "$RELEASES/current"

ACCESS_KEY="$(od -An -N32 -tx1 /dev/urandom | tr -d '[:space:]')"
printf '%s:\n  api_key: %s\ncharliebot:\n  access_key: %s\n' "$BACKEND_CREDENTIAL" "$BACKEND_KEY" "$ACCESS_KEY" \
  > "$CB_HOME/credentials.yaml"
chmod 600 "$CB_HOME/credentials.yaml"
printf 'header = "Authorization: Bearer %s"\n' "$ACCESS_KEY" > "$AUTH_CONFIG"
chmod 600 "$AUTH_CONFIG"
cat > "$CB_HOME/config.yaml" <<EOF
server:
  host: 127.0.0.1
  port: $PORT
  subprocess_buffer_limit_mb: 1024
  session_memory_max_mb: 1024
  session_swap_max_mb: 0
  session_tasks_max: 256
  min_free_disk_gib: 0
paths:
  workspace_dirs:
    - $WORKSPACE
  worktree_dir: $TREE/worktrees
backends:
  pool:
    - charlie-code-release-rehearsal
  options:
    - id: charlie-code-release-rehearsal
      label: Release rehearsal
      type: charlie-code
      model: openai/glm-5.3-flash
      api_base: $BACKEND_API_BASE
      credential: $BACKEND_CREDENTIAL
EOF
chmod 600 "$CB_HOME/config.yaml"

(cd "$REPO" && uv sync --locked) > "$EVIDENCE/uv-sync.log" 2>&1 || preflight_fail "uv sync --locked in the tree"

# The environment every switch and server reads: the tree's home, releases and logs, and a private tmux server.
export HOME="$TRIAL_HOME"
export TMPDIR="$TMP_ROOT" TMP="$TMP_ROOT" TEMP="$TMP_ROOT"
export TMUX_TMPDIR="$TMUX_ROOT"
export XDG_CACHE_HOME="$TREE/xdg-cache" XDG_CONFIG_HOME="$TREE/xdg-config" XDG_DATA_HOME="$TREE/xdg-data"
export UV_CACHE_DIR="$TREE/uv-cache"
export CHARLIEBOT_HOME="$CB_HOME"
export CHARLIEBOT_REPO_ROOT="$REPO"
export CHARLIEBOT_VENV_BIN="$REPO/.venv/bin"
export CHARLIEBOT_RELEASES_DIR="$RELEASES"
export CHARLIEBOT_LOG_DIR="$LOGS"
export CHARLIEBOT_SWITCH_READY_WAIT_S="$READY_WAIT_S"
export CHARLIEBOT_SWITCH_STOP_WAIT_S="$STOP_WAIT_S"
export PATH="$REPO/.venv/bin:$ORIGINAL_PATH"
unset CHARLIE_CODE_API_KEY AIGW_API_KEY OPENAI_API_KEY ANTHROPIC_API_KEY
SOCK="rehearsal-$$"
export CHARLIEBOT_TMUX_SOCKET="$SOCK"
tmux -L "$SOCK" new-session -d -s rehearsal -c "$REPO" -x 200 -y 50 || preflight_fail "start the private tmux server"
log "TMUX private socket $SOCK under $TMUX_ROOT"

http_get() {  # http_get PATH OUTPUT: prints the status
  local status
  status="$(curl --config "$AUTH_CONFIG" --silent --show-error --connect-timeout 2 --max-time 10 --output "$2" \
    --write-out '%{http_code}' "http://127.0.0.1:$PORT$1")" || status=000
  printf '%s' "$status"
}

fail_step() {
  log "FAILED: $*"
  exit 1
}

# start_old: types the launcher into the pane and waits for server_ready in the new launch's log.
start_old() {
  local before newlog end=$((SECONDS + READY_WAIT_S))
  before="$(readlink "$LOGS/server-latest.log" 2> /dev/null || true)"
  tmux -L "$SOCK" send-keys -t rehearsal \
    "cd $(printf %q "$REPO") && env CHARLIEBOT_HOME=$(printf %q "$CB_HOME") CHARLIEBOT_RELEASES_DIR=$(printf %q "$RELEASES") CHARLIEBOT_LOG_DIR=$(printf %q "$LOGS") ./scripts/start-server.sh" Enter
  while (( SECONDS < end )); do
    newlog="$(readlink "$LOGS/server-latest.log" 2> /dev/null || true)"
    if [ -n "$newlog" ] && [ "$newlog" != "$before" ] && helper event "$newlog" server_ready; then
      log "START old: server_ready in $newlog"
      return 0
    fi
    sleep 0.5
  done
  log "START old: no server_ready within ${READY_WAIT_S}s"
  return 1
}

# serving_impl: what owns the listener on the port: rust <sha>, none, several or unknown.
serving_impl() {
  local pids arg real
  pids="$(ss -Hltnp "sport = :$PORT" | grep -o 'pid=[0-9]*' | cut -d= -f2 | sort -u)"
  if [ -z "$pids" ]; then
    echo none
    return
  fi
  if [ "$(grep -c . <<<"$pids")" -ne 1 ]; then
    echo several
    return
  fi
  while IFS= read -r -d '' arg; do
    real="$(readlink -f "$arg" 2> /dev/null)"
    case "$real" in
      "$RELEASES"/*/charliebot-server)
        real="${real#"$RELEASES"/}"
        echo "rust ${real%/charliebot-server}"
        return
        ;;
    esac
  done 2> /dev/null < "/proc/$pids/cmdline"
  echo unknown
}

# expect_version LABEL COMMIT: the port answers /api/internal/version with a sha that prefixes COMMIT.
expect_version() {
  local label=$1 commit=$2 status sha
  status="$(http_get /api/internal/version "$HTTP/version-$label.json")"
  [ "$status" = 200 ] || fail_step "$label: GET /api/internal/version answered $status"
  sha="$(helper version-sha "$HTTP/version-$label.json")" || fail_step "$label: the version response has no sha"
  log "VERSION $label: HTTP $status sha $sha (want $commit)"
  [[ "$commit" == "$sha"* ]] || fail_step "$label: the version sha $sha is not a prefix of $commit"
}

# check_head LABEL COMMIT: the rehearsal checkout is at the release's commit.
check_head() {
  local label=$1 commit=$2 head
  head="$(git -C "$REPO" rev-parse HEAD)"
  log "HEAD $label: $head (want $commit)"
  [ "$head" = "$commit" ] || fail_step "$label: the checkout HEAD is $head, want $commit"
}

# run_switch LABEL ARGS...: runs the switch copy, records its exit status and log, and keeps the output as evidence.
run_switch() {
  local label=$1 rc start_ms end_ms
  shift
  start_ms=$(date +%s%3N)
  "$SWITCH_SCRIPT" "$@" > "$EVIDENCE/$label.out" 2>&1
  rc=$?
  end_ms=$(date +%s%3N)
  log "SWITCH $label: switch-server.sh $* exit $rc; wall $((end_ms - start_ms)) ms; log $(sed -n 's/^switch log: //p' "$EVIDENCE/$label.out" | head -1)"
  return "$rc"
}

start_old || fail_step "the old release did not reach server_ready"
check_head start "$OLD_COMMIT"
PORT_PID="$(helper listener-pid "$PORT" "$TREE")"
[ "$(serving_impl)" = "rust $OLD_COMMIT" ] || fail_step "the listener is $(serving_impl), want rust $OLD_COMMIT"
expect_version start "$OLD_COMMIT"
log "STEP start: $(serving_impl) serves $PORT"

run_switch step1-new rust "$NEW_COMMIT" || fail_step "step 1: the switch to $NEW_COMMIT exited $?"
check_head step1 "$NEW_COMMIT"
[ "$(readlink "$RELEASES/current")" = "$NEW_COMMIT" ] || fail_step "step 1: current is not $NEW_COMMIT"
[ "$(serving_impl)" = "rust $NEW_COMMIT" ] || fail_step "step 1: the listener is $(serving_impl), want rust $NEW_COMMIT"
expect_version step1 "$NEW_COMMIT"
PORT_PID="$(helper listener-pid "$PORT" "$TREE")"
log "STEP 1: new release serves $PORT; checkout HEAD $(git -C "$REPO" rev-parse --short HEAD)"

run_switch step2-old rust "$OLD_COMMIT" || fail_step "step 2: the switch back to $OLD_COMMIT exited $?"
check_head step2 "$OLD_COMMIT"
[ "$(readlink "$RELEASES/current")" = "$OLD_COMMIT" ] || fail_step "step 2: current is not $OLD_COMMIT"
[ "$(serving_impl)" = "rust $OLD_COMMIT" ] || fail_step "step 2: the listener is $(serving_impl), want rust $OLD_COMMIT"
expect_version step2 "$OLD_COMMIT"
PORT_PID="$(helper listener-pid "$PORT" "$TREE")"
log "STEP 2: old release serves $PORT; checkout HEAD $(git -C "$REPO" rev-parse --short HEAD)"

log "ALL STEPS PASSED"
exit 0
