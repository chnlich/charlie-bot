#!/usr/bin/env bash
# Serve one caller command against a server binary with its paired CLI in the same directory.
#
#   scripts/release/trial.sh --serve [--tree-root <dir>] [--seed-home <dir>] [--in-caller-scope] <commit> <server binary> -- <command> [args...]
#
# The commit is extracted from this repository into an isolated tree with its own home, private tmux server and a port
# in 18650-18669, and the server starts on it. The command then runs with CHARLIEBOT_SERVE_URL,
# CHARLIEBOT_SERVE_AUTH_HEADER (one line `Authorization: Bearer <key>`) and CHARLIEBOT_SERVE_PID in its environment.
# The server runs inside a systemd user scope with MemoryMax=2G, so a measurement cannot push the live server into
# swap. After the command ends the trial waits until every run of the trial home has ended, stops the server, asserts
# that no process of the trial tree remains, cleans up and runs sweep-check on the server log.
#
# This host gives an unprivileged process no read-only view of /sys/fs/cgroup, so the preflight refuses (exit 2,
# nothing started) a commit whose diff against its merge base with origin/main touches the cgroup code
# (rust/crates/runtime/agents/src/transport/cgroup.rs, rust/crates/infra/core/src/cgroup.rs or
# rust/crates/runtime/agents/src/agent_environment.rs): the serve server runs only binaries whose cgroup code equals
# main's. It runs with session cgroups off (session_memory_max_mb 0), starts no agent and installs nothing, so it needs
# no writable agents slice; its tmux socket must keep its path under 108 bytes, the sun_path size with the final NUL.
#
# The config carries the live config's weight switches, server.enable_session_cpu_weight and
# server.enable_session_io_weight, read by `release-helper backend-option`; an absent switch takes the server's
# default. CHARLIEBOT_TRIAL_CGROUP_ROOT replaces /sys/fs/cgroup, the root the agents slice path is built on, so the
# tests point it at a sandbox.
#
# Options: --tree-root <dir> builds the tree as <dir>/trial-<first 12 hex of the commit>, so runs of two commits under
# one root share no tree; --seed-home <dir> copies each session directory of <dir> into the trial home before the
# server starts, with the boot marker <dir>/boot_recovery.json when present (a task-tree mock home), and the trial
# keeps its own config.yaml and credentials.yaml; --in-caller-scope keeps the server in the caller's cgroup, which must
# already cap memory, and skips the scope probe and the scope start and stop. Every read of the release under test goes
# through the charliebot beside <server binary>: its `charliebot release-helper` verbs parse the responses, the logs
# and the sessions this script gathers.
#
# Exit: the command's exit code; 1 a foreign cgroup removal, a remaining child process or a cleanup failure; 2 the
# preflight failed, nothing started.
# Evidence goes to $CHARLIEBOT_RELEASE_RUN_DIR/trial-<UTC>/ (or a directory under $CHARLIEBOT_RELEASE_LOG_DIR) and is
# kept after a failure.
set -uo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SOURCE_REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"
readonly LIVE_HOME="${CHARLIEBOT_HOME:-$HOME/.charliebot}"
# The trial exports HOME for its own tree; the evidence parent is resolved before that.
readonly LOG_ROOT="${CHARLIEBOT_RELEASE_LOG_DIR:-$HOME/charliebot-release-logs}"
readonly EVIDENCE_PARENT="${CHARLIEBOT_RELEASE_RUN_DIR:-$LOG_ROOT/trial-manual-$(date -u +%Y%m%dT%H%M%SZ)}"
readonly CGROUP_ROOT="${CHARLIEBOT_TRIAL_CGROUP_ROOT:-/sys/fs/cgroup}"
readonly USER_SERVICE_CGROUP="$CGROUP_ROOT/user.slice/user-$(id -u).slice/user@$(id -u).service"
readonly CGROUP_BASE="$USER_SERVICE_CGROUP/charliebot.slice/charliebot-agents.slice"
# The port range a trial picks from. A caller that runs trials beside another trial names its own range.
readonly PORT_MIN=${TRIAL_PORT_MIN:-18650}
readonly PORT_MAX=${TRIAL_PORT_MAX:-18669}
# start_server polls the server's auth status at 0.25 s intervals this many times before it fails the trial. A serve
# server walks every session of its home at boot before it binds: on the 7,160-session mock home that took about a
# minute, so a serve trial waits 10 minutes.
readonly SERVE_READY_POLLS=2400

preflight_fail() {
  printf 'preflight failed: %s\n' "$1" >&2
  exit 2
}

