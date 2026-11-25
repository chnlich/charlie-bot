#!/usr/bin/env bash
# Serve one caller command against a server binary with its paired CLI in the same directory.
#
#   scripts/release/trial.sh --serve [--tree-root <dir>] [--seed-home <dir>] [--in-caller-scope] <commit> <server binary> -- <command> [args...]
#
# The commit is extracted from this repository into an isolated tree with its own home, private tmux server and a port
# in 18650-18669, and the server starts on it. The command then runs with CHARLIEBOT_SERVE_URL,
# CHARLIEBOT_SERVE_AUTH_HEADER (one line `Authorization: Bearer <key>`), CHARLIEBOT_SERVE_PID, and
# CHARLIEBOT_SERVE_PID_FILE in its environment. A command that restarts the server updates the pid file, which the
# trial follows until every run ends and it stops the current server.
# The server runs inside a systemd user scope with MemoryMax=16G, so a measurement cannot push the live server into
# swap. A server that the scope's OOM killer
# ends fails the trial with a MemoryCap line that names the cap, the peak and the scope. A trial whose server still runs
# at the end of its wait prints the scope's memory peak as SERVE PASS MemoryPeak. After the command ends the trial waits
# until every run of the trial home has ended, stops the server, asserts that no process of the trial tree remains,
# cleans up and runs sweep-check on the server log. After a scoped server exits, the trial waits up to
# SERVE_EXIT_WAIT_SECONDS for its scope to reach a terminal state before classifying the exit. For a scoped serve, a
# runs-read error first waits up to the same bound for the server to exit. If either wait cannot establish the state,
# the trial does not report a memory cap. In --in-caller-scope mode, memory-cap classification stays disabled and a
# runs-read error is reported as before. A server that does not exit on SIGTERM within SERVER_STOP_WAIT_MS fails the
# side; its CAUSE line names its pid, binary and scope.
#
# A side is ready only when the port's listener is this trial's server (or a process it started) and that server
# answers its auth status with 200 after it logged server_ready. free-port picks the port before the server binds it,
# so another process can take the port in between, and a side that starts while another side's server is still booting
# can pick the same port. Then the side fails at start: the FAIL and CAUSE lines name the port and its holder, and no
# command runs. Each server carries its trial's pid in CHARLIEBOT_RELEASE_TRIAL_PID. A holder whose trial has ended is a
# teardown defect, named as one with the holder's pid, binary and scope. A holder whose trial still runs is a side
# serving at the same time. Any other holder is reported as another process.
#
# This host gives an unprivileged process no read-only view of /sys/fs/cgroup, so the preflight refuses (exit 2,
# nothing started) a commit whose diff against its merge base with origin/main touches the cgroup code
# (rust/crates/runtime/agents/src/transport/cgroup.rs, rust/crates/infra/core/src/cgroup.rs or
# rust/crates/runtime/agents/src/agent_environment.rs): the serve server runs only binaries whose cgroup code equals
# main's. It runs with session cgroups off (session_memory_max_mb 0), starts no agent and installs nothing, so it needs
# no writable agents slice; the tmux and Chrome singleton sockets under its temp paths must keep their paths under the
# socket-path limit, the sun_path size with the final NUL.
#
# The config carries the live config's weight switches, server.enable_session_cpu_weight and
# server.enable_session_io_weight, read by `release-helper session-weights`; an absent switch takes the server's
# default. The trial's model endpoint is the first member of the `backends.calls.probe` target that names an api_base:
# the answer that CHARLIEBOT_TRIAL_PROBE_ANSWER names when it is set, else the live home's answer to
# `release-helper endpoint-option`.
# The two Claude stand-in options are the step backends of the default memory-curator,
# and `release-helper stand-in-options` copies each record from the commit's configs/config.example.yaml. The trial
# fires no scheduled task: before its server starts, `release-helper disable-cron-tasks` seeds the commit's default cron
# tasks and sets `enabled: false` on every cron task file of the trial home, copied seed tasks included. Once the server
# is ready, the trial reads its task list back and fails the side unless every task reads disabled. The scheduler still
# binds a session to each task that has none, on its first tick, enabled or not, so those sessions join the trial home.
# CHARLIEBOT_TRIAL_CGROUP_ROOT replaces /sys/fs/cgroup, the root the agents slice path is built on, so the tests point
# it at a sandbox.
#
# Options: --tree-root <dir> builds the tree as <dir>/trial-<first 12 hex of the commit>, so runs of two commits under
# one root share no tree. Without it the tree is ${TMPDIR:-/tmp}/charliebot-release-trial-<uid>-<first 12 hex>, so one
# user's trials of one commit name one path, and a tree path that already exists refuses the run, naming the path and
# its owner. The temp root that the trial exports as TMPDIR is <tree>/tmp with --tree-root, and without it the short
# directory ${TMPDIR:-/tmp}/t-<first 12 hex of the SHA-256 of "<uid> <commit>"> beside the tree; the cleanup removes
# both. The temp root holds the tmux socket directory too. --seed-home <dir> copies each session directory of <dir>
# into the trial home before the server starts, with the boot marker <dir>/boot_recovery.json when present (a task-tree
# mock home), and the trial keeps its own config.yaml and credentials.yaml; --in-caller-scope keeps the server in the
# caller's cgroup, which must already cap memory, and skips the scope probe and the scope start and stop. Every read of
# the release under test goes through the charliebot beside <server binary>: its `charliebot release-helper` verbs parse
# the responses, the logs and the sessions this script gathers.
#
# Exit: the command's exit code; 1 a foreign cgroup removal, a remaining child process, a cleanup failure, a server
# that did not become ready on its own port, a server that stopped before the wait for its runs ended, a server that
# did not exit on SIGTERM, a server that its scope's memory cap ended, or a scheduled task that the started server does
# not read as disabled; 2 the preflight failed, nothing started.
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
# The serve scope's MemoryMax. It holds an 11 GiB mock home and the server's working set in the scope's page cache: the
# largest measured ux side peaked at 12.98 GB (12.09 GiB) in this scope, so 16 GiB leaves 32% above it while a runaway
# server still cannot take more than 16 GiB from the host.
readonly SERVE_MEMORY_MAX=16G
readonly SERVE_MEMORY_MAX_BYTES=$(( ${SERVE_MEMORY_MAX%G} * 1024 * 1024 * 1024 ))
# start_server polls the server's auth status at 0.25 s intervals this many times before it fails the trial. A serve
# server walks every session of its home at boot before it binds: on the 7,160-session mock home that took about a
# minute, so a serve trial waits 10 minutes.
readonly SERVE_READY_POLLS=2400
# stop_server_pid waits this long for a server to exit after SIGTERM, then sends SIGKILL and fails the side.
readonly SERVER_STOP_WAIT_MS=20000
# Each wait for the server or its scope to settle uses this bound.
readonly SERVE_EXIT_WAIT_SECONDS=10
# The scheduled task list route of the server, which charliebot_cron names TASKS_PATH.
readonly CRON_TASKS_PATH=/api/cron/tasks

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
  if ! systemd-run --user --scope -p "MemoryMax=$SERVE_MEMORY_MAX" \
    --unit="charliebot-release-trial-serve-probe-$$" true >/dev/null 2>&1; then
    preflight_fail "systemd-run starts a MemoryMax=$SERVE_MEMORY_MAX user scope for the serve server"
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
if ! command -v ss >/dev/null 2>&1; then
  preflight_fail 'ss resolves on PATH for the port-holder check'
