#!/usr/bin/env bash
# Run the release trial on a server binary with its paired CLI in the same directory.
#
#   scripts/release/trial.sh <full main commit> <server binary>
#   scripts/release/trial.sh --serve [--tree-root <dir>] [--seed-home <dir>] [--in-caller-scope] <commit> <server binary> -- <command> [args...]
#
# The commit is extracted from this repository into a trial tree with its own home, private tmux server and a port in
# 18650-18669. The trial starts the binary, then checks the startup, the manager turn, the session stream, the
# delegation and the takeover, and it shuts down cleanly. The delegation repository is a clone of a local bare
# repository, and the server's git calls run behind a wrapper that waits SLOW_GIT_DELAY_SECONDS first, so a binary with
# sub-second git timeouts fails here instead of on a production repository.
#
# The --serve mode owns the same isolated setup (tree, home, port range, private tmux server, credentials, cleanup and
# sweep-check) but replaces the release checks with one caller command. It starts the binary without the slow-git
# wrapper, whose per-call delay would distort a measurement, inside a systemd user scope with MemoryMax=2G, so a
# measurement cannot push the live server into swap, and runs <command> with CHARLIEBOT_SERVE_URL,
# CHARLIEBOT_SERVE_AUTH_HEADER (one line `Authorization: Bearer <key>`) and CHARLIEBOT_SERVE_PID in its environment.
# After the command ends it waits until every run of the trial home has ended, stops the server, asserts that no
# process of the trial tree remains, cleans up and runs sweep-check on the server log. This host gives an
# unprivileged process no read-only view of /sys/fs/cgroup, so the preflight refuses (exit 2, nothing started) a commit
# whose diff against its merge base with origin/main touches the cgroup code
# (rust/crates/runtime/agents/src/transport/cgroup.rs, rust/crates/infra/core/src/cgroup.rs or
# rust/crates/runtime/agents/src/agent_environment.rs): --serve runs only binaries whose cgroup code equals main's.
# A serve server runs with session cgroups off (session_memory_max_mb 0) and starts no agent, so it needs no writable
# agents slice; its tmux socket must keep its path under 108 bytes, the sun_path size with the final NUL.
#
# --serve options: --tree-root <dir> builds the tree as <dir>/trial-<first 12 hex of the commit>, so runs of two
# commits under one root share no tree; --seed-home <dir> copies each session directory of <dir> into the trial home
# before the server starts, and the trial keeps its own config.yaml and credentials.yaml; --in-caller-scope keeps the
# server in the caller's cgroup, which must already cap memory, and skips the scope probe and the scope start and stop.
# Every check reads the release under test through the charliebot beside <server binary>: its
# `charliebot release-helper` verbs parse the responses, the logs and the sessions this script gathers.
#
# Exit: 0 every check passed; 1 a check or the cleanup failed; 2 the preflight failed, nothing started.
# Exit (--serve): the command's exit code; 1 a foreign cgroup removal, a remaining child process or a cleanup failure;
# 2 the preflight failed, nothing started.
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
readonly CGROUP_BASE="/sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/charliebot.slice/charliebot-agents.slice"
readonly PORT_MIN=18650
readonly PORT_MAX=18669
# The trial server's git calls run at production-repository speed. A wrapper first on that server's PATH only (see
# SLOW_GIT_BIN) waits this long before every git invocation. The release 8fe3e5bee of 2026-10-27 shipped a build whose
# git write timeout (worktree add) and per-attempt remote timeout (ls-remote, fetch) were 0.3 s in place of 60 s and 120 s, so
# every live `charliebot delegate --repo` failed with `git worktree add timed out after 0.3s`. 1 s exceeds that defect and stays
# below every production timeout, so a sub-second binary fails here on any disk speed.
readonly SLOW_GIT_DELAY_SECONDS=1
# start_server polls the server's auth status at 0.25 s intervals this many times before it fails the trial. A release
# server answers within 30 s. A serve server walks every session of its home at boot before it binds: on the 7,160-session
# mock home that took about a minute, so a serve trial waits 10 minutes.
readonly RELEASE_READY_POLLS=120
readonly SERVE_READY_POLLS=2400

preflight_fail() {
  printf 'preflight failed: %s\n' "$1" >&2
  exit 2
}