IN_CALLER_SCOPE=0
TREE_ROOT=
SEED_HOME=
BINARY=
COMMAND=()
if [[ ${1:-} == --serve ]]; then
  shift
  while (( $# > 0 )); do
    case $1 in
      --tree-root)
        (( $# >= 2 )) || preflight_fail '--tree-root takes a directory'
        TREE_ROOT=$2
        shift 2
        ;;
      --seed-home)
        (( $# >= 2 )) || preflight_fail '--seed-home takes a directory'
        SEED_HOME=$2
        shift 2
        ;;
      --in-caller-scope)
        IN_CALLER_SCOPE=1
        shift
        ;;
      *)
        break
        ;;
    esac
  done
  if (( $# < 4 )); then
    preflight_fail 'arguments must be --serve [--tree-root <dir>] [--seed-home <dir>] [--in-caller-scope] <commit> <server binary> -- <command> [args...]'
  fi
  if [[ ! $1 =~ ^[[:xdigit:]]{40}$ ]]; then
    preflight_fail 'the --serve commit must be a full 40-character hexadecimal sha'
  fi
  if [[ -z $2 ]]; then
    preflight_fail 'the --serve server binary path must be non-empty'
  fi
  if [[ $3 != -- ]]; then
    preflight_fail 'arguments must be --serve [options] <commit> <server binary> -- <command> [args...]: the command follows a --'
  fi
  COMMIT=${1,,}
  BINARY=$2
  COMMAND=("${@:4}")
else
  preflight_fail 'arguments must be --serve [options] <commit> <server binary> -- <command> [args...]'
fi
readonly IN_CALLER_SCOPE
readonly TREE_ROOT
readonly SEED_HOME
readonly COMMIT
readonly BINARY
readonly COMMAND
readonly TRIAL_CLI="$(dirname "$BINARY")/charliebot"

if [[ ! -f $BINARY || ! -x $BINARY ]]; then
  preflight_fail "the server binary is an executable file: $BINARY"
fi
# The trial checks the release it was given: every helper below is the charliebot of the same CI build.
if [[ ! -x $TRIAL_CLI ]] || ! "$TRIAL_CLI" release-helper --help > /dev/null 2>&1; then
  preflight_fail "the charliebot beside the server binary runs release-helper: $TRIAL_CLI"
fi
git -C "$SOURCE_REPO" cat-file -e "$COMMIT^{commit}" 2>/dev/null \
  || preflight_fail "the trial sha $COMMIT is not in $SOURCE_REPO; fetch it first"

if [[ -n $TREE_ROOT && ( $TREE_ROOT != /* || ! -d $TREE_ROOT ) ]]; then
  preflight_fail "--tree-root is an existing directory given as an absolute path: $TREE_ROOT"
fi
if [[ -n $SEED_HOME && ! -d $SEED_HOME ]]; then
  preflight_fail "--seed-home is an existing directory: $SEED_HOME"
fi

# The serve server runs beside the live server, and this host gives an unprivileged process no read-only view of
# /sys/fs/cgroup (an unprivileged user may not create the mount namespace a read-only bind needs). The guard is
# therefore the sweep-check after the run plus this refusal: a binary whose cgroup code differs from main's never
# starts.
if ! SERVE_MERGE_BASE=$(git -C "$SOURCE_REPO" merge-base origin/main "$COMMIT" 2>/dev/null); then
  preflight_fail "the merge base of $COMMIT with origin/main resolves in $SOURCE_REPO"
fi
if ! SERVE_CGROUP_CHANGES=$(git -C "$SOURCE_REPO" diff --name-only "$SERVE_MERGE_BASE" "$COMMIT" -- \
  rust/crates/runtime/agents/src/transport/cgroup.rs rust/crates/infra/core/src/cgroup.rs \
  rust/crates/runtime/agents/src/agent_environment.rs); then
  preflight_fail "the cgroup-code diff of $COMMIT against its origin/main merge base runs in $SOURCE_REPO"
fi
if [[ -n $SERVE_CGROUP_CHANGES ]]; then
  preflight_fail "the trial sha $COMMIT changes cgroup code against its origin/main merge base: $(tr '\n' ' ' <<<"$SERVE_CGROUP_CHANGES")"
fi

if (( IN_CALLER_SCOPE == 0 )); then
  export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
  if [[ ! -d $XDG_RUNTIME_DIR ]]; then
    preflight_fail "the user runtime directory exists for the serve scope: $XDG_RUNTIME_DIR"
  fi
  if ! command -v systemd-run >/dev/null 2>&1; then
    preflight_fail 'systemd-run resolves on PATH for the serve scope'
  fi
  if ! systemd-run --user --scope -p MemoryMax=2G \
    --unit="charliebot-release-trial-serve-probe-$$" true >/dev/null 2>&1; then
    preflight_fail 'systemd-run starts a MemoryMax=2G user scope for the serve server'
  fi
fi

if ! command -v ldd >/dev/null 2>&1; then
  preflight_fail 'ldd is available for the result binary'
fi
if ! LDD_OUTPUT=$(ldd "$BINARY" 2>&1); then
  preflight_fail 'ldd can inspect the result binary'
fi
if grep -Fq 'not found' <<<"$LDD_OUTPUT"; then
  preflight_fail 'ldd resolves every shared library for the result binary'
fi

if ! CLC_PATH=$(command -v charlie-code 2>/dev/null) || [[ -z $CLC_PATH ]]; then
  preflight_fail 'charlie-code resolves on PATH'
fi

# The trial runs the local work guard with the live config's switch, read through the release's own CLI (`config get`
# resolves the key through the same load and defaults the server uses). The runner's host is not read here: the server
# does not take it at start, and the runner reads it from config.yaml on each run.
if ! LIVE_GUARD_SWITCH=$(CHARLIEBOT_HOME="$LIVE_HOME" "$TRIAL_CLI" config get server.enable_local_work_guard 2>&1); then
  preflight_fail "the live config reads through the trial CLI: server.enable_local_work_guard: $LIVE_GUARD_SWITCH"
fi
readonly LIVE_GUARD_SWITCH

# The trial's backend entry comes from the live config: the first option with an api_base, so the trial runs the
# operator's own model endpoint and the repository names no host and no credential. Its credential is optional. When
# the entry names one, the trial copies that section's key from the live credentials into the trial home and redacts
# it from saved logs; when it names none, the endpoint needs no client-side key, so the trial home's backend entry has
# no credential field and its credentials file carries no backend section.
#
# The same read gives the two session-weight switches, so the trial's server starts with the switches the live server
# runs with. An absent switch comes back at its default.
LIVE_BACKEND="$("$TRIAL_CLI" release-helper backend-option "$LIVE_HOME/config.yaml")" \
  || preflight_fail "the live config has a backends.options entry with api_base"
TRIAL_API_BASE="$(sed -n 1p <<<"$LIVE_BACKEND")"
TRIAL_CREDENTIAL="$(sed -n 2p <<<"$LIVE_BACKEND")"
CPU_WEIGHT_SWITCH="$(sed -n 3p <<<"$LIVE_BACKEND")"
IO_WEIGHT_SWITCH="$(sed -n 4p <<<"$LIVE_BACKEND")"
BACKEND_KEY=
if [[ -n $TRIAL_CREDENTIAL ]]; then
  if ! BACKEND_KEY=$("$TRIAL_CLI" release-helper credential-key "$LIVE_HOME/credentials.yaml" "$TRIAL_CREDENTIAL"); then
    preflight_fail "the live credentials file holds api_key for $TRIAL_CREDENTIAL"
  fi
fi
readonly TRIAL_API_BASE TRIAL_CREDENTIAL CPU_WEIGHT_SWITCH IO_WEIGHT_SWITCH

if ! CHOSEN_PORT=$("$TRIAL_CLI" release-helper free-port "$PORT_MIN" "$PORT_MAX"); then
  preflight_fail "a free 127.0.0.1 port exists in $PORT_MIN-$PORT_MAX"
fi
readonly CHOSEN_PORT

if [[ -n $TREE_ROOT ]]; then
  readonly TREE="${TREE_ROOT%/}/trial-${COMMIT:0:12}"
else
  readonly TREE="/tmp/charliebot-release-trial-$COMMIT"
fi
readonly TMUX_ROOT="$TREE/tmux"
if [[ -e $TREE ]]; then
  preflight_fail "trial tree does not already exist: $TREE"
fi
# tmux keeps each server's socket in $TMUX_TMPDIR/tmux-<uid>/, and sun_path holds 108 bytes with the final NUL.
TMUX_SOCKET_PATH="$TMUX_ROOT/tmux-$(id -u)/charliebot"
if (( ${#TMUX_SOCKET_PATH} >= 108 )); then
  preflight_fail "the tmux socket path stays under 108 bytes, the sun_path size with its final NUL: $TMUX_SOCKET_PATH (${#TMUX_SOCKET_PATH} bytes)"
fi

# The trial tree is the one this run creates: the default tree or the tree under --tree-root. Removal accepts only these.
is_trial_tree() {
  [[ $1 == "/tmp/charliebot-release-trial-$COMMIT" || ( -n $TREE_ROOT && $1 == "${TREE_ROOT%/}/trial-${COMMIT:0:12}" ) ]]
}

setup_cleanup_on_exit() {
  local exit_status=$?
  trap - EXIT INT TERM
  if [[ -d $TREE ]] && is_trial_tree "$TREE"; then
    if ! rm -rf -- "$TREE"; then
      exit_status=1
    fi
  fi
  exit "$exit_status"
}

if ! mkdir -m 700 "$TREE"; then
  preflight_fail "create trial tree $TREE"
fi
trap setup_cleanup_on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
if ! chmod 700 "$TREE"; then
  printf 'trial setup failed: set trial tree mode 700: %s\n' "$TREE" >&2
  exit 1
fi

readonly TRIAL_HOME="$TREE/home"
readonly TRIAL_CB_HOME="$TRIAL_HOME/.charliebot"
readonly REPO_ROOT="$TREE/repo"
readonly WORKSPACE="$TREE/workspace"
readonly WORKTREE_ROOT="$TREE/worktrees"
readonly TMP_ROOT="$TREE/tmp"
readonly HTTP_ROOT="$TREE/http"
SERVER_LOG=
SERVER_LOGS=()
readonly AUTH_CONFIG="$TREE/trial-auth.curl"
readonly CREDENTIALS="$TRIAL_CB_HOME/credentials.yaml"
readonly SESSIONS_DIR="$TRIAL_CB_HOME/sessions"

if ! mkdir -m 700 -p "$TRIAL_HOME" "$TRIAL_CB_HOME" "$REPO_ROOT" "$WORKSPACE" "$WORKTREE_ROOT" "$TMP_ROOT" "$TMUX_ROOT" "$HTTP_ROOT"; then
  printf 'trial setup failed: create trial directories inside %s\n' "$TREE" >&2
  exit 1
fi
if ! chmod 700 "$TRIAL_HOME" "$TRIAL_CB_HOME" "$WORKSPACE" "$WORKTREE_ROOT" "$TMP_ROOT" "$TMUX_ROOT" "$HTTP_ROOT"; then
  printf 'trial setup failed: set trial directory modes\n' >&2
  exit 1
fi

ACCESS_KEY=$(od -An -N32 -tx1 /dev/urandom | tr -d '[:space:]')
if [[ ! $ACCESS_KEY =~ ^[0-9a-f]{64}$ ]]; then
  printf 'trial setup failed: generate a random trial access key\n' >&2
  exit 1
fi
if [[ -n $TRIAL_CREDENTIAL ]]; then
  printf '%s:\n  api_key: %s\ncharliebot:\n  access_key: %s\n' "$TRIAL_CREDENTIAL" "$BACKEND_KEY" "$ACCESS_KEY" \
    >"$CREDENTIALS"
else
  printf 'charliebot:\n  access_key: %s\n' "$ACCESS_KEY" >"$CREDENTIALS"
fi
chmod 600 "$CREDENTIALS"
printf 'header = "Authorization: Bearer %s"\n' "$ACCESS_KEY" >"$AUTH_CONFIG"
chmod 600 "$AUTH_CONFIG"

# A session memory limit of 0 turns session cgroups off. The serve server starts no agent, so it runs with them off.
SESSION_MEMORY_MAX_MB=0
# The router the config's calls reach the pool through, as the host's routine class does. The stub answers the pool id
# for every role, so a call whose child keeps the pool id launches its work on one of the pool's members.
readonly TRIAL_ROUTER_STUB="$TREE/router-stub.sh"
cat >"$TRIAL_ROUTER_STUB" <<'STUB'
cat >/dev/null
printf '%s' '{"routed":true,"backends":["pool"],"reason":"release trial router stub"}'
STUB

cat >"$TRIAL_CB_HOME/config.yaml" <<EOF
server:
  host: 127.0.0.1
  port: $CHOSEN_PORT
  subprocess_buffer_limit_mb: 1024
  session_memory_max_mb: $SESSION_MEMORY_MAX_MB
  session_swap_max_mb: 0
  session_tasks_max: 256
  min_free_disk_gib: 0
  enable_session_cpu_weight: $CPU_WEIGHT_SWITCH
  enable_session_io_weight: $IO_WEIGHT_SWITCH
  # The guard runs with the live config's switch, so the trial checks the configuration that the switch will start.
  enable_local_work_guard: $LIVE_GUARD_SWITCH
paths:
  workspace_dirs:
    - $WORKSPACE
  worktree_dir: $WORKTREE_ROOT
backends:
  pools:
    - id: pool
      label: Pool
      rule: rotate
      members:
        - charlie-code-release-trial
  calls:
    worker: pool
    improve: pool
    verify: pool
    review: pool
    cold_read: pool
    probe: pool
    autoname: pool
    triage: pool
  routing:
    command: ["/bin/sh", "$TRIAL_ROUTER_STUB"]
  options:
    - id: charlie-code-release-trial
      label: Release trial
      type: charlie-code
      model: openai/glm-5.3-flash
      api_base: $TRIAL_API_BASE
EOF
if [[ -n $TRIAL_CREDENTIAL ]]; then
  printf '      credential: %s\n' "$TRIAL_CREDENTIAL" >>"$TRIAL_CB_HOME/config.yaml"
fi
chmod 600 "$TRIAL_CB_HOME/config.yaml"

if ! git -C "$SOURCE_REPO" archive "$COMMIT" | tar -x -C "$REPO_ROOT"; then
  printf 'trial setup failed: git archive extraction for %s\n' "$COMMIT" >&2
  exit 1
fi

export HOME="$TRIAL_HOME"
export TMPDIR="$TMP_ROOT"
export TMP="$TMP_ROOT"
export TEMP="$TMP_ROOT"
export TMUX_TMPDIR="$TMUX_ROOT"
export CHARLIEBOT_HOME="$TRIAL_CB_HOME"
export CHARLIEBOT_REPO_ROOT="$REPO_ROOT"
export XDG_CACHE_HOME="$TREE/xdg-cache"
export XDG_CONFIG_HOME="$TREE/xdg-config"
export XDG_DATA_HOME="$TREE/xdg-data"
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL="$TREE/gitconfig"
unset CHARLIE_CODE_API_KEY AIGW_API_KEY OPENAI_API_KEY ANTHROPIC_API_KEY

cat >"$GIT_CONFIG_GLOBAL" <<'EOF'
[user]
  name = Release trial
  email = release-trial@example.invalid
[init]
  defaultBranch = main
EOF

EVIDENCE_ROOT="$EVIDENCE_PARENT/trial-$(date -u +%Y%m%dT%H%M%S.%NZ)"
if ! mkdir -m 700 -p "$EVIDENCE_ROOT"; then
  printf 'trial setup failed: create the evidence directory %s\n' "$EVIDENCE_ROOT" >&2
  exit 1
fi
chmod 700 "$EVIDENCE_ROOT"
EVIDENCE_CAPTURED=0

CLEANUP_DONE=0
SERVER_PID=
SERVER_PIDS=()
OWNED_CGROUP_PATHS=()

sanitize_log_text() {
  TRIAL_REDACT_ACCESS_KEY="$ACCESS_KEY" \
    TRIAL_REDACT_BACKEND_KEY="$BACKEND_KEY" \
    "$TRIAL_CLI" release-helper redact-stdin
}

copy_redacted_file() {
  local source=$1
  local destination=$2
  mkdir -m 700 -p "$(dirname "$destination")" || return 1
  sanitize_log_text <"$source" >"$destination" || return 1
  chmod 600 "$destination" || return 1
}

capture_evidence() {
  (( EVIDENCE_CAPTURED == 0 )) || return 0
  local index=0
  local server_log
  for server_log in "${SERVER_LOGS[@]}"; do
    [[ -f $server_log ]] || continue
    index=$((index + 1))
    copy_redacted_file "$server_log" "$EVIDENCE_ROOT/server-$index.log" || return 1
  done
  EVIDENCE_CAPTURED=1
  return 0
}

process_state() {
  ps -p "$1" -o stat= 2>/dev/null | tr -d '[:space:]'
}

stop_server_pid() {
  local pid=$1
  local server_log=$2
  local check_number=$3
  local state
  local wait_status
  local sent=0
  local killed=0
  local start_ms=0
  local exit_ms=0
  local elapsed_ms=0
  local log_line
  local log_found=0
  state=$(process_state "$pid")
  if [[ -n $state && $state != *Z* ]]; then
    if kill -TERM "$pid" 2>/dev/null; then
      sent=1
      start_ms=$(date +%s%3N)
      while :; do
        state=$(process_state "$pid")
        if [[ -z $state || $state == *Z* ]]; then
          break
        fi
        exit_ms=$(date +%s%3N)
        if (( exit_ms - start_ms >= 20000 )); then
          killed=1
          printf 'SIGTERM check=%s pid=%s exceeded 20000 ms; sending SIGKILL\n' "$check_number" "$pid"
          kill -KILL "$pid" 2>/dev/null || true
          break
        fi
        sleep 0.1
      done
    else
      killed=1
      printf 'SIGTERM check=%s pid=%s kill_failed=1; sending SIGKILL for cleanup\n' "$check_number" "$pid"
      kill -KILL "$pid" 2>/dev/null || true
    fi
  else
    printf 'SIGTERM check=%s pid=%s was_not_running=1\n' "$check_number" "$pid"
  fi
  if wait "$pid"; then
    wait_status=0
  else
    wait_status=$?
  fi
  if (( sent == 1 )); then
    exit_ms=$(date +%s%3N)
    elapsed_ms=$((exit_ms - start_ms))
  fi
  printf 'SIGTERM check=%s pid=%s elapsed_ms=%s exit_status=%s sigterm_sent=%s sigkill_sent=%s\n' \
    "$check_number" "$pid" "$elapsed_ms" "$wait_status" "$sent" "$killed"
  if [[ -f $server_log ]]; then
    while IFS= read -r log_line; do
      log_found=1
      printf 'SIGTERM_LOG check=%s server-log=%s %s\n' "$check_number" "${server_log##*/}" "$log_line" | sanitize_log_text
    done < <(rg 'graceful_shutdown_timeout_exceeded|charliebot_shutdown' "$server_log" || true)
  fi
  if (( log_found == 0 )); then
    printf 'SIGTERM_LOG check=%s server-log=%s no matching shutdown lines\n' "$check_number" "${server_log##*/}"
  fi
  if (( sent == 1 && killed == 0 )) && [[ $wait_status == 0 || $wait_status == 143 ]]; then
    return 0
  fi
  return 1
}

stop_current_server() {
  local pid=${SERVER_PID:-}
  local check_number=${1:-6}
  [[ -n $pid ]] || return 0
  SERVER_PID=
  if ! stop_server_pid "$pid" "$SERVER_LOG" "$check_number"; then
    return 1
  fi
  return 0
}

cleanup_trial_tmux_servers() {
  local socket server_pid state
  local failed=0
  # tmux keeps each server's socket one level down, in $TMUX_ROOT/tmux-<uid>/. A server that has exited can leave its
  # socket file behind, so a socket that no server answers has no process to stop. The check waits for the server's
  # process to end, not for the file to go.
  for socket in "$TMUX_ROOT"/tmux-*/*; do
    [[ -S $socket ]] || continue
    if ! command -v tmux >/dev/null 2>&1; then
      printf 'trial tmux socket exists but tmux is unavailable: %s\n' "$socket" >&2
      failed=1
      continue
    fi
    if ! server_pid=$(tmux -S "$socket" display-message -p '#{pid}' 2>/dev/null); then
      continue
    fi
    if ! tmux -S "$socket" kill-server; then
      printf 'trial tmux server shutdown failed: %s\n' "$socket" >&2
      failed=1
      continue
    fi
    for _ in $(seq 1 30); do
      state=$(process_state "$server_pid")
      [[ -z $state || $state == *Z* ]] && break
      sleep 0.1
    done
    state=$(process_state "$server_pid")
    if [[ -n $state && $state != *Z* ]]; then
      printf 'trial tmux server remains after shutdown: pid %s on %s\n' "$server_pid" "$socket" >&2
      failed=1
    fi
  done
  return "$failed"
}

discover_owned_cgroups() {
  local path
  local found
  local server_log
  for server_log in "${SERVER_LOGS[@]}"; do
    [[ -f $server_log ]] || continue
    while IFS= read -r path; do
      [[ -n $path ]] || continue
      [[ $path == "$CGROUP_BASE"/charliebot-sess-* ]] || continue
      found=0
      for existing in "${OWNED_CGROUP_PATHS[@]}"; do
        if [[ $existing == "$path" ]]; then
          found=1
          break
        fi
      done
      if (( found == 0 )); then
        OWNED_CGROUP_PATHS+=("$path")
      fi
    done < <("$TRIAL_CLI" release-helper created-cgroups "$server_log" 2>/dev/null)
  done
}

is_owned_cgroup() {
  local expected=$1
  local path
  for path in "${OWNED_CGROUP_PATHS[@]}"; do
    [[ $path == "$expected" ]] && return 0
  done
  return 1
}

cleanup_owned_cgroups() {
  local path
  local pid
  local state
  local remaining
  local fail=0
  discover_owned_cgroups
  for path in "${OWNED_CGROUP_PATHS[@]}"; do
    [[ -d $path ]] || continue
    while IFS= read -r pid; do
      [[ $pid =~ ^[0-9]+$ ]] || continue
      state=$(process_state "$pid")
      if [[ -n $state && $state != *Z* ]]; then
        if ! kill -TERM "$pid" 2>/dev/null; then
          fail=1
        fi
      fi
    done <"$path/cgroup.procs"
    for _ in $(seq 1 30); do
      remaining=$(cat "$path/cgroup.procs" 2>/dev/null)
      [[ -z $remaining ]] && break
      sleep 0.1
    done
    while IFS= read -r pid; do
      [[ $pid =~ ^[0-9]+$ ]] || continue
      state=$(process_state "$pid")
      if [[ -n $state && $state != *Z* ]]; then
        if ! kill -KILL "$pid" 2>/dev/null; then
          fail=1
        fi
      fi
    done <"$path/cgroup.procs"
    for _ in $(seq 1 30); do
      remaining=$(cat "$path/cgroup.procs" 2>/dev/null)
      [[ -z $remaining ]] && break
      sleep 0.1
    done
    remaining=$(cat "$path/cgroup.procs" 2>/dev/null)
    if [[ -n $remaining ]]; then
      printf 'trial cgroup remains populated: %s\n' "$(basename "$path")" >&2
      fail=1
    elif ! rmdir "$path"; then
      printf 'trial cgroup removal failed: %s\n' "$(basename "$path")" >&2
      fail=1
    fi
  done
  return "$fail"
}

remove_trial_tree() {
  if [[ -d $TREE ]]; then
    if ! is_trial_tree "$TREE"; then
      printf 'refusing to remove unexpected trial path %s\n' "$TREE" >&2
      return 1
    fi
    rm -rf -- "$TREE"
  fi
  [[ ! -e $TREE ]]
}

cleanup_on_exit() {
  local exit_status=$?
  trap - EXIT INT TERM
  if (( CLEANUP_DONE == 0 )); then
    if ! stop_current_server 6; then
      exit_status=1
    fi
    if ! cleanup_owned_cgroups; then
      exit_status=1
    fi
    if ! cleanup_trial_tmux_servers; then
      exit_status=1
    fi
    if ! capture_evidence; then
      printf 'evidence capture failed: %s\n' "$EVIDENCE_ROOT" >&2
      exit_status=1
    fi
    remove_trial_tree || exit_status=1
    printf 'EVIDENCE path=%s\n' "$EVIDENCE_ROOT"
  fi
  exit "$exit_status"
}

trap cleanup_on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

AUTH_STATUS_FILE="$HTTP_ROOT/auth-status.json"

http_get() {
  local path=$1
  local output=$2
  local status
  status=$(curl --config "$AUTH_CONFIG" --silent --show-error --connect-timeout 2 --max-time 10 --output "$output" --write-out '%{http_code}' "http://127.0.0.1:$CHOSEN_PORT$path")
  local curl_status=$?
  if (( curl_status != 0 )); then
    status=000
  fi
  printf '%s' "$status"
}

# copy_seed_sessions copies each session directory of SEED_HOME into the trial home. A copy, not a link: the server
# appends to the event logs it serves, and the seed must stay as generated for the next run. A seed's boot marker and
# skip proof go where the task-tree walk reads them, so the walk compares the copied file times with the seed's marker
# and serves the seed proof's rows while those times still hold.
copy_seed_sessions() {
  local entry
  mkdir -m 700 -p "$SESSIONS_DIR" || return 1
  for entry in "$SEED_HOME"/*/; do
    [[ -d $entry ]] || continue
    cp -a -- "${entry%/}" "$SESSIONS_DIR/" || return 1
  done
  if [[ -f $SEED_HOME/boot_recovery.json ]]; then
    mkdir -m 700 -p "$TRIAL_CB_HOME/state" || return 1
    cp -a -- "$SEED_HOME/boot_recovery.json" "$TRIAL_CB_HOME/state/boot_recovery.json" || return 1
  fi
  if [[ -f $SEED_HOME/boot_skip_proof.json ]]; then
    mkdir -m 700 -p "$TRIAL_CB_HOME/state" || return 1
    cp -a -- "$SEED_HOME/boot_skip_proof.json" "$TRIAL_CB_HOME/state/boot_skip_proof.json" || return 1
  fi
  if [[ -f $SEED_HOME/boot_trigger_index.json ]]; then
    mkdir -m 700 -p "$TRIAL_CB_HOME/state" || return 1
    cp -a -- "$SEED_HOME/boot_trigger_index.json" "$TRIAL_CB_HOME/state/boot_trigger_index.json" || return 1
  fi
}

start_server() {
  local server_number=$(( ${#SERVER_PIDS[@]} + 1 ))
  SERVER_LOG="$TREE/server-$server_number.log"
  SERVER_LOGS+=("$SERVER_LOG")
  if (( IN_CALLER_SCOPE == 1 )); then
    # The server stays in the caller's cgroup, whose memory limit the caller's scope already sets.
    "$BINARY" >>"$SERVER_LOG" 2>&1 &
  else
    # The server runs inside a scope whose MemoryMax keeps a measurement beside the live server from pressing the host
    # into swap.
    systemd-run --user --scope -p MemoryMax=2G --unit="$SERVE_UNIT" "$BINARY" >>"$SERVER_LOG" 2>&1 &
  fi
  SERVER_PID=$!
  SERVER_PIDS+=("$SERVER_PID")
  for _ in $(seq 1 "$SERVE_READY_POLLS"); do
    local status
    status=$(http_get /api/auth/status "$AUTH_STATUS_FILE")
    # The poll names this server's port, which another server may hold: this server is ready when its own log says so.
    [[ $status == 200 ]] && grep -qE '[[:space:]]server_ready[[:space:]]' "$SERVER_LOG" && return 0
    [[ -n $(process_state "$SERVER_PID") && $(process_state "$SERVER_PID") != *Z* ]] || return 1
    sleep 0.25
  done
  return 1
}

# check_sweep_names LABEL: the startup sweep of the Rust server that wrote SERVER_LOG removed only session cgroups named for
# sessions under the trial home. `charliebot release-helper sweep-check` is the shared check. A populated cgroup that ends
# when its run ends is not a sweep removal, so the check reads only what the sweep logged.
check_sweep_names() {
  local label=$1 output rc
  output=$("$TRIAL_CLI" release-helper sweep-check "$SERVER_LOG" "$SESSIONS_DIR" 2>&1)
  rc=$?
  if (( rc == 0 )); then
    printf 'SWEEP_NAMES PASS %s: %s\n' "$label" "$output"
    return 0
  fi
  printf 'SWEEP_NAMES FAIL %s: a startup sweep removed a directory not named for a session under the trial home: %s\n' "$label" "$output"
  return 1
}

SERVE_UNIT=
SERVE_WAIT_RUNS_SECONDS=600

serve_home_has_active_run() {
  # Prints 1 while some run of a trial-home session is still queued or running, else 0. A stopped, withheld or
  # input-blocked run consumes no server process, so the active set is queued and running only. Every page of a
  # session's runs is read, because the newest run is on the last page; a session whose runs cannot be read counts as
  # active, because its runs are not proven ended.
  local trial_session_id trial_session_ids runs_status cursor page_output
  local -a page=()
  if [[ ! -d $SESSIONS_DIR ]]; then
    printf '0'
    return 0
  fi
  trial_session_ids=$("$TRIAL_CLI" release-helper session-ids "$SESSIONS_DIR") || {
    printf '1'
    return 0
  }
  while IFS= read -r trial_session_id; do
    [[ -n $trial_session_id ]] || continue
    cursor=
    while :; do
      runs_status=$(http_get "/api/sessions/$trial_session_id/runs?order=asc&limit=500${cursor:+&cursor=$cursor}" \
        "$HTTP_ROOT/serve-runs.json")
      if [[ $runs_status != 200 ]] || ! page_output=$("$TRIAL_CLI" release-helper run-page "$HTTP_ROOT/serve-runs.json"); then
        printf '1'
        return 0
      fi
      mapfile -t page <<<"$page_output"
      if [[ ${page[0]} == 1 ]]; then
        printf '1'
        return 0
      fi
      cursor=${page[1]:-}
      [[ -n $cursor ]] || break
    done
  done <<<"$trial_session_ids"
  printf '0'
}

stop_serve_scope() {
  # The scope normally ends with the server process; the stop clears a unit that outlived an unclean server exit, and a
  # scope whose server died abnormally is loaded in the failed state, which reset-failed unloads.
  local unit=$1
  local active
  active=$(systemctl --user is-active "$unit" 2>/dev/null)
  if [[ $active == active ]] && ! systemctl --user stop "$unit"; then
    printf 'trial serve scope stop failed: %s\n' "$unit" >&2
    return 1
  fi
  if [[ $active == failed ]] && ! systemctl --user reset-failed "$unit" >/dev/null 2>&1; then
    printf 'trial serve scope reset-failed failed: %s\n' "$unit" >&2
    return 1
  fi
  printf 'SERVE scope stop unit=%s state=%s\n' "$unit" "${active:-unreachable}"
  return 0
}

run_serve_mode() {
  # One caller command. The exit code is the command's, demoted to 1 by a foreign cgroup removal (sweep-check) or any
  # cleanup failure.
  local cmd_rc=0 sweep_failed=0 cleanup_failed=0
  local wait_started server_pid state stray_pids
  SERVE_UNIT="charliebot-release-trial-serve-$(date -u +%Y%m%dT%H%M%SZ)-$$"
  if [[ -n $SEED_HOME ]]; then
    if ! copy_seed_sessions; then
      printf 'FAIL serve Seed: the sessions of %s did not copy into %s\n' "$SEED_HOME" "$SESSIONS_DIR"
      return 1
    fi
    printf 'SERVE seed from=%s into=%s\n' "$SEED_HOME" "$SESSIONS_DIR"
  fi
  if ! start_server; then
    printf 'FAIL serve Startup: the server did not become ready on 127.0.0.1:%s\n' "$CHOSEN_PORT"
    printf 'CAUSE serve: the trial server exited or did not bind the selected port\n'
    return 1
  fi
  if (( IN_CALLER_SCOPE == 1 )); then
    printf 'SERVE PASS Startup url=http://127.0.0.1:%s pid=%s scope=caller\n' "$CHOSEN_PORT" "$SERVER_PID"
  else
    printf 'SERVE PASS Startup url=http://127.0.0.1:%s pid=%s unit=%s memory_max=2147483648\n' "$CHOSEN_PORT" "$SERVER_PID" "$SERVE_UNIT"
  fi
  check_sweep_names 'start 1' || sweep_failed=1

  export CHARLIEBOT_SERVE_URL="http://127.0.0.1:$CHOSEN_PORT"
  export CHARLIEBOT_SERVE_AUTH_HEADER="Authorization: Bearer $ACCESS_KEY"
  export CHARLIEBOT_SERVE_PID="$SERVER_PID"
  printf 'SERVE command start: %s\n' "${COMMAND[*]}"
  "${COMMAND[@]}"
  cmd_rc=$?
  unset CHARLIEBOT_SERVE_URL CHARLIEBOT_SERVE_AUTH_HEADER CHARLIEBOT_SERVE_PID
  printf 'SERVE command exit=%s\n' "$cmd_rc"

  wait_started=$SECONDS
  while [[ $(serve_home_has_active_run) == 1 ]]; do
    if (( SECONDS - wait_started >= SERVE_WAIT_RUNS_SECONDS )); then
      cleanup_failed=1
      printf 'FAIL serve RunWait: a trial-home run is still queued or running after %s s\n' "$SERVE_WAIT_RUNS_SECONDS"
      break
    fi
    sleep 1
  done
  if (( cleanup_failed == 0 )); then
    printf 'SERVE PASS RunWait: every run of the trial home has ended\n'
  fi

  if ! stop_current_server 6; then
    cleanup_failed=1
  fi
  if (( IN_CALLER_SCOPE == 0 )) && ! stop_serve_scope "$SERVE_UNIT"; then
    cleanup_failed=1
  fi
  if ! cleanup_owned_cgroups; then
    cleanup_failed=1
  fi
  if ! cleanup_trial_tmux_servers; then
    cleanup_failed=1
  fi
  for server_pid in "${SERVER_PIDS[@]}"; do
    state=$(process_state "$server_pid")
    if [[ -n $state && $state != *Z* ]]; then
      cleanup_failed=1
      printf 'FAIL serve ChildProcess: the server process %s remains\n' "$server_pid"
    fi
  done
  stray_pids=$(pgrep -f "$TREE" || true)
  if [[ -n $stray_pids ]]; then
    cleanup_failed=1
    printf 'FAIL serve ChildProcess: processes of the trial tree remain: %s\n' "$stray_pids"
  else
    printf 'SERVE PASS ChildProcess: no process of the trial tree remains\n'
  fi
  check_sweep_names 'serve' || sweep_failed=1

  if ! capture_evidence; then
    cleanup_failed=1
    printf 'evidence capture failed: %s\n' "$EVIDENCE_ROOT" >&2
  fi
  if ! remove_trial_tree; then
    cleanup_failed=1
  fi
  CLEANUP_DONE=1
  printf 'EVIDENCE path=%s\n' "$EVIDENCE_ROOT"
  if (( sweep_failed == 1 || cleanup_failed == 1 )); then
    return 1
  fi
  return "$cmd_rc"
}

run_serve_mode
exit $?