fi
if ! command -v pgrep >/dev/null 2>&1; then
  preflight_fail 'pgrep resolves on PATH for the server-tree check'
fi
if ! command -v jq >/dev/null 2>&1; then
  preflight_fail 'jq resolves on PATH for the probe answer'
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

# The trial's model endpoint is the first member of the `backends.calls.probe` target that names an api_base, so the
# trial runs the operator's own probe model and the repository names no model, host or credential. The answer is
# `charliebot release-helper endpoint-option <live home> probe`, which walks the target's members in config order and
# takes the first with an api_base. CHARLIEBOT_TRIAL_PROBE_ANSWER names a file that holds that answer: an improve
# round's launcher resolves it where the server runs and ships it, so a host that runs no server reads it there. Without
# the variable the live home answers. The member needs an api_base, since the trial's charlie-code option is an endpoint
# type. Its credential is optional. When the member names one, the trial copies that section's key from the live
# credentials into the trial home and redacts it from saved logs; when it names none, the endpoint needs no client-side
# key, so the trial home's backend entry has no credential field and its credentials file carries no backend section.
if [[ -v CHARLIEBOT_TRIAL_PROBE_ANSWER ]]; then
  PROBE_SOURCE=$CHARLIEBOT_TRIAL_PROBE_ANSWER
  PROBE_ANSWER=$(cat -- "$PROBE_SOURCE") \
    || preflight_fail "CHARLIEBOT_TRIAL_PROBE_ANSWER names a file that cannot be read: $PROBE_SOURCE"
else
  PROBE_SOURCE="the probe target of the live home at $LIVE_HOME"
  PROBE_ANSWER=$(CHARLIEBOT_HOME="$LIVE_HOME" "$TRIAL_CLI" release-helper endpoint-option "$LIVE_HOME" probe) \
    || preflight_fail "no probe answer: CHARLIEBOT_TRIAL_PROBE_ANSWER is unset, and the live home names no endpoint member for backends.calls.probe"
fi
TRIAL_PROBE_ID=$(jq -er '.option.id' <<<"$PROBE_ANSWER") \
  || preflight_fail "the probe answer from $PROBE_SOURCE names no option id"
TRIAL_MODEL=$(jq -r '.option.model // empty' <<<"$PROBE_ANSWER")
TRIAL_API_BASE=$(jq -r '.option.api_base // empty' <<<"$PROBE_ANSWER")
TRIAL_CREDENTIAL=$(jq -r '.option.credential // empty' <<<"$PROBE_ANSWER")
[[ -n $TRIAL_MODEL ]] || preflight_fail "the probe member $TRIAL_PROBE_ID names no model"
[[ -n $TRIAL_API_BASE ]] || preflight_fail "the probe member $TRIAL_PROBE_ID names no api_base; the trial needs one"

# The two session-weight switches come from the live config, so the trial's server starts with the switches the live
# server runs with. An absent switch comes back at its default.
SESSION_WEIGHTS="$("$TRIAL_CLI" release-helper session-weights "$LIVE_HOME/config.yaml")" \
  || preflight_fail "the live config's session-weight switches read through the trial CLI"
CPU_WEIGHT_SWITCH="$(sed -n 1p <<<"$SESSION_WEIGHTS")"
IO_WEIGHT_SWITCH="$(sed -n 2p <<<"$SESSION_WEIGHTS")"
BACKEND_KEY=
if [[ -n $TRIAL_CREDENTIAL ]]; then
  if ! BACKEND_KEY=$("$TRIAL_CLI" release-helper credential-key "$LIVE_HOME/credentials.yaml" "$TRIAL_CREDENTIAL"); then
    preflight_fail "the live credentials file holds api_key for $TRIAL_CREDENTIAL"
  fi
fi
readonly TRIAL_PROBE_ID TRIAL_MODEL TRIAL_API_BASE TRIAL_CREDENTIAL
readonly CPU_WEIGHT_SWITCH IO_WEIGHT_SWITCH

if ! CHOSEN_PORT=$("$TRIAL_CLI" release-helper free-port "$PORT_MIN" "$PORT_MAX"); then
  preflight_fail "a free 127.0.0.1 port exists in $PORT_MIN-$PORT_MAX"
fi
readonly CHOSEN_PORT

# Without --tree-root the tree sits in the caller's TMPDIR, and the uid in its name keeps users apart. A round's write
# boundary allows the round's TMPDIR, so a round's trial needs no --tree-root.
#
# The temp root holds every socket the preflight checks, so its name sets the socket budget. A round's TMPDIR is
# /tmp/improve-<direction>-tmp/scope.<8 characters>: 34 bytes for ux, and 42 for the longest direction name on host3.
# Chrome's socket adds 45 bytes to the temp root, so the default temp root is a short sibling of the tree, named
# t-<first 12 hex of the SHA-256 of "<uid> <full commit>">. The digest keeps the uid's digits out of the name, so the
# budget does not grow with the uid's length (host3's uid has six digits), and it separates users and commits that
# share one TMPDIR. The cleanup removes the temp root with the tree.
if [[ -n $TREE_ROOT ]]; then
  readonly TREE="${TREE_ROOT%/}/trial-${COMMIT:0:12}"
  readonly TMP_ROOT="$TREE/tmp"