yaml_scalar() {
  local section=$1
  local field=$2
  local file=$3
  awk -v wanted_section="$section" -v wanted_field="$field" '
    /^[^[:space:]#][^:]*:/ {
      section_name = $0
      sub(/:.*/, "", section_name)
      in_section = section_name == wanted_section
      next
    }
    in_section && /^[[:space:]]+[A-Za-z0-9_-]+:[[:space:]]*/ {
      line = $0
      sub(/^[[:space:]]+/, "", line)
      field_name = line
      sub(/:.*/, "", field_name)
      if (field_name == wanted_field) {
        sub(/^[^:]*:[[:space:]]*/, "", line)
        sub(/[[:space:]]+#.*$/, "", line)
        if (line == "" || line == "null" || line == "~" || line == "\"\"" || line == "\047\047") {
          exit 1
        }
        print line
        found = 1
        exit
      }
    }
    END {
      if (!found) {
        exit 1
      }
    }
  ' "$file"
}

unquote_yaml_scalar() {
  local value=$1
  case "$value" in
    \"*\") value=${value:1:${#value}-2} ;;
    \'*\') value=${value:1:${#value}-2} ;;
  esac
  printf '%s' "$value"
}

SERVE_MODE=0
IN_CALLER_SCOPE=0
TREE_ROOT=
SEED_HOME=
BINARY=
COMMAND=()
if [[ ${1:-} == --serve ]]; then
  SERVE_MODE=1
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
elif [[ $# -eq 2 && $1 =~ ^[[:xdigit:]]{40}$ && -n $2 ]]; then
  COMMIT=${1,,}
  BINARY=$2
else
  preflight_fail 'arguments must be a full 40-character hexadecimal sha followed by the server binary to run, or --serve [options] <commit> <server binary> -- <command> [args...]'
fi
readonly SERVE_MODE
readonly IN_CALLER_SCOPE
readonly TREE_ROOT
readonly SEED_HOME
readonly COMMIT
readonly BINARY
readonly COMMAND
readonly TRIAL_CLI="$(dirname "$BINARY")/charliebot"
REAL_GIT=

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

if (( SERVE_MODE == 1 )); then
  # --serve runs a measurement beside the live server, and this host gives an unprivileged process no read-only view
  # of /sys/fs/cgroup (an unprivileged user may not create the mount namespace a read-only bind needs). The guard is
  # therefore the sweep-check after the run plus this refusal: a binary whose cgroup code differs from main's never
  # starts, which keeps the risk equal to a release trial's.
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

# The wrapper must exec the distribution binary directly: a PATH-resolved git can be a shell wrapper (CharlieBot puts its
# stash guard first on an agent's PATH) that looks git up on PATH again and would find this wrapper, recursing forever.
if [[ ! -x /usr/bin/git ]]; then
  preflight_fail 'the system git /usr/bin/git is executable for the slow-git wrapper'
fi
REAL_GIT=/usr/bin/git

# The trial's backend entry comes from the live config: the first option with an api_base, so the trial runs the
# operator's own model endpoint and the repository names no host and no credential. Its credential is optional. When
# the entry names one, the trial copies that section's key from the live credentials into the trial home and redacts
# it from saved logs; when it names none, the endpoint needs no client-side key, so the trial home's backend entry has
# no credential field and its credentials file carries no backend section.
LIVE_BACKEND="$("$TRIAL_CLI" release-helper backend-option "$LIVE_HOME/config.yaml")" \
  || preflight_fail "the live config has a backends.options entry with api_base"
TRIAL_API_BASE="$(sed -n 1p <<<"$LIVE_BACKEND")"
TRIAL_CREDENTIAL="$(sed -n 2p <<<"$LIVE_BACKEND")"
BACKEND_KEY=
if [[ -n $TRIAL_CREDENTIAL ]]; then
  if ! BACKEND_KEY=$("$TRIAL_CLI" release-helper credential-key "$LIVE_HOME/credentials.yaml" "$TRIAL_CREDENTIAL"); then
    preflight_fail "the live credentials file holds api_key for $TRIAL_CREDENTIAL"
  fi
fi
readonly TRIAL_API_BASE TRIAL_CREDENTIAL

# A serve run starts no agent, so its server needs no writable agents slice.
if (( SERVE_MODE == 0 )) && [[ ! -d $CGROUP_BASE || ! -w $CGROUP_BASE ]]; then
  preflight_fail "the agents' slice cgroup directory is writable: $CGROUP_BASE"
fi

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
if (( SERVE_MODE == 1 )); then
  # tmux keeps each server's socket in $TMUX_TMPDIR/tmux-<uid>/, and sun_path holds 108 bytes with the final NUL.
  TMUX_SOCKET_PATH="$TMUX_ROOT/tmux-$(id -u)/charliebot"
  if (( ${#TMUX_SOCKET_PATH} >= 108 )); then
    preflight_fail "the tmux socket path stays under 108 bytes, the sun_path size with its final NUL: $TMUX_SOCKET_PATH (${#TMUX_SOCKET_PATH} bytes)"
  fi
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

LIVE_ACCESS_KEY=
if LIVE_SCALAR=$(yaml_scalar charliebot access_key "$LIVE_HOME/credentials.yaml" 2>/dev/null); then
  LIVE_ACCESS_KEY=$(unquote_yaml_scalar "$LIVE_SCALAR")
fi

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
readonly SYNTH_REPO="$WORKSPACE/trial-repo"
# The delegation repository is a clone of a local bare repository, so its origin is a real remote: the server's base-branch
# resolution runs a remote probe (ls-remote) against it and then creates the worktree on top of that probe. The seed and the
# bare origin stay outside the configured workspace directory, which the server lists for repos.
readonly ORIGIN_SEED="$TREE/origin-seed"
readonly ORIGIN_REPO="$TREE/origin/trial-repo.git"
readonly SLOW_GIT_BIN="$TREE/slow-git-bin"
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

# A session memory limit of 0 turns session cgroups off. A serve server starts no agent, so it runs with them off.
if (( SERVE_MODE == 1 )); then
  SESSION_MEMORY_MAX_MB=0
else
  SESSION_MEMORY_MAX_MB=1024
fi
cat >"$TRIAL_CB_HOME/config.yaml" <<EOF
server:
  host: 127.0.0.1
  port: $CHOSEN_PORT
  subprocess_buffer_limit_mb: 1024
  session_memory_max_mb: $SESSION_MEMORY_MAX_MB
  session_swap_max_mb: 0
  session_tasks_max: 256
  min_free_disk_gib: 0
paths:
  workspace_dirs:
    - $WORKSPACE
  worktree_dir: $WORKTREE_ROOT
backends:
  pool:
    - charlie-code-release-trial
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

# The approval policy table is required host configuration; the trial's synthetic home carries a stand-in table.
mkdir -p "$TRIAL_CB_HOME/config.d/approval.d"
cat >"$TRIAL_CB_HOME/config.d/approval.d/policy.yaml" <<'EOF'
consequence_tests:
  - {id: irreversible, rank: 1, text: "Deletes data with no backup, or sends what cannot be recalled."}
  - {id: chao-name, rank: 2, text: "A message or publication in Chao's name reaches other people."}
  - {id: money, rank: 3, text: "Spends money beyond a budget that Chao set."}
  - {id: authority, rank: 4, text: "Widens what agents may do."}
  - {id: rule-named, rank: 4, text: "A rule in force names the user's explicit approval for this step."}
  - {id: reading, rank: 5, text: "Picks a reading for work over about one worker-day."}
standing_policies:
  - {id: release-restart, scope: "Release and restart the CharlieBot checkout.", cap_per_pt_day: null, expires: 2099-01-01}
enable_ask_tier_refusal: false
EOF

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
export UV_CACHE_DIR="$TREE/uv-cache"
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

# The wrapper waits SLOW_GIT_DELAY_SECONDS before every git call. start_server puts it first on the server's PATH only; the
# trial script's own git calls keep the real git and stay fast. --serve starts the binary without it, because the per-call
# delay would distort a measurement.
if (( SERVE_MODE == 0 )); then
  if ! mkdir -m 700 "$SLOW_GIT_BIN"; then
    printf 'trial setup failed: create the slow-git wrapper directory %s\n' "$SLOW_GIT_BIN" >&2
    exit 1
  fi
  cat >"$SLOW_GIT_BIN/git" <<EOF
#!/usr/bin/env bash
# trial.sh: every git call of the trial server waits $SLOW_GIT_DELAY_SECONDS s first, so a binary whose git timeouts are
# shorter than a production repository's git runs fails the delegation check instead of passing on a tiny local repository.
sleep $SLOW_GIT_DELAY_SECONDS
exec $(printf '%q' "$REAL_GIT") "\$@"
EOF
  if ! chmod 700 "$SLOW_GIT_BIN/git"; then
    printf 'trial setup failed: set the slow-git wrapper mode\n' >&2
    exit 1
  fi
fi

mkdir -p "$ORIGIN_SEED" "$TREE/origin"
git -C "$ORIGIN_SEED" init --initial-branch=main >"$TREE/synthetic-git.log" 2>&1
git_status=$?
if (( git_status != 0 )); then
  printf 'trial setup failed: initialize the synthetic repository seed\n' >&2
  exit 1
fi
printf 'before\n' >"$ORIGIN_SEED/trial.txt"
git -C "$ORIGIN_SEED" add trial.txt >>"$TREE/synthetic-git.log" 2>&1
git -C "$ORIGIN_SEED" commit -m 'Create synthetic trial file' >>"$TREE/synthetic-git.log" 2>&1
git_status=$?
if (( git_status != 0 )); then
  printf 'trial setup failed: commit the synthetic repository baseline\n' >&2
  exit 1
fi
git clone --bare "$ORIGIN_SEED" "$ORIGIN_REPO" >>"$TREE/synthetic-git.log" 2>&1
git_status=$?
if (( git_status != 0 )); then
  printf 'trial setup failed: create the synthetic origin bare repository\n' >&2
  exit 1
fi
git clone "$ORIGIN_REPO" "$SYNTH_REPO" >>"$TREE/synthetic-git.log" 2>&1
git_status=$?
if (( git_status != 0 )); then
  printf 'trial setup failed: clone the delegation repository from its origin\n' >&2
  exit 1
fi

cat >"$TREE/delegate-task.md" <<EOF
## Goal
Change trial.txt to contain worker completed.

## Source Files
- $SYNTH_REPO/trial.txt

## Required Behavior
Write worker completed followed by one newline to trial.txt.
Commit the one changed file in the worker worktree.

## Acceptance Tests
Verify trial.txt contains worker completed.
Verify the commit changes only trial.txt.

## Reviewer Checklist
Confirm that the worker made the commit in its worktree.
Confirm that the worktree is clean.

## Out of Scope
Change no other file.
EOF

EVIDENCE_ROOT="$EVIDENCE_PARENT/trial-$(date -u +%Y%m%dT%H%M%S.%NZ)"
if ! mkdir -m 700 -p "$EVIDENCE_ROOT"; then
  printf 'trial setup failed: create the evidence directory %s\n' "$EVIDENCE_ROOT" >&2
  exit 1
fi
chmod 700 "$EVIDENCE_ROOT"
EVIDENCE_CAPTURED=0

OVERALL_FAIL=0
LIVE_BEFORE_OK=0
LIVE_AFTER_OK=0
LIVE_AFTER_RECORDED=0
CLEANUP_DONE=0
SERVER_PID=
SERVER_PIDS=()
OWNED_CGROUP_PATHS=()
CAPTURED_AGENT_PIDS=()
STOP_FAILURES=0
readonly STARTED_AT="$(date +%s)"

live_version_probe() {
  local status
  if [[ -n $LIVE_ACCESS_KEY ]]; then
    status=$(curl --config <(printf 'header = "Authorization: Bearer %s"\n' "$LIVE_ACCESS_KEY") --silent --show-error --connect-timeout 2 --max-time 5 --output /dev/null --write-out '%{http_code}' http://127.0.0.1:18498/api/internal/version)
  else
    status=$(curl --silent --show-error --connect-timeout 2 --max-time 5 --output /dev/null --write-out '%{http_code}' http://127.0.0.1:18498/api/internal/version)
  fi
  local curl_status=$?
  if (( curl_status != 0 )); then
    status=000
  fi
  printf '%s' "$status"
}

sanitize_log_text() {
  TRIAL_REDACT_ACCESS_KEY="$ACCESS_KEY" \
    TRIAL_REDACT_LIVE_KEY="$LIVE_ACCESS_KEY" \
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
  local session_dir
  local source
  local relative
  for server_log in "${SERVER_LOGS[@]}"; do
    [[ -f $server_log ]] || continue
    index=$((index + 1))
    copy_redacted_file "$server_log" "$EVIDENCE_ROOT/server-$index.log" || return 1
  done
  # A serve run copies no session data: its sessions are the seed, which its seed directory already holds.
  if (( SERVE_MODE == 0 )) && [[ -d $SESSIONS_DIR ]]; then
    for session_dir in "$SESSIONS_DIR"/*; do
      [[ -d $session_dir ]] || continue
      local session_id=${session_dir##*/}
      source="$session_dir/data/chat_events.jsonl"
      if [[ -f $source ]]; then
        copy_redacted_file "$source" "$EVIDENCE_ROOT/sessions/$session_id/data/chat_events.jsonl" || return 1
      fi
      if [[ -d $session_dir/data/runs ]]; then
        while IFS= read -r -d '' source; do
          relative=${source#"$session_dir/"}
          copy_redacted_file "$source" "$EVIDENCE_ROOT/sessions/$session_id/$relative" || return 1
        done < <(find "$session_dir/data/runs" -type f -print0)
      fi
    done
  fi
  for source in "$HTTP_ROOT/check3-rust-frames.jsonl"; do
    if [[ -f $source ]]; then
      copy_redacted_file "$source" "$EVIDENCE_ROOT/frames/${source##*/}" || return 1
    fi
  done
  EVIDENCE_CAPTURED=1
  return 0
}

print_web_reply_nudges() {
  local server_log
  local found=0
  for server_log in "${SERVER_LOGS[@]}"; do
    [[ -f $server_log ]] || continue
    while IFS= read -r line; do
      found=1
      printf 'WEB_REPLY_NUDGE server-log=%s %s\n' "${server_log##*/}" "$line" | sanitize_log_text
    done < <(rg 'web_reply_nudge' "$server_log" || true)
  done
  if (( found == 0 )); then
    printf 'WEB_REPLY_NUDGE none\n'
  fi
}

print_log_context() {
  local check_number=$1
  local cause=$2
  printf 'CAUSE %s: %s\n' "$check_number" "$cause"
  if [[ -n $SERVER_LOG && -f $SERVER_LOG ]]; then
    printf 'SERVER LOG %s (last 12 lines):\n' "$check_number"
    tail -n 12 "$SERVER_LOG" | sanitize_log_text
  else
    printf 'SERVER LOG %s: no server log was created\n' "$check_number"
  fi
}

report_pass() {
  printf 'PASS %s %s: %s\n' "$1" "$2" "$3"
}

report_fail() {
  OVERALL_FAIL=1
  printf 'FAIL %s %s: %s\n' "$1" "$2" "$3"
  print_log_context "$1" "$4"
}

process_state() {
  ps -p "$1" -o stat= 2>/dev/null | tr -d '[:space:]'
}

LAST_STOP_OK=0
LAST_STOP_SUMMARY=

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
    LAST_STOP_OK=1
    LAST_STOP_SUMMARY="elapsed_ms=$elapsed_ms exit_status=$wait_status sigkill_sent=0"
    return 0
  fi
  LAST_STOP_SUMMARY="elapsed_ms=$elapsed_ms exit_status=$wait_status sigkill_sent=$killed"
  return 1
}

stop_current_server() {
  local pid=${SERVER_PID:-}
  local check_number=${1:-6}
  [[ -n $pid ]] || return 0
  SERVER_PID=
  if ! stop_server_pid "$pid" "$SERVER_LOG" "$check_number"; then
    STOP_FAILURES=$((STOP_FAILURES + 1))
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

record_cgroup_pids() {
  local path=$1
  local pid
  [[ -r $path/cgroup.procs ]] || return 0
  while IFS= read -r pid; do
    [[ $pid =~ ^[0-9]+$ ]] || continue
    CAPTURED_AGENT_PIDS+=("$pid")
  done <"$path/cgroup.procs"
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
    record_cgroup_pids "$path"
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
    if (( SERVE_MODE == 0 && LIVE_AFTER_RECORDED == 0 )); then
      local live_status
      live_status=$(live_version_probe)
      if [[ $live_status == 200 ]]; then
        printf 'LIVE_AFTER PASS GET /api/internal/version port=18498 HTTP 200\n'
      else
        printf 'LIVE_AFTER FAIL GET /api/internal/version port=18498 HTTP %s\n' "$live_status"
        exit_status=1
      fi
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

if (( SERVE_MODE == 0 )); then
  live_status=$(live_version_probe)
  if [[ $live_status == 200 ]]; then
    LIVE_BEFORE_OK=1
    printf 'LIVE_BEFORE PASS GET /api/internal/version port=18498 HTTP 200\n'
  else
    OVERALL_FAIL=1
    printf 'LIVE_BEFORE FAIL GET /api/internal/version port=18498 HTTP %s\n' "$live_status"
  fi
fi

AUTH_STATUS_FILE="$HTTP_ROOT/auth-status.json"
VERSION_FILE="$HTTP_ROOT/trial-version.json"
ROOT_FILE="$HTTP_ROOT/trial-root.html"

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

http_post() {
  local path=$1
  local body=$2
  local output=$3
  local status
  status=$(curl --config "$AUTH_CONFIG" --silent --show-error --connect-timeout 2 --max-time 15 --header 'Content-Type: application/json' --data-binary "@$body" --output "$output" --write-out '%{http_code}' "http://127.0.0.1:$CHOSEN_PORT$path")
  local curl_status=$?
  if (( curl_status != 0 )); then
    status=000
  fi
  printf '%s' "$status"
}

# copy_seed_sessions copies each session directory of SEED_HOME into the trial home. A copy, not a link: the server
# appends to the event logs it serves, and the seed must stay as generated for the next run.
copy_seed_sessions() {
  local entry
  mkdir -m 700 -p "$SESSIONS_DIR" || return 1
  for entry in "$SEED_HOME"/*/; do
    [[ -d $entry ]] || continue
    cp -a -- "${entry%/}" "$SESSIONS_DIR/" || return 1
  done
}

start_server() {
  local server_number=$(( ${#SERVER_PIDS[@]} + 1 ))
  SERVER_LOG="$TREE/server-$server_number.log"
  SERVER_LOGS+=("$SERVER_LOG")
  if (( SERVE_MODE == 0 )); then
    # The trial's git-timeout check needs the slow-git wrapper first on the server's PATH.
    PATH="$SLOW_GIT_BIN:$PATH" "$BINARY" >>"$SERVER_LOG" 2>&1 &
  elif (( IN_CALLER_SCOPE == 1 )); then
    # The serve server stays in the caller's cgroup, whose memory limit the caller's scope already sets.
    "$BINARY" >>"$SERVER_LOG" 2>&1 &
  else
    # The serve server runs inside a scope whose MemoryMax keeps a measurement beside the live server, and without the
    # slow-git wrapper, whose per-call delay would distort the measurement's timings.
    systemd-run --user --scope -p MemoryMax=2G --unit="$SERVE_UNIT" "$BINARY" >>"$SERVER_LOG" 2>&1 &
  fi
  SERVER_PID=$!
  SERVER_PIDS+=("$SERVER_PID")
  local ready_polls=$RELEASE_READY_POLLS
  if (( SERVE_MODE == 1 )); then
    ready_polls=$SERVE_READY_POLLS
  fi
  for _ in $(seq 1 "$ready_polls"); do
    local status
    status=$(http_get /api/auth/status "$AUTH_STATUS_FILE")
    [[ $status == 200 ]] && return 0
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
  OVERALL_FAIL=1
  printf 'SWEEP_NAMES FAIL %s: a startup sweep removed a directory not named for a session under the trial home: %s\n' "$label" "$output"
  return 1
}

read_run_summary() {
  local response=$1
  local -a fields=()
  "$TRIAL_CLI" release-helper run-summary "$response" >"$HTTP_ROOT/run-summary.nul" || return 1
  mapfile -d '' -t fields <"$HTTP_ROOT/run-summary.nul"
  (( ${#fields[@]} == 8 )) || return 1
  RUN_COUNT=${fields[0]}
  RUN_ID=${fields[1]}
  RUN_STATE=${fields[2]}
  RUN_PID=${fields[3]}
  RUN_REPO=${fields[4]}
  RUN_WORKTREE=${fields[5]}
  RUN_BRANCH=${fields[6]}
  RUN_STATES=${fields[7]}
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
  # One caller command instead of the release checks. The exit code is the command's, demoted to 1 by a foreign cgroup
  # removal (sweep-check) or any cleanup failure.
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

if (( SERVE_MODE == 1 )); then
  run_serve_mode
  exit $?
fi

manager_id=
manager_first_input_id=
takeover_id=
manager_first_outcome=
manager_first_cgroup='not observed'
delegation_evidence='not run'
takeover_evidence='not run'
startup_ok=0
session_test_ok=0
stream_test_ok=0
delegation_test_ok=0
takeover_test_ok=0

if start_server; then
  version_status=$(http_get /api/internal/version "$VERSION_FILE")
  root_status=$(curl --config "$AUTH_CONFIG" --silent --show-error --connect-timeout 2 --max-time 10 --output "$ROOT_FILE" --write-out '%{http_code}' "http://127.0.0.1:$CHOSEN_PORT/")
  root_curl_status=$?
  [[ $root_curl_status == 0 ]] || root_status=000
  cgroup_summary=
  mapfile -d '' -t cgroup_fields < <("$TRIAL_CLI" release-helper cgroup-enabled "$SERVER_LOG" 2>/dev/null)
  cgroup_status=$?
  if [[ $version_status == 200 && $root_status == 200 && $cgroup_status == 0 && ${#cgroup_fields[@]} == 4 ]]; then
    CGROUP_MEMORY_MB=${cgroup_fields[1]}
    CGROUP_SWAP_MB=${cgroup_fields[2]}
    CGROUP_TASKS_MAX=${cgroup_fields[3]}
    if [[ $CGROUP_MEMORY_MB == 1024 && $CGROUP_TASKS_MAX == 256 ]]; then
      startup_ok=1
      report_pass 1 Startup "version HTTP 200; root HTTP 200; session cgroup control is on (memory_max_mb=$CGROUP_MEMORY_MB, tasks_max=$CGROUP_TASKS_MAX)"
    else
      report_fail 1 Startup "version HTTP $version_status; root HTTP $root_status; cgroup startup values were ${cgroup_fields[*]:-missing}" 'the server started, but its cgroup startup line did not state the configured limits'
    fi
  else
    report_fail 1 Startup "version HTTP $version_status; root HTTP $root_status; cgroup_startup=${cgroup_fields[*]:-missing}" 'the server did not answer both authenticated routes or did not log session_cgroup_enabled'
  fi
else
  report_fail 1 Startup "server did not become ready on 127.0.0.1:$CHOSEN_PORT" 'the trial server exited or did not bind the selected port'
fi
check_sweep_names "start 1"

manager_request_id="trial-manager-$STARTED_AT-$$"
cat >"$HTTP_ROOT/create-manager.json" <<EOF
{"profile":"manager","request_id":"$manager_request_id","backend":"charlie-code-release-trial","name":"Release trial manager"}
EOF
create_manager_status=$(http_post /api/sessions/ "$HTTP_ROOT/create-manager.json" "$HTTP_ROOT/create-manager-response.json")
if [[ $create_manager_status == 200 ]]; then
  manager_id=$("$TRIAL_CLI" release-helper session-id "$HTTP_ROOT/create-manager-response.json" 2>/dev/null)
fi

if [[ -n $manager_id && -n $SERVER_PID ]]; then
  cat >"$TREE/manager-one-word.txt" <<'EOF'
Run a shell command that sleeps for 8 seconds.
After it exits, reply with exactly the single word HELLO.
EOF
  "$TRIAL_CLI" release-helper write-message "$TREE/manager-one-word.txt" "$HTTP_ROOT/manager-message.json"
  message_status=$(http_post "/api/chat/$manager_id/message" "$HTTP_ROOT/manager-message.json" "$HTTP_ROOT/manager-message-response.json")
  if [[ $message_status == 202 ]]; then
    manager_first_input_id=$("$TRIAL_CLI" release-helper json-field "$HTTP_ROOT/manager-message-response.json" input_event_id 2>/dev/null || true)
  fi
  manager_started_at=$SECONDS
  manager_stable_since=-1
  manager_run_signature=
  while (( SECONDS - manager_started_at < 180 )); do
    runs_status=$(http_get "/api/sessions/$manager_id/runs?order=asc&limit=100" "$HTTP_ROOT/manager-runs.json")
    if [[ $runs_status == 200 ]] && read_run_summary "$HTTP_ROOT/manager-runs.json"; then
      if [[ $RUN_STATE == running && $RUN_PID =~ ^[0-9]+$ ]]; then
        cgroup_path="$CGROUP_BASE/charliebot-sess-${manager_id:0:8}"
        discover_owned_cgroups
        if is_owned_cgroup "$cgroup_path" && [[ -r $cgroup_path/cgroup.procs && -r $cgroup_path/memory.max && -r $cgroup_path/pids.max ]]; then
          record_cgroup_pids "$cgroup_path"
          memory_value=$(cat "$cgroup_path/memory.max")
          pids_value=$(cat "$cgroup_path/pids.max")
          if grep -Fxq "$RUN_PID" "$cgroup_path/cgroup.procs" && [[ $memory_value == 1073741824 && $pids_value == 256 ]]; then
            manager_first_cgroup="pid=$RUN_PID memory.max=$memory_value pids.max=$pids_value"
          fi
        fi
      fi
      manager_first_outcome=$RUN_STATE
      current_signature="$RUN_COUNT|$RUN_STATES"
      if [[ $current_signature != "$manager_run_signature" ]]; then
        manager_run_signature=$current_signature
        manager_stable_since=$SECONDS
      fi
      if (( RUN_COUNT > 0 )) && "$TRIAL_CLI" release-helper runs-success "$HTTP_ROOT/manager-runs.json" >/dev/null 2>&1; then
        if (( manager_stable_since >= 0 && SECONDS - manager_stable_since >= 5 )); then
          break
        fi
      fi
      if [[ $RUN_STATE == failed || $RUN_STATE == interrupted ]]; then
        manager_first_outcome=$RUN_STATE
        break
      fi
    fi
    sleep 0.5
  done
  first_reply=0
  if [[ -f $SESSIONS_DIR/$manager_id/data/chat_events.jsonl ]]; then
    first_reply=$("$TRIAL_CLI" release-helper assistant-reply "$SESSIONS_DIR/$manager_id/data/chat_events.jsonl" HELLO 2>/dev/null)
  fi
  manager_run_contract_status=1
  manager_run_contract_output='run contract was not checked'
  if [[ -f $HTTP_ROOT/manager-runs.json && -f $SESSIONS_DIR/$manager_id/data/chat_events.jsonl ]]; then
    manager_run_contract_output=$("$TRIAL_CLI" release-helper run-contract "$HTTP_ROOT/manager-runs.json" "$SESSIONS_DIR/$manager_id/data/chat_events.jsonl" check2 "$manager_first_input_id" 2>&1)
    manager_run_contract_status=$?
  fi
  printf 'CHECK 2 %s\n' "$manager_run_contract_output"
  print_web_reply_nudges
  if [[ $create_manager_status == 200 && $message_status == 202 && $manager_first_outcome == success && $first_reply == 1 && $manager_first_cgroup != 'not observed' && $manager_run_contract_status == 0 ]]; then
    session_test_ok=1
    report_pass 2 'Manager turn' "manager=$manager_id outcome=$manager_first_outcome assistant_reply=HELLO cgroup=[$manager_first_cgroup]"
  else
    report_fail 2 'Manager turn' "create HTTP $create_manager_status; message HTTP ${message_status:-not sent}; input_event_id=${manager_first_input_id:-missing} outcome=${manager_first_outcome:-timeout} assistant_reply=$first_reply cgroup=[$manager_first_cgroup]" 'the manager run, one-word chat reply, or in-flight session cgroup evidence did not match the trial contract'
  fi
else
  report_fail 2 'Manager turn' "create HTTP $create_manager_status; session=${manager_id:-missing}" 'the manager session could not be created because the trial server was unavailable or rejected the request'
fi

if [[ -n $manager_id && -n $SERVER_PID ]]; then
  stream_output_file="$HTTP_ROOT/stream-output.txt"
  rust_frames_file="$HTTP_ROOT/check3-rust-frames.jsonl"
  if "$TRIAL_CLI" release-helper websocket "ws://127.0.0.1:$CHOSEN_PORT" "$manager_id" "$CREDENTIALS" "$rust_frames_file" >"$stream_output_file" 2>"$HTTP_ROOT/stream-error.txt"; then
    printf 'RUST %s\n' "$(cat "$stream_output_file")"
    stream_frame_count=$(sed -n 's/^frames=//p' "$stream_output_file")
    stream_test_ok=1
    report_pass 3 'Session stream' "Rust WebSocket catchup replay delivered $stream_frame_count frame(s) from the manager session's chat events"
  else
    stream_error=$(cat "$HTTP_ROOT/stream-error.txt" 2>/dev/null)
    report_fail 3 'Session stream' "${stream_error:-WebSocket replay failed}" 'the Rust WebSocket replay did not deliver catchup frames'
  fi
else
  report_fail 3 'Session stream' 'no manager session was available for replay' 'the WebSocket replay depends on the manager session from check 2'
fi

if [[ -n $manager_id && -n $SERVER_PID ]]; then
  cat >"$TREE/manager-delegate.txt" <<EOF
Run this command once:
charliebot delegate --repo $SYNTH_REPO --base-branch origin/main --task-spec-file $TREE/delegate-task.md --keep-worktree 1 --task-type quick-edit
Reply with exactly one word: STARTED.
When the worker completion report arrives, reply with exactly one word: DONE.
EOF
  "$TRIAL_CLI" release-helper write-message "$TREE/manager-delegate.txt" "$HTTP_ROOT/delegate-message.json"
  delegate_message_status=$(http_post "/api/chat/$manager_id/message" "$HTTP_ROOT/delegate-message.json" "$HTTP_ROOT/delegate-message-response.json")
  delegate_input_id=
  if [[ $delegate_message_status == 202 ]]; then
    delegate_input_id=$("$TRIAL_CLI" release-helper json-field "$HTTP_ROOT/delegate-message-response.json" input_event_id 2>/dev/null || true)
  fi
  delegate_started_at=$SECONDS
  delegate_worker_id=
  delegate_task_type=
  delegate_repo_path=
  worker_state=
  worker_worktree=
  worker_branch=
  manager_final_state=
  manager_final_count=0
  manager_final_states=
  manager_report_count=0
  manager_stable_since=-1
  manager_run_signature=
  worker_all_success=0
  worker_commit=''
  worker_changed=''
  worker_clean=0
  while (( SECONDS - delegate_started_at < 900 )); do
    if "$TRIAL_CLI" release-helper child-workers "$SESSIONS_DIR" "$manager_id" >"$HTTP_ROOT/worker-rows.nul" 2>/dev/null; then
      mapfile -d '' -t worker_fields <"$HTTP_ROOT/worker-rows.nul"
      if (( ${#worker_fields[@]} >= 3 )); then
        delegate_worker_id=${worker_fields[0]}
        delegate_task_type=${worker_fields[1]}
        delegate_repo_path=${worker_fields[2]}
      fi
      if [[ -n $delegate_worker_id ]]; then
        worker_runs_status=$(http_get "/api/sessions/$delegate_worker_id/runs?order=asc&limit=100" "$HTTP_ROOT/worker-runs.json")
        if [[ $worker_runs_status == 200 ]] && read_run_summary "$HTTP_ROOT/worker-runs.json"; then
          worker_state=$RUN_STATE
          worker_worktree=$RUN_WORKTREE
          worker_branch=$RUN_BRANCH
          if "$TRIAL_CLI" release-helper runs-success "$HTTP_ROOT/worker-runs.json" >/dev/null 2>&1; then
            worker_all_success=1
          fi
          discover_owned_cgroups
          if [[ $worker_state == success && -d $worker_worktree && $worker_worktree == "$WORKTREE_ROOT"/* ]]; then
            worker_commit=$(git -C "$worker_worktree" rev-parse HEAD 2>/dev/null)
            worker_changed=$(git -C "$worker_worktree" diff-tree --no-commit-id --name-only -r HEAD^ HEAD 2>/dev/null)
            if [[ -z $(git -C "$worker_worktree" status --porcelain 2>/dev/null) ]]; then
              worker_clean=1
            fi
          fi
        fi
      fi
    fi
    manager_runs_status=$(http_get "/api/sessions/$manager_id/runs?order=asc&limit=100" "$HTTP_ROOT/manager-runs.json")
    if [[ $manager_runs_status == 200 ]] && read_run_summary "$HTTP_ROOT/manager-runs.json"; then
      manager_final_count=$RUN_COUNT
      manager_final_state=$RUN_STATE
      manager_final_states=$RUN_STATES
      current_signature="$RUN_COUNT|$RUN_STATES"
      if [[ $current_signature != "$manager_run_signature" ]]; then
        manager_run_signature=$current_signature
        manager_stable_since=$SECONDS
      fi
    fi
    if [[ -f $SESSIONS_DIR/$manager_id/data/chat_events.jsonl ]]; then
      manager_report_count=$("$TRIAL_CLI" release-helper event-type-count "$SESSIONS_DIR/$manager_id/data/chat_events.jsonl" child_report 2>/dev/null || printf '0')
    fi
    if [[ $worker_all_success == 1 && $manager_final_count -ge 3 && $manager_report_count -ge 1 ]] && "$TRIAL_CLI" release-helper runs-success "$HTTP_ROOT/manager-runs.json" >/dev/null 2>&1; then
      if (( manager_stable_since >= 0 && SECONDS - manager_stable_since >= 5 )); then
        break
      fi
    fi
    sleep 1
  done
  worker_reply=0
  manager_started_reply=0
  manager_contract_status=1
  manager_contract_output='run contract was not checked'
  if [[ -f $HTTP_ROOT/manager-runs.json && -f $SESSIONS_DIR/$manager_id/data/chat_events.jsonl ]]; then
    manager_contract_output=$("$TRIAL_CLI" release-helper run-contract "$HTTP_ROOT/manager-runs.json" "$SESSIONS_DIR/$manager_id/data/chat_events.jsonl" check4 "$manager_first_input_id" "$delegate_input_id" 2>&1)
    manager_contract_status=$?
  fi
  worker_contract_status=1
  worker_contract_output='worker runs were not checked'
  if [[ -f $HTTP_ROOT/worker-runs.json ]]; then
    worker_contract_output=$("$TRIAL_CLI" release-helper runs-success "$HTTP_ROOT/worker-runs.json" 2>&1)
    worker_contract_status=$?
  fi
  printf 'CHECK 4 %s\n' "$manager_contract_output"
  printf 'CHECK 4 worker %s\n' "$worker_contract_output"
  print_web_reply_nudges
  # The server's own git timeout text, read from the sessions it wrote: with a sub-second binary the delegation check must
  # name the failing limit instead of only reporting a missing worker.
  delegation_error=''
  if [[ -n $manager_id ]]; then
    delegation_error=$("$TRIAL_CLI" release-helper delegation-error "$SESSIONS_DIR" "$manager_id" 2>/dev/null | sanitize_log_text || true)
  fi
  if [[ -f $SESSIONS_DIR/$manager_id/data/chat_events.jsonl ]]; then
    worker_reply=$("$TRIAL_CLI" release-helper assistant-reply "$SESSIONS_DIR/$manager_id/data/chat_events.jsonl" DONE 2>/dev/null)
    manager_started_reply=$("$TRIAL_CLI" release-helper assistant-reply "$SESSIONS_DIR/$manager_id/data/chat_events.jsonl" STARTED 2>/dev/null)
  fi
  delegation_evidence="delegate_input_id=${delegate_input_id:-missing} worker=$delegate_worker_id task_type=$delegate_task_type repo=$delegate_repo_path run=$worker_state worktree=$worker_worktree commit=$worker_commit changed=$worker_changed clean=$worker_clean manager_runs=$manager_final_count states=${manager_final_states:-missing} task_report_events=$manager_report_count manager_started_reply=$manager_started_reply manager_completion_reply=$worker_reply"
  if [[ -n $delegation_error ]]; then
    delegation_evidence+=" server_error=$delegation_error"
  fi
  if [[ $delegate_message_status == 202 && -n $delegate_input_id && -n $delegate_worker_id && $delegate_task_type == quick-edit && $delegate_repo_path == "$SYNTH_REPO" && $worker_state == success && $worker_all_success == 1 && $worker_contract_status == 0 && $worker_worktree == "$WORKTREE_ROOT"/* && $worker_commit =~ ^[0-9a-f]{40}$ && $worker_changed == trial.txt && $worker_clean == 1 && $manager_final_count -ge 3 && $manager_report_count -ge 1 && $manager_contract_status == 0 && $worker_reply == 1 ]]; then
    delegation_test_ok=1
    report_pass 4 Delegation "$delegation_evidence"
  elif [[ -n $delegation_error ]]; then
    report_fail 4 Delegation "HTTP $delegate_message_status; $delegation_evidence" "the server's delegation did not complete: $delegation_error"
  else
    report_fail 4 Delegation "HTTP $delegate_message_status; $delegation_evidence" 'the quick-edit worker did not commit one file or manager run inputs/outcomes did not match the required delegation, task report, and nudge classes'
  fi
else
  report_fail 4 Delegation 'no manager session or trial server was available' 'the quick-edit delegation could not start because check 2 did not create a usable manager session'
fi

takeover_request_id="trial-takeover-$STARTED_AT-$$"
cat >"$HTTP_ROOT/create-takeover.json" <<EOF
{"profile":"manager","request_id":"$takeover_request_id","backend":"charlie-code-release-trial","name":"Release trial takeover"}
EOF
takeover_create_status=$(http_post /api/sessions/ "$HTTP_ROOT/create-takeover.json" "$HTTP_ROOT/create-takeover-response.json")
if [[ $takeover_create_status == 200 ]]; then
  takeover_id=$("$TRIAL_CLI" release-helper session-id "$HTTP_ROOT/create-takeover-response.json" 2>/dev/null)
fi

if [[ -n $takeover_id && -n $SERVER_PID ]]; then
  cat >"$TREE/takeover-prompt.txt" <<'EOF'
Run a shell command that sleeps for 60 seconds.
After the command finishes, reply with exactly one word: TAKEOVER_OK.
EOF
  "$TRIAL_CLI" release-helper write-message "$TREE/takeover-prompt.txt" "$HTTP_ROOT/takeover-message.json"
  takeover_message_status=$(http_post "/api/chat/$takeover_id/message" "$HTTP_ROOT/takeover-message.json" "$HTTP_ROOT/takeover-message-response.json")
  takeover_input_id=
  if [[ $takeover_message_status == 202 ]]; then
    takeover_input_id=$("$TRIAL_CLI" release-helper json-field "$HTTP_ROOT/takeover-message-response.json" input_event_id 2>/dev/null || true)
  fi
  takeover_started_at=$SECONDS
  takeover_run_pid=
  takeover_sleep_seen=0
  takeover_initial_state=
  while (( SECONDS - takeover_started_at < 45 )); do
    takeover_runs_status=$(http_get "/api/sessions/$takeover_id/runs?order=asc&limit=100" "$HTTP_ROOT/takeover-runs.json")
    if [[ $takeover_runs_status == 200 ]] && read_run_summary "$HTTP_ROOT/takeover-runs.json"; then
      takeover_initial_state=$RUN_STATE
      takeover_run_pid=$RUN_PID
      cgroup_path="$CGROUP_BASE/charliebot-sess-${takeover_id:0:8}"
      discover_owned_cgroups
      if [[ $RUN_STATE == running && $RUN_PID =~ ^[0-9]+$ && -r $cgroup_path/cgroup.procs ]] && is_owned_cgroup "$cgroup_path"; then
        record_cgroup_pids "$cgroup_path"
        if grep -Fxq "$RUN_PID" "$cgroup_path/cgroup.procs"; then
          while IFS= read -r child_pid; do
            [[ $child_pid =~ ^[0-9]+$ ]] || continue
            if [[ -r /proc/$child_pid/comm ]] && [[ $(cat "/proc/$child_pid/comm") == sleep ]]; then
              takeover_sleep_seen=1
              break
            fi
          done <"$cgroup_path/cgroup.procs"
        fi
      fi
      if (( takeover_sleep_seen == 1 )); then
        break
      fi
    fi
    sleep 0.5
  done
  old_server_stopped=0
  old_server_stop_summary='not attempted'
  new_server_ready=0
  restart_version_status=000
  if (( takeover_sleep_seen == 1 )) && [[ $takeover_message_status == 202 ]]; then
    if stop_current_server 5; then
      old_server_stopped=1
    fi
    old_server_stop_summary=$LAST_STOP_SUMMARY
    if (( old_server_stopped == 1 )) && start_server; then
      new_server_ready=1
      restart_version_status=$(http_get /api/internal/version "$HTTP_ROOT/restarted-version.json")
    fi
    if (( old_server_stopped == 1 )); then
      check_sweep_names "start 2"
    fi
  fi
  takeover_wait_started=$SECONDS
  takeover_state=
  takeover_run_count=0
  takeover_states=
  while (( SECONDS - takeover_wait_started < 240 )); do
    if (( new_server_ready == 1 )); then
      takeover_runs_status=$(http_get "/api/sessions/$takeover_id/runs?order=asc&limit=100" "$HTTP_ROOT/takeover-runs.json")
      if [[ $takeover_runs_status == 200 ]] && read_run_summary "$HTTP_ROOT/takeover-runs.json"; then
        takeover_state=$RUN_STATE
        takeover_run_count=$RUN_COUNT
        takeover_states=$RUN_STATES
        discover_owned_cgroups
        if [[ $takeover_state == success || $takeover_state == failed || $takeover_state == interrupted ]]; then
          break
        fi
      fi
    fi
    sleep 1
  done
  takeover_reply=0
  if [[ -f $SESSIONS_DIR/$takeover_id/data/chat_events.jsonl ]]; then
    takeover_reply=$("$TRIAL_CLI" release-helper assistant-reply "$SESSIONS_DIR/$takeover_id/data/chat_events.jsonl" TAKEOVER_OK 2>/dev/null)
  fi
  takeover_contract_status=1
  takeover_contract_output='not checked'
  if [[ -n ${takeover_input_id:-} && -f $HTTP_ROOT/takeover-runs.json && -f $SESSIONS_DIR/$takeover_id/data/chat_events.jsonl ]]; then
    takeover_contract_output=$("$TRIAL_CLI" release-helper run-contract "$HTTP_ROOT/takeover-runs.json" "$SESSIONS_DIR/$takeover_id/data/chat_events.jsonl" check5 "$takeover_input_id" 2>&1)
    takeover_contract_status=$?
  fi
  printf 'CHECK 5 %s\n' "$takeover_contract_output"
  takeover_evidence="session=$takeover_id message_HTTP=$takeover_message_status sleep_in_cgroup=$takeover_sleep_seen old_server_SIGTERM=$old_server_stopped old_server_stop=[$old_server_stop_summary] restart_ready=$new_server_ready restart_version_HTTP=$restart_version_status run_count=$takeover_run_count run_outcome=${takeover_state:-timeout} run_states=${takeover_states:-missing} assistant_reply=$takeover_reply"
  if [[ $takeover_create_status == 200 && $takeover_message_status == 202 && $takeover_sleep_seen == 1 && $old_server_stopped == 1 && $new_server_ready == 1 && $restart_version_status == 200 && $takeover_contract_status == 0 && $takeover_state == success && $takeover_reply == 1 ]]; then
    takeover_test_ok=1
    report_pass 5 Takeover "$takeover_evidence"
  else
    report_fail 5 Takeover "create_HTTP=$takeover_create_status; $takeover_evidence" 'the restart did not adopt one running sleep-60 manager run and finish it successfully'
  fi
else
  report_fail 5 Takeover "create_HTTP=$takeover_create_status; session=${takeover_id:-missing}" 'the takeover manager turn could not start because the server or session was unavailable'
fi

shutdown_stop_ok=1
if [[ -n ${SERVER_PID:-} ]]; then
  if ! stop_current_server 6; then
    shutdown_stop_ok=0
  fi
else
  shutdown_stop_ok=0
  LAST_STOP_SUMMARY='not attempted: no last server process was available'
  printf 'SIGTERM check=6 no_last_server_process=1\n'
fi
if ! cleanup_owned_cgroups; then
  shutdown_cgroup_ok=0
else
  shutdown_cgroup_ok=1
fi
if ! cleanup_trial_tmux_servers; then
  shutdown_tmux_ok=0
else
  shutdown_tmux_ok=1
fi

session_cgroup_remaining=0
if [[ -d $SESSIONS_DIR ]]; then
  while IFS= read -r trial_session_id; do
    [[ -n $trial_session_id ]] || continue
    trial_cgroup="$CGROUP_BASE/charliebot-sess-${trial_session_id:0:8}"
    if [[ -d $trial_cgroup ]]; then
      session_cgroup_remaining=$((session_cgroup_remaining + 1))
    fi
  done < <("$TRIAL_CLI" release-helper session-ids "$SESSIONS_DIR" 2>/dev/null)
fi

server_processes_remaining=0
for server_pid in "${SERVER_PIDS[@]}"; do
  state=$(process_state "$server_pid")
  if [[ -n $state && $state != *Z* ]]; then
    server_processes_remaining=$((server_processes_remaining + 1))
  fi
done
agent_processes_remaining=0
for agent_pid in "${CAPTURED_AGENT_PIDS[@]}"; do
  state=$(process_state "$agent_pid")
  if [[ -n $state && $state != *Z* ]]; then
    agent_processes_remaining=$((agent_processes_remaining + 1))
  fi
done

shutdown_log_context=
if [[ -n $SERVER_LOG && -f $SERVER_LOG ]]; then
  shutdown_log_context=$(tail -n 12 "$SERVER_LOG" | sanitize_log_text)
fi

live_status=$(live_version_probe)
LIVE_AFTER_RECORDED=1
if [[ $live_status == 200 ]]; then
  LIVE_AFTER_OK=1
  printf 'LIVE_AFTER PASS GET /api/internal/version port=18498 HTTP 200\n'
else
  OVERALL_FAIL=1
  printf 'LIVE_AFTER FAIL GET /api/internal/version port=18498 HTTP %s\n' "$live_status"
fi

tree_removed=0
if ! capture_evidence; then
  OVERALL_FAIL=1
  printf 'evidence capture failed: %s\n' "$EVIDENCE_ROOT" >&2
fi
if remove_trial_tree; then
  tree_removed=1
fi
CLEANUP_DONE=1

if [[ $shutdown_stop_ok == 1 && $shutdown_cgroup_ok == 1 && $shutdown_tmux_ok == 1 && $session_cgroup_remaining == 0 && $server_processes_remaining == 0 && $agent_processes_remaining == 0 && $tree_removed == 1 ]]; then
  report_pass 6 Shutdown "server_stop=[$LAST_STOP_SUMMARY] server_processes_remaining=$server_processes_remaining agent_processes_remaining=$agent_processes_remaining trial_session_cgroups_remaining=$session_cgroup_remaining tree_removed=$tree_removed"
else
  OVERALL_FAIL=1
  printf 'FAIL 6 Shutdown: server_stop_ok=%s server_stop=[%s] cgroup_cleanup_ok=%s tmux_cleanup_ok=%s trial_session_cgroups_remaining=%s server_processes_remaining=%s agent_processes_remaining=%s tree_removed=%s\n' "$shutdown_stop_ok" "$LAST_STOP_SUMMARY" "$shutdown_cgroup_ok" "$shutdown_tmux_ok" "$session_cgroup_remaining" "$server_processes_remaining" "$agent_processes_remaining" "$tree_removed"
  printf 'CAUSE 6: a trial process, cgroup, or tree remained after SIGTERM cleanup\n'
  if [[ -n $shutdown_log_context ]]; then
    printf 'SERVER LOG 6 (last 12 lines):\n%s\n' "$shutdown_log_context"
  fi
fi

if (( LIVE_BEFORE_OK == 0 || LIVE_AFTER_OK == 0 )); then
  OVERALL_FAIL=1
fi
if (( OVERALL_FAIL != 0 )); then
  printf 'EVIDENCE path=%s\n' "$EVIDENCE_ROOT"
  exit 1
fi
printf 'EVIDENCE path=%s\n' "$EVIDENCE_ROOT"
exit 0
