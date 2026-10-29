#!/usr/bin/env bash
# Run one improve round on the remote host.
#
#   remote-runner.sh <launch id> <direction> <CLI commit>
#   remote-runner.sh --in-scope <launch id> <direction>
#   remote-runner.sh --prepare <launch id>     (the gateway tokens on standard input)
#
# tools/improve/remote-launch.sh sends this file, with the round's arguments and inputs, into
# ~/improve/launch/<launch id>/ and runs the remaining forms over ssh. `--prepare` reads the
# gateway tokens from its standard input and writes them into ~/improve/home/credentials.yaml,
# the only file that holds a token. The first form prepares the round's environment, then starts
# the round's scope through systemd-run; the scope runs the `--in-scope` form, which empties
# TMPDIR, runs claude under the write-only Landlock boundary and records the scope's memory peak.
#
# Exit codes: claude's exit code; 1 a local failure, such as a missing input file or a refused
# scope; 4 the direction's clone is missing or is not this protocol's clone; 5 the previous round
# of this direction still runs. Nothing starts on 4 and 5.
#
# The round reads its direction from this invocation, so the scope name, the boundary and TMPDIR
# name one direction only. Two rounds of one direction never run together: the scope name refuses
# the second one.
#
# The clone check runs before the CLI build because the runner's own checkout takes its origin
# from that clone. The runner deletes its launch directory when it exits; the round's evidence is
# a separate directory, which the launcher copies back after this script has ended.
set -euo pipefail

readonly STDERR_PREFIX='improve runner:'
readonly UNIT_MEMORY_MAX='19G'
readonly BUILD_JOBS='4'

fail() {
  local code=$1
  shift
  printf '%s %s\n' "$STDERR_PREFIX" "$*" >&2
  exit "$code"
}

usage() {
  printf 'usage: %s <launch id> <direction> <CLI commit>\n       %s --in-scope <launch id> <direction>\n       %s --prepare <launch id>\n' \
    "${BASH_SOURCE[0]}" "${BASH_SOURCE[0]}" "${BASH_SOURCE[0]}" >&2
  exit 1
}