else
  readonly TREE="${TMPDIR:-/tmp}/charliebot-release-trial-$(id -u)-${COMMIT:0:12}"
  TMP_KEY=$(printf '%s %s\n' "$(id -u)" "$COMMIT" | sha256sum) \
    || preflight_fail "the default trial temp root needs sha256sum to name it, for commit $COMMIT"
  readonly TMP_ROOT="${TMPDIR:-/tmp}/t-${TMP_KEY:0:12}"
  if [[ $TREE != /* ]]; then
    preflight_fail "the default trial tree needs an absolute TMPDIR, got: $TREE"
  fi
fi
# tmux's socket directory sits under the temp root, beside Chrome's, so the temp root owns every socket path
# checked below.
readonly TMUX_ROOT="$TMP_ROOT/tmux"
if [[ -e $TREE ]]; then
  tree_owner=$(stat -c %U -- "$TREE") || preflight_fail "the trial tree path exists and its owner cannot be read: $TREE"
  preflight_fail "the trial tree path already exists: $TREE (owned by $tree_owner)"
fi
# The sockets that the trial's processes create under its temp paths: tmux keeps its server socket in
# $TMUX_TMPDIR/tmux-<uid>/, and Chrome keeps its singleton socket in a mkdtemp directory under TMPDIR. The six random
# characters of that directory name are written XXXXXX, which has the same length. sun_path holds
# SUN_PATH_LIMIT bytes with the final NUL.
readonly SUN_PATH_LIMIT=108
socket_paths_fit() {
  local socket_path socket_bytes failed=0
  for socket_path in "$TMUX_ROOT/tmux-$(id -u)/charliebot" "$TMP_ROOT/org.chromium.Chromium.XXXXXX/SingletonSocket"; do
    socket_bytes=$(printf '%s' "$socket_path" | wc -c)
    if (( socket_bytes >= SUN_PATH_LIMIT )); then
      printf 'preflight failed: socket path must stay under %s bytes (sun_path with final NUL): %s (%s bytes)\n' \
        "$SUN_PATH_LIMIT" "$socket_path" "$socket_bytes" >&2
      failed=1
    else
      printf 'PREFLIGHT PASS socket: %s (%s bytes)\n' "$socket_path" "$socket_bytes"
    fi
  done
  return "$failed"
}
socket_paths_fit || exit 2

# remove_trial_paths removes the trial's tree and temp root when they exist. This run creates both, and removal touches
# no other path. The setup trap and the cleanup both call it.
remove_trial_paths() {
  local path
  for path in "$TREE" "$TMP_ROOT"; do
    if [[ -e $path ]] && ! rm -rf -- "$path"; then
      return 1
    fi
  done
}

setup_cleanup_on_exit() {
  local exit_status=$?
  trap - EXIT INT TERM
  if ! remove_trial_paths; then
    exit_status=1
  fi
  exit "$exit_status"
}

if ! mkdir -m 700 "$TREE"; then
  tree_parent=$(dirname -- "$TREE")
  parent_owner=$(stat -c %U -- "$tree_parent") || exit 2
  preflight_fail "create trial tree $TREE: mkdir failed above, and its directory $tree_parent is owned by $parent_owner"
fi
if ! mkdir -m 700 "$TMP_ROOT"; then
  rmdir -- "$TREE"
  preflight_fail "create trial temp root $TMP_ROOT: mkdir failed above"
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
readonly HTTP_ROOT="$TREE/http"
SERVER_LOG=
SERVER_LOGS=()
readonly AUTH_CONFIG="$TREE/trial-auth.curl"
readonly CREDENTIALS="$TRIAL_CB_HOME/credentials.yaml"
readonly SESSIONS_DIR="$TRIAL_CB_HOME/sessions"

if ! mkdir -m 700 -p "$TRIAL_HOME" "$TRIAL_CB_HOME" "$REPO_ROOT" "$WORKSPACE" "$WORKTREE_ROOT" "$TMUX_ROOT" "$HTTP_ROOT"; then
  printf 'trial setup failed: create trial directories inside %s\n' "$TREE" >&2
  exit 1
fi
if ! chmod 700 "$TRIAL_HOME" "$TRIAL_CB_HOME" "$WORKSPACE" "$WORKTREE_ROOT" "$TMUX_ROOT" "$HTTP_ROOT"; then
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

# trial_probe_option prints the trial's charlie-code option: the probe member's model, endpoint and credential name.
trial_probe_option() {
  printf '    - id: %s\n      label: %s\n      type: charlie-code\n      model: %s\n' "$1" "$2" "$3"
  printf '      prompt_overlay: none\n      api_base: %s\n' "$TRIAL_API_BASE"
  if [[ -n $TRIAL_CREDENTIAL ]]; then
    printf '      credential: %s\n' "$TRIAL_CREDENTIAL"
  fi
}

if ! git -C "$SOURCE_REPO" archive "$COMMIT" | tar -x -C "$REPO_ROOT"; then
  printf 'trial setup failed: git archive extraction for %s\n' "$COMMIT" >&2
  exit 1
fi

# The stand-in options are the default memory-curator's step backends, each copied from the commit's example config by
# id. The seed of that task needs them in the trial's config; the script names neither their ids nor their models.
if ! STAND_IN_OPTIONS=$("$TRIAL_CLI" release-helper stand-in-options "$REPO_ROOT/configs/cron.default.yaml" \
  "$REPO_ROOT/configs/config.example.yaml"); then
  printf 'trial setup failed: the stand-in options of the default memory-curator, reason above\n' >&2
  exit 1
fi
STAND_IN_OPTION_LINES=$(sed 's/^/    /' <<<"$STAND_IN_OPTIONS")

# backends.calls.new_session names the trial's first option, the target new sessions took before the key was required.
# Without it the server refuses to start.
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
    new_session: charlie-code-release-trial
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
$(trial_probe_option charlie-code-release-trial "Release trial" "$TRIAL_MODEL")
$STAND_IN_OPTION_LINES
EOF
chmod 600 "$TRIAL_CB_HOME/config.yaml"

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
SERVE_UNIT_STARTED=0
SERVER_PID=
SERVER_PID_FILE=
SERVER_CGROUP_DIR=
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

# stop_failed CAUSE: the FAIL line of a serve stop, then the cause, which names the server's pid, its binary and its
# scope: the systemd unit, or the caller's cgroup with --in-caller-scope.
stop_failed() {
  local scope=$SERVE_UNIT
  if (( IN_CALLER_SCOPE == 1 )); then
    scope=caller
  fi
  printf 'FAIL serve Stop: the trial server did not stop cleanly\n'
  printf 'CAUSE serve: %s (server binary %s, scope %s)\n' "$1" "$BINARY" "$scope"
}

# stop_server_pid PID LOG CHECK WAITABLE: stops the server PID with SIGTERM and waits at most SERVER_STOP_WAIT_MS for it
# to exit. A server that is still running then gets SIGKILL, so no process of the trial outlives the stop, and the stop
# fails. A child PID must exit with status 0 or 143; a replacement PID must log a new charliebot_shutdown line.
stop_server_pid() {
  local pid=$1
  local server_log=$2
  local check_number=$3
  local waitable=$4
  local state
  local wait_status
  local sent=0
  local killed=0
  local start_ms=0
  local exit_ms=0
  local elapsed_ms=0
  local log_line
  local log_found=0
  local log_offset=0
  local graceful_exit=0
  local stop_cause=
  if [[ -f $server_log ]]; then
    log_offset=$(wc -c <"$server_log")
  fi
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
        if (( exit_ms - start_ms >= SERVER_STOP_WAIT_MS )); then
          killed=1
          stop_cause="server pid $pid did not exit within $SERVER_STOP_WAIT_MS ms of SIGTERM; the trial sent SIGKILL"
          printf 'SIGTERM check=%s pid=%s exceeded %s ms; sending SIGKILL\n' \
            "$check_number" "$pid" "$SERVER_STOP_WAIT_MS"
          kill -KILL "$pid" 2>/dev/null || true
          break
        fi
        sleep 0.1
      done
    else
      killed=1
      stop_cause="SIGTERM to server pid $pid failed; the trial sent SIGKILL"
      printf 'SIGTERM check=%s pid=%s kill_failed=1; sending SIGKILL for cleanup\n' "$check_number" "$pid"
      kill -KILL "$pid" 2>/dev/null || true
    fi
  else
    printf 'SIGTERM check=%s pid=%s was_not_running=1\n' "$check_number" "$pid"
  fi
  if (( waitable == 1 )); then
    if wait "$pid"; then
      wait_status=0
    else
      wait_status=$?
    fi
  else
    wait_status=unavailable
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
    if (( waitable == 0 )) && tail -c +$((log_offset + 1)) "$server_log" | rg -q 'charliebot_shutdown'; then
      graceful_exit=1
    fi
  fi
  if (( log_found == 0 )); then
    printf 'SIGTERM_LOG check=%s server-log=%s no matching shutdown lines\n' "$check_number" "${server_log##*/}"
  fi
  if (( sent == 1 && killed == 0 )); then
    if (( waitable == 1 )) && [[ $wait_status == 0 || $wait_status == 143 ]]; then
      return 0
    fi
    if (( waitable == 0 && graceful_exit == 1 )); then
      return 0
    fi
  fi
  if (( sent == 1 && killed == 0 )); then
    if (( waitable == 0 )); then
      stop_cause="server pid $pid exited after SIGTERM without a new charliebot_shutdown log line"
    else
      stop_cause="server pid $pid exited with status $wait_status after SIGTERM"
    fi
  fi
  if [[ -n $stop_cause ]]; then
    stop_failed "$stop_cause"
  fi
  return 1
}

stop_current_server() {
  local pid=${SERVER_PID:-}
  local check_number=${1:-6}
  [[ -n $pid ]] || return 0
  local server_pid
  local waitable=0
  if ! IFS= read -r pid <"$SERVER_PID_FILE"; then
    printf 'trial server pid file cannot be read: %s\n' "$SERVER_PID_FILE" >&2
    return 1
  fi
  if [[ ! $pid =~ ^[1-9][0-9]*$ ]]; then
    printf 'trial server pid file does not contain a positive pid: %s\n' "$SERVER_PID_FILE" >&2
    return 1
  fi
  for server_pid in "${SERVER_PIDS[@]}"; do
    if [[ $server_pid == "$pid" ]]; then
      waitable=1
      break
    fi
  done
  if (( waitable == 0 )); then
    SERVER_PIDS+=("$pid")
  fi
  SERVER_PID=
  if ! stop_server_pid "$pid" "$SERVER_LOG" "$check_number" "$waitable"; then
    return 1
  fi
  return 0
}

cleanup_trial_tmux_servers() {
  local socket server_pid state pid uid pids rc=0
  local failed=0
  uid=$(id -u)
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
  # When the socket file under $TMUX_ROOT was removed mid-run (or kill-server did not end the server), find any
  # surviving tmux server started with this trial's TMUX_TMPDIR and stop it directly.
  if pids=$(pgrep -u "$uid" '^tmux'); then
    for pid in $pids; do
      [[ -r /proc/$pid/environ ]] || continue
      grep -Fzqx "TMUX_TMPDIR=$TMUX_ROOT" "/proc/$pid/environ" 2>/dev/null || continue
      state=$(process_state "$pid")
      [[ -n $state && $state != *Z* ]] || continue
      if ! kill -TERM "$pid" 2>/dev/null; then
        state=$(process_state "$pid")
        [[ -z $state || $state == *Z* ]] && continue
        printf 'trial tmux server SIGTERM failed: pid %s\n' "$pid" >&2
        failed=1
        continue
      fi
      for _ in $(seq 1 30); do
        state=$(process_state "$pid")
        [[ -z $state || $state == *Z* ]] && break
        sleep 0.1
      done
      state=$(process_state "$pid")
      if [[ -n $state && $state != *Z* ]]; then
        kill -KILL "$pid" 2>/dev/null || true
        for _ in $(seq 1 10); do
          state=$(process_state "$pid")
          [[ -z $state || $state == *Z* ]] && break
          sleep 0.1
        done
      fi
      state=$(process_state "$pid")
      if [[ -n $state && $state != *Z* ]]; then
        printf 'trial tmux server remains after fallback shutdown: pid %s\n' "$pid" >&2
        failed=1
      fi
    done
  else
    rc=$?
    if (( rc != 1 )); then
      printf 'trial tmux fallback scan failed: pgrep exited %s\n' "$rc" >&2
      failed=1
    fi
  fi
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

# cleanup_on_exit is the EXIT trap. A serve trial that ends before run_serve_mode stops its server stops it here, so no
# process of the trial outlives the trial: a bench lock the caller holds on an inherited descriptor is free once the
# trial ends.
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
    if (( IN_CALLER_SCOPE == 0 && SERVE_UNIT_STARTED == 1 )) && ! stop_serve_scope "$SERVE_UNIT"; then
      exit_status=1
    fi
    if ! capture_evidence; then
      printf 'evidence capture failed: %s\n' "$EVIDENCE_ROOT" >&2
      exit_status=1
    fi
    remove_trial_paths || exit_status=1
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
  if [[ -d $SEED_HOME/.cron.d ]]; then
    mkdir -m 700 -p "$TRIAL_CB_HOME/config.d/cron.d" || return 1
    for task in "$SEED_HOME"/.cron.d/*.yaml; do
      [[ -f $task ]] || continue
      cp -a -- "$task" "$TRIAL_CB_HOME/config.d/cron.d/" || return 1
    done
  fi
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

# startup_failed CAUSE: the FAIL line of a serve startup, then the cause, which names the port and any holder.
startup_failed() {
  printf 'FAIL serve Startup: the server did not become ready on 127.0.0.1:%s\n' "$CHOSEN_PORT"
  printf 'CAUSE serve: %s\n' "$1"
}

# server_cgroup_dir PID: the cgroup directory, under CGROUP_ROOT, of a live process of the trial's own server. Under
# --scope the systemd-run process sits in the scope's cgroup, so its pid names the scope.
server_cgroup_dir() {
  local cgroup
  cgroup=$(sed -n 's/^0:://p' "/proc/$1/cgroup") || return 1
  [[ -n $cgroup ]] || return 1
  printf '%s%s' "$CGROUP_ROOT" "$cgroup"
}

# SCOPE_RESULT is the Result of the serve scope once its state is terminal. SCOPE_CAUSE says what
# serve_scope_result read.
SCOPE_RESULT=
SCOPE_CAUSE=

# serve_scope_result sets SCOPE_RESULT and SCOPE_CAUSE from the serve scope's ActiveState and Result. systemd moves a
# scope to a terminal state (failed or inactive) after its last process exits, so the call polls the state for at most
# SERVE_EXIT_WAIT_SECONDS. It fails when the state is still active at the bound or the read fails; it does not guess a
# result from an active scope.
serve_scope_result() {
  local started=$SECONDS props state
  SCOPE_RESULT=
  while :; do
    if ! props=$(systemctl --user show -p ActiveState -p Result "$SERVE_UNIT.scope"); then
      SCOPE_CAUSE="the state of scope $SERVE_UNIT.scope cannot be read"
      return 1
    fi
    state=$(sed -n 's/^ActiveState=//p' <<<"$props")
    if [[ $state == failed || $state == inactive ]]; then
      SCOPE_RESULT=$(sed -n 's/^Result=//p' <<<"$props")
      SCOPE_CAUSE="scope $SERVE_UNIT.scope ended with Result=$SCOPE_RESULT"
      return 0
    fi
    if (( SECONDS - started >= SERVE_EXIT_WAIT_SECONDS )); then
      SCOPE_CAUSE="scope $SERVE_UNIT.scope is still $state after $SERVE_EXIT_WAIT_SECONDS s"
      return 1
    fi
    sleep 0.1
  done
}

# server_memory_cap_killed succeeds when the serve scope ended by its memory cap: systemd records the scope's Result as
# oom-kill, the result of a scope whose OOM killer ended one of its processes. The scope stays loaded in the failed
# state until stop_serve_scope resets it, so a caller reads the result before the stop. A host-wide OOM kill of a scope
# process leaves the same result, and the trial does not tell the two apart. In --in-caller-scope mode the caller's
# scope holds the cap, so this reads nothing.
server_memory_cap_killed() {
  (( IN_CALLER_SCOPE == 0 )) || return 1
  serve_scope_result || return 1
  [[ $SCOPE_RESULT == oom-kill ]]
}

# server_is_gone follows the current PID file and succeeds when that server has exited or is a zombie.
server_is_gone() {
  local state
  if ! IFS= read -r SERVER_PID <"$SERVER_PID_FILE"; then
    printf 'trial server pid file cannot be read: %s\n' "$SERVER_PID_FILE" >&2
    return 2
  fi
  if [[ ! $SERVER_PID =~ ^[1-9][0-9]*$ ]]; then
    printf 'trial server pid file does not contain a positive pid: %s\n' "$SERVER_PID_FILE" >&2
    return 2
  fi
  state=$(process_state "$SERVER_PID")
  [[ -z $state || $state == *Z* ]]
}

# wait_server_gone succeeds once the trial server is gone, and it polls for at most SERVE_EXIT_WAIT_SECONDS. A server
# that has closed its listener can still be running for a moment while the kernel finishes its exit, so a failed runs
# read waits here before the read failure is reported.
wait_server_gone() {
  local started=$SECONDS
  local status
  while :; do
    if server_is_gone; then
      return 0
    else
      status=$?
      (( status == 1 )) || return 1
    fi
    if (( SECONDS - started >= SERVE_EXIT_WAIT_SECONDS )); then
      return 1
    fi
    sleep 0.1
  done
}

# report_server_exit prints the FAIL and CAUSE lines of a serve whose server exited before every run of its home had
# ended. A scope that ended by its memory cap is the memory-cap failure. Any other end is the RunWait failure, and its
# CAUSE names the scope's state, so a scope still active at the bound is never reported as a memory cap.
report_server_exit() {
  if server_memory_cap_killed; then
    memory_cap_failed
    return
  fi
  printf 'FAIL serve RunWait: the trial server stopped before every run of the trial home had ended\n'
  if (( IN_CALLER_SCOPE == 1 )); then
    printf 'CAUSE serve: server pid %s exited while the trial waited for its runs\n' "$SERVER_PID"
  else
    printf 'CAUSE serve: server pid %s exited while the trial waited for its runs; %s\n' "$SERVER_PID" "$SCOPE_CAUSE"
  fi
}

# memory_cap_failed: the FAIL and CAUSE lines of a serve whose memory cap ended the server. The kernel keeps no peak
# once a scope ends, and an OOM kill happens when the scope reaches its cap, so the peak named is the cap.
memory_cap_failed() {
  printf 'FAIL serve MemoryCap: the memory cap of scope %s ended the trial server pid %s by an OOM kill\n' \
    "$SERVE_UNIT.scope" "$SERVER_PID"
  printf 'CAUSE serve: MemoryMax=%s bytes, peak %s bytes (at the cap), scope %s\n' \
    "$SERVE_MEMORY_MAX_BYTES" "$SERVE_MEMORY_MAX_BYTES" "$SERVE_UNIT.scope"
}

# report_server_peak: prints the serve scope's memory peak and cap while the server still runs. A read that fails fails
# the side.
report_server_peak() {
  local peak max
  peak=$(cat -- "$SERVER_CGROUP_DIR/memory.peak") \
    || { printf 'FAIL serve MemoryPeak: the memory peak of scope %s cannot be read\n' "$SERVE_UNIT.scope"; return 1; }
  max=$(cat -- "$SERVER_CGROUP_DIR/memory.max") \
    || { printf 'FAIL serve MemoryPeak: the memory cap of scope %s cannot be read\n' "$SERVE_UNIT.scope"; return 1; }
  printf 'SERVE PASS MemoryPeak: scope=%s peak=%s max=%s\n' "$SERVE_UNIT.scope" "$peak" "$max"
}

# port_listeners prints the `ss` line of each listening socket on the trial's port, with its socket inode (-e). The
# lines name no owner, and the check does not ask ss for one: `ss -p` reads the fds of every process before it dumps
# the sockets, so a listener bound during that read is listed without its owner. Ownership comes from the fd tables
# of the trial's processes instead (server_tree_pids and unproven_holders).
port_listeners() {
  ss -H -ltne "sport = :$CHOSEN_PORT"
}

# listener_inode LINE prints the socket inode of one `ss -e` line. A line without an inode fails: no proof exists.
listener_inode() {
  local inode
  inode=$(sed -n 's/.* ino:\([0-9]*\).*/\1/p' <<<"$1")
  [[ -n $inode ]] || return 1
  printf '%s' "$inode"
}