mode=run
commit=
case ${1:-} in
  --in-scope)
    [[ $# -eq 3 ]] || usage
    mode=in_scope
    launch_id=$2
    direction=$3
    ;;
  --prepare)
    [[ $# -eq 2 ]] || usage
    mode=prepare
    launch_id=$2
    direction=
    ;;
  '')
    usage
    ;;
  *)
    [[ $# -eq 3 ]] || usage
    launch_id=$1
    direction=$2
    commit=$3
    ;;
esac
[[ $launch_id =~ ^[0-9A-Za-z._-]+$ ]] || fail 1 "not a launch id: $launch_id"
if [[ $mode != prepare ]]; then
  [[ $direction =~ ^[a-z][a-z0-9_]*$ ]] || fail 1 "not an improve direction: $direction"
fi

readonly HOME_DIR=$HOME
readonly LAUNCH_DIR="$HOME_DIR/improve/launch/$launch_id"
readonly ROUND_HOME="$HOME_DIR/improve/home"
readonly RUNNER_ROOT="$HOME_DIR/improve/runner"
[[ $LAUNCH_DIR == "$HOME_DIR"/improve/launch/* ]] || fail 1 "the launch directory is not under the improve root: $LAUNCH_DIR"
if [[ $mode != prepare ]]; then
  [[ -d $LAUNCH_DIR ]] || fail 1 "the launch directory is missing: $LAUNCH_DIR"
  readonly ROUND_ROOT="$HOME_DIR/improve/$direction"
  readonly ROUND_TMP="/tmp/improve-$direction"
  readonly ROUND_TMPDIR="/tmp/improve-$direction-tmp"
fi

# The variables the launcher forwarded. The launcher writes only these names, and a name outside
# the list stops the round: an unlisted name would reach claude from an input nobody reviewed.
read_launch_env() {
  local record name value
  while IFS= read -r -d '' record; do
    name=${record%%=*}
    value=${record#*=}
    case $name in
      ANTHROPIC_* | CLAUDE_* | BASH_DEFAULT_TIMEOUT_MS | BASH_MAX_TIMEOUT_MS | CHARLIEBOT_SESSION_ID)
        export "$name=$value"
        ;;
      *) fail 1 "the launch sent an unexpected variable name: $name" ;;
    esac
  done <"$LAUNCH_DIR/env"
}

# Read the gateway tokens from standard input and write them into the round's credentials file,
# one section per backend entry that names a credential. The tokens arrive on standard input so no
# token reaches a command line, and this file is the only one that holds them. The sections come
# from config.yaml, which the host worker owns, so CLC and the release trial find their key under
# the section name that their own entry declares.
write_credentials() {
  local auth_token= api_key=
  local target="$ROUND_HOME/credentials.yaml"
  local sections section record name value
  while IFS= read -r -d '' record; do
    name=${record%%=*}
    value=${record#*=}
    case $name in
      ANTHROPIC_AUTH_TOKEN) auth_token=$value ;;
      ANTHROPIC_API_KEY) api_key=$value ;;
      *) fail 1 "the token stream names an unexpected variable: $name" ;;
    esac
  done
  local token=${auth_token:-$api_key}
  [[ -n $token ]] || fail 1 'the launch sent no gateway token'
  mkdir -p -- "$ROUND_HOME"
  [[ -f $ROUND_HOME/config.yaml ]] || fail 1 "the round's config.yaml is missing: $ROUND_HOME/config.yaml"
  sections=$(sed -n 's/^[[:space:]]*-\{0,1\}[[:space:]]*credential:[[:space:]]*//p' "$ROUND_HOME/config.yaml" |
    sed 's/[[:space:]]*#.*//; s/^["'\'']//; s/["'\'']$//' |
    grep -Ev '^(null|~|)$' | sort -u) || sections=
  [[ -n $sections ]] || fail 1 "the round's config.yaml names no backend credential: $ROUND_HOME/config.yaml"
  umask 077
  : >"$target"
  for section in $sections; do
    printf '%s:\n  api_key: %s\n' "$section" "$token" >>"$target"
  done
  chmod 600 "$target"
}

# Copy the leak pattern file the launcher sent into the round's home. github-post.sh reads it from
# $CHARLIEBOT_HOME, so the round checks its GitHub text against this launch's version.
install_pattern_file() {
  cp -- "$LAUNCH_DIR/skills_leak_patterns.local.txt" "$ROUND_HOME/skills_leak_patterns.local.txt"
  chmod 600 "$ROUND_HOME/skills_leak_patterns.local.txt"
}

map_path() {
  local arg=$1
  local host_path copy
  while IFS=$'\t' read -r host_path copy; do
    if [[ $arg == "$host_path" ]]; then
      printf '%s' "$LAUNCH_DIR/$copy"
      return
    fi
  done <"$LAUNCH_DIR/paths"
  printf '%s' "$arg"
}

# Replace each host path with its copy in the launch directory, as the `paths` file pairs them,
# and drop the `--mcp-config` pair. That pair names this host's localhost port: a reverse tunnel
# would expose the server to every account on this host, so a round runs without the web search
# tool and keeps its WebFetch tool.
rewrite_command() {
  local -a original=()
  local -a rewritten=()
  local index=0
  local count arg
  mapfile -d '' -t original <"$LAUNCH_DIR/args"
  count=${#original[@]}
  while (( index < count )); do
    arg=${original[index]}
    if [[ $arg == --mcp-config ]]; then
      index=$((index + 2))
      continue
    fi
    rewritten+=("$(map_path "$arg")")
    index=$((index + 1))
  done
  REWRITTEN=("${rewritten[@]}")
}

# The other directions' clones are standalone repositories on this host, so the list that the
# launcher computed there cannot see their work, and two directions pick topics in rust/. Every
# file that those clones change against origin/main therefore joins the round's list before the
# clone check: each branch's committed work past its merge base, plus the uncommitted and
# untracked paths of the checkout, the way inflight-files.sh reads a worktree. A branch without a
# merge base is skipped with a note, as inflight-files.sh skips one.
other_inflight_files() {
  local clone other branch merge_base
  for clone in "$HOME_DIR"/improve/*/charlie-bot; do
    [[ -d $clone ]] || continue
    other=$(basename -- "$(dirname -- "$clone")")
    [[ $other == "$direction" ]] && continue
    while IFS= read -r branch; do
      if merge_base=$(git -C "$clone" merge-base origin/main "$branch" 2>/dev/null); then
        git -C "$clone" diff --name-only "$merge_base" "$branch" || :
      else
        printf 'improve runner: skip branch %s in %s: no merge base with origin/main\n' "$branch" "$clone" >&2
      fi
    done < <(git -C "$clone" for-each-ref --format='%(refname:short)' refs/heads/)
    git -C "$clone" diff --name-only HEAD 2>/dev/null || :
    git -C "$clone" ls-files --others --exclude-standard 2>/dev/null || :
  done
}

# The direction's clone is the round's only checkout inside the boundary. A round whose clone is
# absent or foreign must not start: on 2026-10-29 a local round found no clone and continued in
# another checkout although its prompt told it to stop, so the check holds here, before claude.
check_clone() {
  local clone="$ROUND_ROOT/charlie-bot"
  local hooks origin
  [[ -d $clone ]] || fail 4 "the direction clone is missing: $clone"
  hooks=$(git -C "$clone" config --get core.hooksPath 2>/dev/null) || hooks=
  [[ $hooks == tools/git-hooks ]] || fail 4 "the direction clone does not set core.hooksPath=tools/git-hooks: $clone"
  origin=$(git -C "$clone" remote get-url origin 2>/dev/null) || origin=
  [[ $(basename -- "${origin:-/nonexistent}" .git) == charlie-bot ]] \
    || fail 4 "the direction clone's origin does not point at charlie-bot: ${origin:-no origin}"
}

# The hooks of a round call `charliebot`. The CLI comes from the commit this host's runtime clone
# runs, built in a checkout the runner owns: the boundary cannot write that checkout, so a round
# cannot change the code that runs outside its boundary.
ensure_cli() {
  local commit=$1
  local bin_dir="$RUNNER_ROOT/bin"
  local checkout="$RUNNER_ROOT/charlie-bot"
  local cli="$bin_dir/charliebot-$commit"
  local origin
  mkdir -p -- "$bin_dir"
  if [[ ! -x $cli ]]; then
    if [[ ! -d $checkout/.git ]]; then
      origin=$(git -C "$ROUND_ROOT/charlie-bot" remote get-url origin)
      git clone --quiet -- "$origin" "$checkout" || fail 1 "cannot clone the runner's checkout: $checkout"
    fi
    git -C "$checkout" fetch --quiet origin "$commit" || fail 1 "cannot fetch commit $commit into $checkout"
    git -C "$checkout" -c advice.detachedHead=false checkout --quiet --detach "$commit" \
      || fail 1 "cannot check out commit $commit in $checkout"
    ( cd "$checkout/rust" &&
      CARGO_BUILD_JOBS=$BUILD_JOBS CARGO_TARGET_DIR="$RUNNER_ROOT/target" \
        nice -n 19 cargo build --locked --release -p charliebot-cli --bin charliebot ) \
      || fail 1 "cannot build the charliebot CLI at commit $commit in $checkout"
    install -m 755 -- "$RUNNER_ROOT/target/release/charliebot" "$cli"
  fi
  ln -sfn -- "charliebot-$commit" "$bin_dir/charliebot"
}

# Empty TMPDIR. A killed round leaves large copies there, and this round owns the directory: the
# scope name keeps another round of this direction out.
empty_tmpdir() {
  [[ $ROUND_TMPDIR == "/tmp/improve-$direction-tmp" ]] || fail 1 "not this round's TMPDIR: $ROUND_TMPDIR"
  find "$ROUND_TMPDIR" -mindepth 1 -maxdepth 1 -exec rm -rf -- {} + || fail 1 "cannot empty TMPDIR: $ROUND_TMPDIR"
}

# Append the scope's peak memory with the launch id. The value disappears when the scope ends, so
# the round reads it inside the scope.
record_memory_peak() {
  local cgroup peak
  cgroup=$(sed -n 's/^0:://p' /proc/self/cgroup)
  peak=$(cat "/sys/fs/cgroup$cgroup/memory.peak" 2>/dev/null) || peak=
  if [[ -n $peak ]]; then
    printf '%s\t%s\n' "$launch_id" "$peak" >>"$ROUND_ROOT/memory-peaks.tsv"
  else
    printf '%s no memory.peak for launch %s; the peak is not recorded\n' "$STDERR_PREFIX" "$launch_id" >&2
  fi
}

# Export the gateway token that claude reads as ANTHROPIC_AUTH_TOKEN. The prepare step wrote the token to the
# round's credentials file, and the launcher keeps it out of the environment file, so this is the one place it enters
# the round: the environment of claude, with no argument and no other file that carries it.
export_gateway_token() {
  local tokens token
  tokens=$(sed -n 's/^[[:space:]]*api_key:[[:space:]]*//p' "$ROUND_HOME/credentials.yaml")
  token=${tokens%%$'\n'*}
  [[ -n $token ]] || fail 1 "the round's credentials file holds no gateway token: $ROUND_HOME/credentials.yaml"
  export ANTHROPIC_AUTH_TOKEN=$token
}

run_in_scope() {
  # The marker tells the heartbeat that this launch owns the scope: a refusal leaves no marker,
  # so a heartbeat that closes cannot stop the previous round's scope.
  : >"$LAUNCH_DIR/started"
  empty_tmpdir
  export_gateway_token
  local status=0
  python3 -- "$LAUNCH_DIR/landlock_exec.py" "$direction" -- claude "${REWRITTEN[@]}" || status=$?
  record_memory_peak
  exit "$status"
}

if [[ $mode == prepare ]]; then
  write_credentials
  exit 0
fi

if [[ $mode == in_scope ]]; then
  rewrite_command
  run_in_scope
fi

[[ $commit =~ ^[0-9a-f]{40}$ ]] || fail 1 "not a full commit: $commit"

cleanup_launch_dir() {
  [[ $LAUNCH_DIR == "$HOME_DIR"/improve/launch/* ]] && rm -rf -- "$LAUNCH_DIR"
  return 0
}
trap cleanup_launch_dir EXIT

read_launch_env
install_pattern_file
rewrite_command
other_inflight_files >>"$LAUNCH_DIR/inflight-files.txt"
check_clone

# The round works in the direction's root, because `--resume` finds a Claude session by the
# directory. The PATH puts the remote binaries first: claude and charlie-code from ~/.local/bin,
# cargo from ~/.cargo/bin, and the charliebot CLI that the hooks call from the runner's bin.
export CHARLIEBOT_HOME="$ROUND_HOME"
export CLAUDE_CONFIG_DIR="$HOME_DIR/.claude-improve"
export TMPDIR="$ROUND_TMPDIR"
export IMPROVE_LAUNCH_DIR="$LAUNCH_DIR"
export IMPROVE_DIRECTION="$direction"
export PATH="$RUNNER_ROOT/bin:$HOME_DIR/.local/bin:$HOME_DIR/.cargo/bin:${PATH:-/usr/local/bin:/usr/bin:/bin}"
mkdir -p -- "$ROUND_ROOT/evidence" "$ROUND_ROOT/clc-sessions" "$ROUND_TMP" "$ROUND_TMPDIR" "$ROUND_HOME" "$CLAUDE_CONFIG_DIR"
if [[ ! -e $ROUND_ROOT/memory-peaks.tsv ]]; then
  printf 'launch\tmemory_peak_bytes\n' >"$ROUND_ROOT/memory-peaks.tsv"
fi
cd "$ROUND_ROOT"

ensure_cli "$commit"

# The scope bounds the round's memory: the round and every process it starts, the trial server
# included, die inside it instead of pushing another owner's build into swap. The scope's stderr
# is held in a file, so a refused scope can be named, and it is printed once the scope ends.
scope_stderr="$LAUNCH_DIR/scope.stderr"
scope_status=0
systemd-run --user --scope --unit "improve-$direction" \
  -p "MemoryMax=$UNIT_MEMORY_MAX" -p MemorySwapMax=0 -- \
  bash "$LAUNCH_DIR/remote-runner.sh" --in-scope "$launch_id" "$direction" 2>"$scope_stderr" || scope_status=$?
cat -- "$scope_stderr" >&2 || true
if (( scope_status != 0 )); then
  if grep -q 'already loaded' "$scope_stderr"; then
    fail 5 "the previous round still runs: unit improve-$direction.scope exists"
  fi
  exit "$scope_status"
fi