# server_tree_pids prints the pid of the trial's server process and of each process it started, descendants included.
# pgrep -P exits 1 for a process without children, which is not an error.
server_tree_pids() {
  local -a queue=("$SERVER_PID")
  local pid children status
  while (( ${#queue[@]} > 0 )); do
    pid=${queue[0]}
    queue=("${queue[@]:1}")
    printf '%s\n' "$pid"
    children=$(pgrep -P "$pid")
    status=$?
    (( status == 0 || status == 1 )) || return 1
    queue+=($children)
  done
}

# unproven_holders LISTENERS TREE prints the CAUSE text (holder_cause) of each listening line whose socket no process
# of TREE holds, one per line. A process of TREE holds a socket when its fd table lists the socket's inode. Only the
# trial's own processes are read, so the proof admits nothing else. It fails when a line has no socket inode.
unproven_holders() {
  local listeners=$1 tree=$2 line inode pid proven
  while IFS= read -r line; do
    inode=$(listener_inode "$line") || return 1
    proven=0
    for pid in $tree; do
      if ls -l "/proc/$pid/fd" 2>/dev/null | grep -F "socket:[$inode]" >/dev/null; then
        proven=1
        break
      fi
    done
    if (( proven == 0 )); then
      holder_cause "$line" "$inode"
      printf '\n'
    fi
  done <<<"$listeners"
}

# holder_pid LINE INODE prints the pid that `ss -p -e` names for the socket inode of one `ss -e` line, or nothing when
# ss names no pid for it. It fails when ss cannot list the sockets. The pid names the holder in a CAUSE line and selects
# the holder's trial; no check of the trial's own tree reads it.
holder_pid() {
  local line=$1 inode=$2 named named_line pid=
  if ! named=$(ss -H -ltnpe "sport = :$CHOSEN_PORT"); then
    return 1
  fi
  while IFS= read -r named_line; do
    [[ $named_line =~ (^|[[:space:]])ino:${inode}([[:space:]]|$) ]] || continue
    pid=$(sed -n 's/.*pid=\([0-9]*\).*/\1/p' <<<"$named_line")
    break
  done <<<"$named"
  printf '%s' "$pid"
}

# holder_text LINE INODE describes the process that holds the socket of one `ss -e` line: its pid, command line and
# working directory. When `ss -p` names no pid, the line describes the listener and its inode.
holder_text() {
  local line=$1 inode=$2 pid
  if ! pid=$(holder_pid "$line" "$inode"); then
    printf 'a listener ss could not name (%s)' "$line"
    return 0
  fi
  if [[ -z $pid ]]; then
    printf 'a process this user cannot name (%s)' "$line"
    return 0
  fi
  printf 'pid %s %s (cwd %s)' "$pid" "$({ tr '\0' ' ' <"/proc/$pid/cmdline"; } 2>/dev/null)" \
    "$(readlink "/proc/$pid/cwd" 2>/dev/null)"
}

# holder_trial_pid PID prints the pid of the trial that started PID: each trial gives its server
# CHARLIEBOT_RELEASE_TRIAL_PID, its own pid, in the server's environment. It prints nothing for a process that no trial
# started, or whose environment this user cannot read.
holder_trial_pid() {
  { tr '\0' '\n' <"/proc/$1/environ"; } 2>/dev/null | sed -n 's/^CHARLIEBOT_RELEASE_TRIAL_PID=//p'
}

# trial_holder_text PID names a trial's server that holds the port: its pid, its binary, its scope and its working
# directory. The scope is the leaf of the process's cgroup: the serve unit of a scoped trial, or the caller's cgroup.
trial_holder_text() {
  local pid=$1 cmdline cgroup
  cmdline=$({ tr '\0' ' ' <"/proc/$pid/cmdline"; } 2>/dev/null)
  cgroup=$(sed -n 's/^0:://p' "/proc/$pid/cgroup" 2>/dev/null)
  printf 'pid %s, binary %s, scope %s (cwd %s)' "$pid" "${cmdline%% *}" "${cgroup##*/}" \
    "$(readlink "/proc/$pid/cwd" 2>/dev/null)"
}

# holder_cause LINE INODE prints the CAUSE text of one listener that no process of this trial holds. A holder that a
# trial started names that trial's pid. When the trial has ended, the holder is a teardown defect: a trial stops its
# server before it ends, so a server that still holds the port outlived its trial. When the trial still runs, the holder
# is a side that serves at the same time. A holder no trial started keeps the generic text.
holder_cause() {
  local line=$1 inode=$2 pid trial holder trial_state
  pid=$(holder_pid "$line" "$inode") || pid=
  trial=
  if [[ -n $pid ]]; then
    trial=$(holder_trial_pid "$pid")
  fi
  if [[ -z $trial ]]; then
    printf '%s' "port $CHOSEN_PORT is held by another process, so this trial's server cannot bind it: " \
      "$(holder_text "$line" "$inode")"
    return 0
  fi
  holder=$(trial_holder_text "$pid")
  trial_state=$(process_state "$trial")
  if [[ -n $trial_state && $trial_state != *Z* ]]; then
    printf '%s' "port $CHOSEN_PORT is held by another process, so this trial's server cannot bind it: $holder, " \
      "the server of trial pid $trial, which still runs: a side serving at the same time holds the port"
  else
    printf '%s' "teardown defect: port $CHOSEN_PORT is held by $holder, " \
      "the server of trial pid $trial, which has ended and left its server running"
  fi
}

start_server() {
  local server_number=$(( ${#SERVER_PIDS[@]} + 1 ))
  SERVER_LOG="$TREE/server-$server_number.log"
  SERVER_LOGS+=("$SERVER_LOG")
  # The server carries this trial's pid in CHARLIEBOT_RELEASE_TRIAL_PID. A later trial that finds the server on its port
  # reads the pid, and holder_cause names the trial that started the holder and whether that trial has ended.
  if (( IN_CALLER_SCOPE == 1 )); then
    # The server stays in the caller's cgroup, whose memory limit the caller's scope already sets.
    CHARLIEBOT_RELEASE_TRIAL_PID=$$ "$BINARY" >>"$SERVER_LOG" 2>&1 &
  else
    # The server runs inside a scope whose MemoryMax keeps a measurement beside the live server from pressing the host
    # into swap.
    SERVE_UNIT_STARTED=1
    CHARLIEBOT_RELEASE_TRIAL_PID=$$ systemd-run --user --scope -p "MemoryMax=$SERVE_MEMORY_MAX" --unit="$SERVE_UNIT" \
      "$BINARY" >>"$SERVER_LOG" 2>&1 &
  fi
  SERVER_PID=$!
  SERVER_PIDS+=("$SERVER_PID")
  printf '%s\n' "$SERVER_PID" >"$SERVER_PID_FILE"
  # Each poll reads the port's listeners, then asks whether a process this trial started holds each one. A listener that
  # none of them holds is another process's: this server's bind fails behind it, so the side fails now, with the holder.
  for _ in $(seq 1 "$SERVE_READY_POLLS"); do
    local listeners tree holders summary held status state
    if ! listeners=$(port_listeners); then
      startup_failed "ss cannot list the listeners on 127.0.0.1:$CHOSEN_PORT"
      return 1
    fi
    if [[ -n $listeners ]]; then
      if ! tree=$(server_tree_pids); then
        summary=$(paste -sd ';' <<<"$listeners")
        startup_failed "port $CHOSEN_PORT has unproven listener $summary; trial process list failed"
        return 1
      fi
      if ! holders=$(unproven_holders "$listeners" "$tree"); then
        summary=$(paste -sd ';' <<<"$listeners")
        startup_failed "port $CHOSEN_PORT has a listener without a socket inode: $summary"
        return 1
      fi
      if [[ -n $holders ]]; then
        startup_failed "$(paste -sd ';' <<<"$holders")"
        return 1
      fi
      # Every listener on the port is held by this trial's server, so the answer is this server's.
      status=$(http_get /api/auth/status "$AUTH_STATUS_FILE")
      [[ $status == 200 ]] && grep -qE '[[:space:]]server_ready[[:space:]]' "$SERVER_LOG" && return 0
    fi
    state=$(process_state "$SERVER_PID")
    if [[ -z $state || $state == *Z* ]]; then
      if server_memory_cap_killed; then
        memory_cap_failed
        return 1
      fi
      if ! listeners=$(port_listeners); then
        startup_failed "the trial server exited before it became ready; ss cannot list the listeners"
        return 1
      fi
      if [[ -z $listeners ]]; then
        startup_failed "the trial server exited before it became ready; port $CHOSEN_PORT has no listener"
        return 1
      fi
      if ! tree=$(server_tree_pids); then
        summary=$(paste -sd ';' <<<"$listeners")
        startup_failed "server exited; port $CHOSEN_PORT has unproven listener $summary (process list failed)"
        return 1
      fi
      if ! holders=$(unproven_holders "$listeners" "$tree"); then
        summary=$(paste -sd ';' <<<"$listeners")
        startup_failed "server exited before ready; port $CHOSEN_PORT has listener without socket inode: $summary"
        return 1
      fi
      if [[ -z $holders ]]; then
        held="port $CHOSEN_PORT is held by a process of the trial tree"
        startup_failed "the trial server exited before it became ready; $held"
        return 1
      fi
      startup_failed "the trial server exited before it became ready; $(paste -sd ';' <<<"$holders")"
      return 1
    fi
    sleep 0.25
  done
  startup_failed "this trial's server did not answer on 127.0.0.1:$CHOSEN_PORT within $SERVE_READY_POLLS polls"
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
# serve_home_has_active_run sets RUN_ACTIVE to 1 while a run of the trial home is queued or running, else 0, and sets
# RUN_CAUSE to the session that holds the run or to the read that failed.
RUN_ACTIVE=0
RUN_CAUSE=
# RUN_WAIT_STARTED is the SECONDS value when the run wait began. Every read of the wait shares its bound.
RUN_WAIT_STARTED=0

# read_session_runs SESSION_ID CURSOR: reads one runs page of a trial-home session into serve-runs.json. A read the
# server does not answer within http_get's limit is retried while the server runs: a loaded host, or a lazy fact scan
# of a large log, can hold a read past any fixed limit. The judges are the server's exit and SERVE_WAIT_RUNS_SECONDS
# from the start of the wait. Any other status fails the read at once. It returns 1 with RUN_CAUSE set.
read_session_runs() {
  local session_id=$1 cursor=$2 status state
  while :; do
    status=$(http_get "/api/sessions/$session_id/runs?order=asc&limit=500${cursor:+&cursor=$cursor}" \
      "$HTTP_ROOT/serve-runs.json")
    case $status in
      200) return 0 ;;
      000) ;;
      *)
        RUN_CAUSE="server pid $SERVER_PID gave status $status for the runs of session $session_id"
        return 1
        ;;
    esac
    state=$(process_state "$SERVER_PID")
    if [[ -z $state || $state == *Z* ]]; then
      RUN_CAUSE="server pid $SERVER_PID exited before it answered the runs of session $session_id"
      return 1
    fi
    if (( SECONDS - RUN_WAIT_STARTED >= SERVE_WAIT_RUNS_SECONDS )); then
      RUN_CAUSE="server pid $SERVER_PID did not answer the runs of session $session_id within ${SERVE_WAIT_RUNS_SECONDS}s"
      return 1
    fi
    sleep 1
  done
}

# serve_home_has_active_run reads the runs of every trial-home session. A stopped, withheld or input-blocked run
# consumes no server process, so the active set is queued and running only. Every page of a session's runs is read,
# because the newest run is on the last page. It returns 1 when a page cannot be read, and read_session_runs names why.
serve_home_has_active_run() {
  local trial_session_id trial_session_ids cursor page_output
  local -a page=()
  RUN_ACTIVE=0
  if [[ ! -d $SESSIONS_DIR ]]; then
    return 0
  fi
  if ! trial_session_ids=$("$TRIAL_CLI" release-helper session-ids "$SESSIONS_DIR"); then
    RUN_CAUSE="the sessions of the trial home cannot be listed: $SESSIONS_DIR"
    return 1
  fi
  while IFS= read -r trial_session_id; do
    [[ -n $trial_session_id ]] || continue
    cursor=
    while :; do
      read_session_runs "$trial_session_id" "$cursor" || return 1
      if ! page_output=$("$TRIAL_CLI" release-helper run-page "$HTTP_ROOT/serve-runs.json"); then
        RUN_CAUSE="the runs page of session $trial_session_id does not parse"
        return 1
      fi
      mapfile -t page <<<"$page_output"
      if [[ ${page[0]} == 1 ]]; then
        RUN_ACTIVE=1
        RUN_CAUSE="session $trial_session_id has a queued or running run"
        return 0
      fi
      cursor=${page[1]:-}
      [[ -n $cursor ]] || break
    done
  done <<<"$trial_session_ids"
  return 0
}

stop_serve_scope() {
  # The scope normally ends with the server process; the stop clears a unit that outlived an unclean server exit, and a
  # scope whose server died abnormally is loaded in the failed state, which reset-failed unloads. systemd keeps a scope
  # active while any process of its cgroup lives, so a daemonized child (such as tmux) keeps the scope up after the
  # server exits. systemctl reads a bare name as a .service, so every call names the scope with its suffix. A stop or
  # reset-failed can unload the unit and its ControlGroup, so the cgroup is read first, and any PID that stays in it is
  # terminated too.
  local unit=$1
  local active cg procs_file pid state remaining fail=0
  active=$(systemctl --user is-active "$unit.scope" 2>/dev/null)
  cg=$(systemctl --user show -p ControlGroup --value "$unit.scope") || {
    printf 'trial serve scope cgroup read failed: %s\n' "$unit" >&2
    return 1
  }
  if [[ $active == active ]] && ! systemctl --user stop "$unit.scope"; then
    printf 'trial serve scope stop failed: %s\n' "$unit" >&2
    fail=1
  fi
  if [[ $active == failed ]] && ! systemctl --user reset-failed "$unit.scope" >/dev/null 2>&1; then
    printf 'trial serve scope reset-failed failed: %s\n' "$unit" >&2
    fail=1
  fi
  procs_file="${CGROUP_ROOT}${cg}/cgroup.procs"
  if [[ -n $cg && -r $procs_file ]]; then
    while IFS= read -r pid; do
      [[ $pid =~ ^[0-9]+$ ]] || continue
      state=$(process_state "$pid")
      if [[ -n $state && $state != *Z* ]]; then
        if ! kill -TERM "$pid" 2>/dev/null; then
          state=$(process_state "$pid")
          [[ -z $state || $state == *Z* ]] || fail=1
        fi
      fi
    done <"$procs_file"
    for _ in $(seq 1 30); do
      remaining=$(cat "$procs_file" 2>/dev/null || true)
      [[ -z $remaining ]] && break
      sleep 0.1
    done
    while IFS= read -r pid; do
      [[ $pid =~ ^[0-9]+$ ]] || continue
      state=$(process_state "$pid")
      if [[ -n $state && $state != *Z* ]]; then
        if ! kill -KILL "$pid" 2>/dev/null; then
          state=$(process_state "$pid")
          [[ -z $state || $state == *Z* ]] || fail=1
        fi
      fi
    done <"$procs_file"
    for _ in $(seq 1 10); do
      remaining=$(cat "$procs_file" 2>/dev/null || true)
      [[ -z $remaining ]] && break
      sleep 0.1
    done
    remaining=$(cat "$procs_file" 2>/dev/null || true)
    if [[ -n $remaining ]]; then
      printf 'trial serve scope cgroup remains populated: %s (%s)\n' "$unit" "$remaining" >&2
      fail=1
    fi
  fi
  printf 'SERVE scope stop unit=%s state=%s\n' "$unit" "${active:-unreachable}"
  return "$fail"
}

# cron_disabled_readback DISABLED: reads the scheduled task list of the started server back and passes only when every
# listed task reads enabled false and each task in DISABLED (one name per line, from disable-cron-tasks) is listed. The
# server seeds nothing enabled at its start, so a listed task that reads enabled means a task would fire in the trial.
cron_disabled_readback() {
  local disabled=$1 status verdict tasks_file="$HTTP_ROOT/cron-tasks.json"
  status=$(http_get "$CRON_TASKS_PATH" "$tasks_file")
  if [[ $status != 200 ]]; then
    printf 'FAIL serve CronDisabled: the server gave status %s for its scheduled task list\n' "$status"
    return 1
  fi
  if ! verdict=$(jq -r --arg disabled "$disabled" '
    [.[] | select(.enabled != false) | (.name // "(unnamed)")] as $enabled
    | [$disabled | split("\n")[] | select(length > 0)] as $wanted
    | [.[].name] as $listed
    | [$wanted[] | select(. as $name | $listed | index($name) | not)] as $missing
    | if ($enabled | length) == 0 and ($missing | length) == 0 then "ok \(length)"
      else "enabled: \($enabled | join(" ")); not listed: \($missing | join(" "))" end' "$tasks_file"); then
    printf 'FAIL serve CronDisabled: the scheduled task list did not parse: %s\n' "$tasks_file"
    return 1
  fi
  if [[ $verdict != ok\ * ]]; then
    printf 'FAIL serve CronDisabled: the started server does not read every cron task as disabled: %s\n' "$verdict"
    return 1
  fi
  printf 'SERVE PASS CronDisabled readback: %s listed tasks read disabled\n' "${verdict#ok }"
}

run_serve_mode() {
  # One caller command. The exit code is the command's, demoted to 1 by a foreign cgroup removal (sweep-check) or any
  # cleanup failure.
  local cmd_rc=0 sweep_failed=0 cleanup_failed=0
  local wait_started server_pid server_state server_status state stray_pids disabled_tasks
  SERVE_UNIT="charliebot-release-trial-serve-$(date -u +%Y%m%dT%H%M%SZ)-$$"
  SERVER_PID_FILE="$TREE/server.pid"
  if [[ -n $SEED_HOME ]]; then
    if ! copy_seed_sessions; then
      printf 'FAIL serve Seed: the sessions of %s did not copy into %s\n' "$SEED_HOME" "$SESSIONS_DIR"
      return 1
    fi
    printf 'SERVE seed from=%s into=%s\n' "$SEED_HOME" "$SESSIONS_DIR"
  fi
  # The trial fires no scheduled task. The server seeds the repo's default tasks at its start, so the trial seeds them
  # first and disables every cron task of its home before the server starts.
  if ! disabled_tasks=$("$TRIAL_CLI" release-helper disable-cron-tasks "$TRIAL_CB_HOME" "$REPO_ROOT"); then
    printf 'FAIL serve CronDisabled: the cron tasks of %s are not all disabled before the server starts\n' \
      "$TRIAL_CB_HOME"
    return 1
  fi
  printf 'SERVE PASS CronDisabled: %s\n' "$(tr '\n' ' ' <<<"$disabled_tasks")"
  # start_server prints the FAIL and CAUSE lines; a side that did not start runs no command.
  start_server || return 1
  if (( IN_CALLER_SCOPE == 1 )); then
    printf 'SERVE PASS Startup url=http://127.0.0.1:%s pid=%s scope=caller\n' "$CHOSEN_PORT" "$SERVER_PID"
  else
    printf 'SERVE PASS Startup url=http://127.0.0.1:%s pid=%s unit=%s memory_max=%s\n' "$CHOSEN_PORT" "$SERVER_PID" \
      "$SERVE_UNIT" "$SERVE_MEMORY_MAX_BYTES"
  fi
  cron_disabled_readback "$disabled_tasks" || return 1
  check_sweep_names 'start 1' || sweep_failed=1

  export CHARLIEBOT_SERVE_URL="http://127.0.0.1:$CHOSEN_PORT"
  export CHARLIEBOT_SERVE_AUTH_HEADER="Authorization: Bearer $ACCESS_KEY"
  export CHARLIEBOT_SERVE_PID="$SERVER_PID"
  export CHARLIEBOT_SERVE_PID_FILE="$SERVER_PID_FILE"
  printf 'SERVE command start: %s\n' "${COMMAND[*]}"
  "${COMMAND[@]}"
  cmd_rc=$?
  unset CHARLIEBOT_SERVE_URL CHARLIEBOT_SERVE_AUTH_HEADER CHARLIEBOT_SERVE_PID CHARLIEBOT_SERVE_PID_FILE
  printf 'SERVE command exit=%s\n' "$cmd_rc"

  # The wait stops the moment the server is gone, or a runs page cannot be read, and it runs out at the bound. A runs
  # page that cannot be read while the server still runs may be the read of a server that is exiting, so the exit is
  # awaited before that read is reported.
  wait_started=$SECONDS
  RUN_WAIT_STARTED=$wait_started
  while :; do
    if server_is_gone; then
      cleanup_failed=1
      report_server_exit
      break
    else
      server_status=$?
      if (( server_status != 1 )); then
        cleanup_failed=1
        printf 'FAIL serve RunWait: the current trial server pid cannot be read\n'
        break
      fi
    fi
    if ! serve_home_has_active_run; then
      # A read that fails because the server exited is classified by the first check of the next pass, which names a
      # memory cap when the scope's OOM killer ended the server.
      if server_is_gone; then
        continue
      else
        server_status=$?
        if (( server_status != 1 )); then
          cleanup_failed=1
          printf 'FAIL serve RunWait: the current trial server pid cannot be read\n'
          break
        fi
      fi
      server_state=$(process_state "$SERVER_PID")
      cleanup_failed=1
      if (( IN_CALLER_SCOPE == 0 )) && wait_server_gone; then
        report_server_exit
      else
        printf 'FAIL serve RunWait: a run of the trial home could not be read\n'
        printf 'CAUSE serve: %s\n' "$RUN_CAUSE"
      fi
      break
    fi
    if (( RUN_ACTIVE == 0 )); then
      break
    fi
    if (( SECONDS - wait_started >= SERVE_WAIT_RUNS_SECONDS )); then
      cleanup_failed=1
      printf 'FAIL serve RunWait: a trial-home run is still queued or running after %s s\n' "$SERVE_WAIT_RUNS_SECONDS"
      printf 'CAUSE serve: %s\n' "$RUN_CAUSE"
      break
    fi
    sleep 1
  done
  if (( cleanup_failed == 0 )); then
    printf 'SERVE PASS RunWait: every run of the trial home has ended\n'
  fi
  # The peak is read while the server still runs, whatever the wait's result: a wait that ran out still leaves its peak.
  # A cgroup read that fails is a cleanup failure, so the trial ends through its cleanup before it exits.
  server_state=$(process_state "$SERVER_PID")
  if (( IN_CALLER_SCOPE == 0 )) && [[ -n $server_state && $server_state != *Z* ]]; then
    if SERVER_CGROUP_DIR=$(server_cgroup_dir "$SERVER_PID"); then
      report_server_peak || cleanup_failed=1
    else
      printf 'FAIL serve Cgroup: the cgroup of the trial server pid %s is not readable\n' "$SERVER_PID"
      cleanup_failed=1
    fi
  fi

  if ! stop_current_server 6; then
    cleanup_failed=1
  fi
  if ! cleanup_owned_cgroups; then
    cleanup_failed=1
  fi
  if ! cleanup_trial_tmux_servers; then
    cleanup_failed=1
  fi
  if (( IN_CALLER_SCOPE == 0 && SERVE_UNIT_STARTED == 1 )) && ! stop_serve_scope "$SERVE_UNIT"; then
    cleanup_failed=1
  fi
  for server_pid in "${SERVER_PIDS[@]}"; do
    state=$(process_state "$server_pid")
    if [[ -n $state && $state != *Z* ]]; then
      cleanup_failed=1
      printf 'FAIL serve ChildProcess: the server process %s remains\n' "$server_pid"
    fi
  done
  stray_pids=$({ pgrep -f "$TREE"; pgrep -f "$TMP_ROOT"; } || true)
  if [[ -n $stray_pids ]]; then
    cleanup_failed=1
    printf 'FAIL serve ChildProcess: processes of the trial tree or temp root remain: %s\n' "$stray_pids"
  else
    printf 'SERVE PASS ChildProcess: no process of the trial tree remains\n'
  fi
  check_sweep_names 'serve' || sweep_failed=1

  if ! capture_evidence; then
    cleanup_failed=1
    printf 'evidence capture failed: %s\n' "$EVIDENCE_ROOT" >&2
  fi
  if ! remove_trial_paths; then
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
