#!/usr/bin/env bash
# Run one improve round on the remote host.
#
#   remote-runner.sh <launch id> <direction> <CLI commit> <member kind>
#   remote-runner.sh --in-scope <launch id> <direction> <member kind>
#   remote-runner.sh --prepare <launch id>     (the review key and the member's login on standard input)
#
# The member kind is `claude` or `codex`: the agent the round runs.
#
# dev/improve/remote-launch.sh sends this file, with the round's arguments and inputs, into
# ~/improve/launch/<launch id>/ and runs the remaining forms over ssh. `--prepare` reads the round's
# records from its standard input. The review key goes into ~/improve/home/credentials.yaml under its
# credential's name, written through a temporary file and renamed over it. The member's login goes
# into the launch's member-credential file (mode 600), which only this launch reads. The first form
# checks the round's dependencies, prepares the round's environment, then starts the round's scope
# through systemd-run; the scope runs the `--in-scope` form, which empties TMPDIR, runs the member's
# agent under the Landlock boundary that limits its writes and signals, and records the scope's memory peak.
#
# Exit codes: the agent's exit code; 1 a local failure, such as a missing input file or a refused
# scope, or a round whose review still runs in the scope after its agent exited (stderr names the
# review); 4 the direction's clone is missing or is not this protocol's clone; 5 the previous round
# of this direction still runs; 7 a round dependency is missing (a login, the review key, the round's
# home or the write boundary), and stderr names it. Nothing starts on 4, 5 and 7.
#
# The round reads its direction from this invocation, so the scope name, the boundary and TMPDIR
# name one direction only. Two rounds of one direction never run together: the scope name refuses
# the second one. A killed round leaves its scope in the failed state, so the runner clears a
# failed scope of the same name before the start. A loaded scope whose round is over is stale: a
# process that outlived the round, a detached mock-home helper for instance, keeps the unit active
# and refuses every later fire, so the runner stops it, every process in the cgroup included,
# before the start. A live round's launch directory and leader refuse the round with exit 5.
#
# The clone check runs before the CLI build, so a refused clone starts no build. The runner deletes
# its launch directory when it exits; the round's evidence is a separate directory, which the
# launcher copies back after this script has ended.
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
  printf 'usage: %s <launch id> <direction> <CLI commit> <member kind>\n       %s --in-scope <launch id> <direction> <member kind>\n       %s --prepare <launch id>\n' \
    "${BASH_SOURCE[0]}" "${BASH_SOURCE[0]}" "${BASH_SOURCE[0]}" >&2
  exit 1
}

mode=run
commit=
kind=
case ${1:-} in
  --in-scope)
    [[ $# -eq 4 ]] || usage
    mode=in_scope
    launch_id=$2
    direction=$3
    kind=$4
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
    [[ $# -eq 4 ]] || usage
    launch_id=$1
    direction=$2
    commit=$3
    kind=$4
    ;;
esac
[[ $launch_id =~ ^[0-9A-Za-z._-]+$ ]] || fail 1 "not a launch id: $launch_id"
if [[ $mode != prepare ]]; then
  [[ $direction =~ ^[a-z][a-z0-9_]*$ ]] || fail 1 "not an improve direction: $direction"
fi

readonly HOME_DIR=${HOME:-}
[[ $HOME_DIR == /* ]] || fail 1 'HOME must be an absolute path'
readonly LAUNCH_DIR="$HOME_DIR/improve/launch/$launch_id"
if [[ $mode != prepare ]]; then
  member_kinds_file="$LAUNCH_DIR/member_kinds.txt"
  [[ -r $member_kinds_file ]] || fail 1 "the member kind list is missing or unreadable: $member_kinds_file"
  grep -q '[^[:space:]]' "$member_kinds_file" || fail 1 "the member kind list is empty: $member_kinds_file"
  grep -Fxq -- "$kind" "$member_kinds_file" || fail 1 "not a member kind: $kind"
fi
readonly ROUND_HOME="$HOME_DIR/improve/home"
readonly RUNNER_ROOT="$HOME_DIR/improve/runner"
# The member's login file of this launch: the records `--prepare` wrote, each `NAME=value` with a NUL after it.
readonly MEMBER_CREDENTIAL="$LAUNCH_DIR/member-credential"
[[ $LAUNCH_DIR == "$HOME_DIR"/improve/launch/* ]] || fail 1 "the launch directory is not under the improve root: $LAUNCH_DIR"
if [[ $mode != prepare ]]; then
  [[ -d $LAUNCH_DIR ]] || fail 1 "the launch directory is missing: $LAUNCH_DIR"
  readonly ROUND_ROOT="$HOME_DIR/improve/$direction"
  readonly ROUND_TMP="/tmp/improve-$direction"
  readonly ROUND_TMPDIR="/tmp/improve-$direction-tmp"
  # The CODEX_HOME of a codex round. It sits under ROUND_TMP, which the write boundary already allows; only its
  # auth.json lives for one round.
  readonly CODEX_HOME_TARGET="$ROUND_TMP/codex-home"
fi

# The variables the launcher forwarded. The launcher writes only these names, and a name outside
# the list stops the round: an unlisted name would reach the agent from an input nobody reviewed.
read_launch_env() {
  local record name value
  while IFS= read -r -d '' record; do
    name=${record%%=*}
    value=${record#*=}
    case $name in
      ANTHROPIC_* | CLAUDE_* | IMPROVE_REVIEW_* | BASH_DEFAULT_TIMEOUT_MS | BASH_MAX_TIMEOUT_MS | CHARLIEBOT_SESSION_ID)
        export "$name=$value"
        ;;
      *) fail 1 "the launch sent an unexpected variable name: $name" ;;
    esac
  done <"$LAUNCH_DIR/env"
}

# Read the round's records from standard input. The review key goes into the round's credentials file under its
# credential's name. That file is replaced through a temporary file renamed over it, so a parallel launch never
# leaves it half written. The member's record (a login or a gateway token) goes into this launch's member-credential
# file, which no other launch reads. The records arrive on standard input so no key or token reaches a command line.
# The review settings are not secrets: they join the launch environment, which the first form exports. Each record is
# selected by its name.
prepare_credentials() {
  local record name value review_credential= review_key= member_record=
  local target="$ROUND_HOME/credentials.yaml" staged
  umask 077
  : >"$MEMBER_CREDENTIAL"
  while IFS= read -r -d '' record; do
    name=${record%%=*}
    value=${record#*=}
    case $name in
      CHARLIE_CODE_API_KEY) review_key=$value ;;
      CLAUDE_CODE_OAUTH_TOKEN | ANTHROPIC_AUTH_TOKEN | IMPROVE_CODEX_AUTH)
        [[ -z $member_record ]] || fail 1 'the launch sent more than one member record'
        member_record=$name
        printf '%s\0' "$record" >>"$MEMBER_CREDENTIAL"
        ;;
      IMPROVE_REVIEW_CREDENTIAL)
        review_credential=$value
        printf '%s\0' "$record" >>"$LAUNCH_DIR/env"
        ;;
      IMPROVE_REVIEW_MODEL | IMPROVE_REVIEW_API_BASE)
        printf '%s\0' "$record" >>"$LAUNCH_DIR/env"
        ;;
      *) fail 1 "the launch sent an unexpected variable name: $name" ;;
    esac
  done
  [[ -n $review_credential ]] || fail 1 'the launch named no review credential'
  [[ -n $review_key ]] || fail 1 'the launch sent no review key'
  [[ -n $member_record ]] || fail 1 'the launch sent no member record'
  mkdir -p -- "$ROUND_HOME"
  staged=$(mktemp -- "$ROUND_HOME/credentials.yaml.XXXXXXXX") \
    || fail 1 'cannot make the credentials file in the round home'
  printf '%s:\n  api_key: %s\n' "$review_credential" "$review_key" >"$staged"
  mv -f -- "$staged" "$target"
}

# The value of the first record named `name` in this launch's member-credential file. Nothing is printed and the
# status is 1 when the file holds no such record.
member_record_value() {
  local wanted=$1 record name
  [[ -f $MEMBER_CREDENTIAL ]] || return 1
  while IFS= read -r -d '' record; do
    name=${record%%=*}
    if [[ $name == "$wanted" ]]; then
      printf '%s' "${record#*=}"
      return 0
    fi
  done <"$MEMBER_CREDENTIAL"
  return 1
}

# The api_key under one top-level section of the round's credentials file, found by the section's name.
credential_key() {
  awk -v section="$1" '
    /^[^ ]/ { inside = ($0 == section ":") }
    inside && /^  api_key:/ { value = $0; sub(/^  api_key:[ \t]*/, "", value); print value; exit }
  ' "$ROUND_HOME/credentials.yaml"
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

# The other directions' clones are standalone repositories, so the host list cannot see their worktrees. The
# sourced in-flight rule supplies the same claimant and path functions, and each clone enters it as a checkout
# whose state the task store cannot name: the scanned direction's own round decides it. A clone holds one
# checkout, so its claim is the work of the branch that checkout carries — the checked-out branch's committed
# paths and the clone's dirty paths, while that round runs or a process runs in the clone, each printed in the
# list's form against the main ref this scan fetched into the clone. A branch the checkout does not carry claims
# no path, and its open pull request, when one exists, is already on the launcher's list.
other_inflight_files() {
  source "$LAUNCH_DIR/inflight-files.sh"
  local clone other merge_base main_ref origin_url current_branch
  main_ref="refs/improve/inflight/$direction"
  for clone in "$HOME_DIR"/improve/*/charlie-bot; do
    [[ -d $clone ]] || continue
    [[ $clone == "$RUNNER_ROOT/charlie-bot" ]] && continue
    other=$(basename -- "$(dirname -- "$clone")")
    [[ $other == "$direction" ]] && continue
    if ! current_branch=$(git -C "$clone" symbolic-ref --quiet --short HEAD); then
      current_branch=detached
    fi
    if round_is_live "$other"; then
      mark_worktree_state "$clone" open
    else
      mark_worktree_state "$clone" closed
    fi
    origin_url=$(git -C "$clone" remote get-url origin) || fail 1 "cannot read the origin of $clone"
    git -C "$clone" fetch --quiet --no-tags --no-write-fetch-head "$origin_url" \
      +refs/heads/main:"$main_ref" || fail 1 "cannot fetch origin/main in $clone"
    if [[ $current_branch != detached ]]; then
      if merge_base=$(git -C "$clone" merge-base "$main_ref" "$current_branch" 2>/dev/null); then
        if worktree_has_live_claimant "$clone" "$current_branch"; then
          read_main_patch_ids "$clone" "$merge_base" "$main_ref"
          list_committed_paths "$clone" "$merge_base" "$current_branch"
        fi
      else
        printf 'improve runner: skip branch %s in %s: no merge base with origin/main\n' \
          "$current_branch" "$clone" >&2
      fi
    fi
    if git -C "$clone" merge-base "$main_ref" HEAD >/dev/null 2>&1; then
      if worktree_has_live_claimant "$clone" "$current_branch"; then
        list_dirty_paths "$clone" "$current_branch"
      fi
    else
      printf 'improve runner: skip worktree %s: no merge base with origin/main\n' "$clone" >&2
    fi
  done
}

# The direction's clone is the round's only checkout inside the boundary. A round whose clone is
# absent or foreign must not start: on 2026-10-29 a local round found no clone and continued in
# another checkout although its prompt told it to stop, so the check holds here, before the round's agent starts.
check_clone() {
  local clone="$ROUND_ROOT/charlie-bot"
  local hooks origin
  [[ -d $clone ]] || fail 4 "the direction clone is missing: $clone"
  hooks=$(git -C "$clone" config --get core.hooksPath 2>/dev/null) || hooks=
  # The clone points at the runner checkout's hooks (an absolute path), so a clone from any layout runs them.
  [[ $hooks == dev/git-hooks || $hooks == */dev/git-hooks ]] \
    || fail 4 "the direction clone does not set core.hooksPath to dev/git-hooks: $clone"
  origin=$(git -C "$clone" remote get-url origin 2>/dev/null) || origin=
  [[ $(basename -- "${origin:-/nonexistent}" .git) == charlie-bot ]] \
    || fail 4 "the direction clone's origin does not point at charlie-bot: ${origin:-no origin}"
}

# Whether a live round of one direction holds its scope. The clone scan calls this for the other
# directions, and the start below calls it for its own.
#
# The evidence is the round's own: its leader, the `--in-scope` bash that the systemd-run call
# below starts from the round's launch directory, and that launch directory, which the wrapper
# removes when it exits. The leader's command line names both, so a live round shows the pair and
# a finished round does not; elapsed time and process names cannot tell the two apart, so neither
# decides here. The wrapper counts too: between the scope's start and the leader's first moment it
# is the round's only process, and a second fire of the direction must refuse it, not stop it.
round_is_live() {
  local live_direction=$1
  local proc launch_dir
  local -a arguments=()
  for proc in /proc/[0-9]*/cmdline; do
    [[ $proc != "/proc/$$/cmdline" ]] || continue
    [[ -r $proc ]] || continue
    arguments=()
    while IFS= read -r -d '' argument; do
      arguments+=("$argument")
    done <"$proc"
    (( ${#arguments[@]} >= 5 )) || continue
    [[ ${arguments[1]} == "$HOME_DIR"/improve/launch/*/remote-runner.sh ]] || continue
    case ${arguments[2]} in
      # The leader: bash <launch dir>/remote-runner.sh --in-scope <launch id> <direction> <member kind>.
      --in-scope) [[ ${arguments[4]} == "$live_direction" ]] || continue ;;
      # The --prepare form holds no direction and starts no scope.
      --prepare) continue ;;
      # The wrapper: bash <launch dir>/remote-runner.sh <launch id> <direction> <CLI commit> <member kind>.
      *) [[ ${arguments[3]} == "$live_direction" ]] || continue ;;
    esac
    launch_dir=$(dirname -- "${arguments[1]}")
    [[ -d $launch_dir ]] || continue
    return 0
  done
  return 1
}

# Whether a process's command line names this launch id as an argument. The launcher heartbeat holds it from before
# `--prepare` until the wrapper finishes, so a concurrent start cannot sweep this launch's prepared credential.
launch_is_referenced() {
  local launch=$1 proc argument
  local -a arguments=()
  for proc in /proc/[0-9]*/cmdline; do
    [[ -r $proc ]] || continue
    arguments=()
    while IFS= read -r -d '' argument; do
      arguments+=("$argument")
    done <"$proc"
    for argument in "${arguments[@]}"; do
      [[ $argument == "$launch" ]] && return 0
    done
  done
  return 1
}

# Remove what an earlier launch left behind when it died: the member-credential file of a launch whose processes
# are gone, and the round's Codex auth.json when no live round holds the direction. Both files hold a login, so
# they do not wait for a later start of their own launch. This runs before the round's scope starts, so a live
# round's files stay.
sweep_launch_files() {
  local member launch
  for member in "$HOME_DIR"/improve/launch/*/member-credential; do
    [[ -f $member ]] || continue
    launch=$(basename -- "$(dirname -- "$member")")
    if ! launch_is_referenced "$launch"; then
      rm -f -- "$member"
    fi
  done
  if ! round_is_live "$direction"; then
    rm -f -- "$CODEX_HOME_TARGET/auth.json"
  fi
}

# The hooks of a round call `charliebot`, and the round's boundary and mock data come from the same
# commit. The three programs build together from the commit this host's runtime clone runs, in a
# checkout the runner owns. Their build inputs are the runner's own too: CARGO_HOME, TMPDIR, and the
# origin that the launcher sent from the runtime clone. No round's write allow-list covers any of
# them, so a round cannot change the code that runs outside its boundary.
#
# The directions start in a burst after a release, so two rounds can need the same missing commit at
# once. Every write below -- the clone, its fetch and checkout, the cargo build, the target
# directory, the installed tools and their links -- runs under one lock per runner root, whose
# holder's exit releases it. The check for the tools runs again after the wait, so the round whose
# commit the holder already built skips the build.
ensure_cli() {
  local commit=$1
  local bin_dir="$RUNNER_ROOT/bin"
  local checkout="$RUNNER_ROOT/charlie-bot"
  local release_dir="$RUNNER_ROOT/target/release"
  local cli="$bin_dir/charliebot-$commit"
  local sandbox="$bin_dir/improve-sandbox-$commit"
  local mock_home="$bin_dir/mock-home-$commit"
  local commit_bin="$bin_dir/$commit"
  local origin lock_fd
  mkdir -p -- "$RUNNER_ROOT"
  exec {lock_fd}>>"$RUNNER_ROOT/build.lock"
  flock "$lock_fd" || fail 1 "cannot lock the runner's build state: $RUNNER_ROOT/build.lock"
  mkdir -p -- "$bin_dir" "$RUNNER_ROOT/cargo-home" "$RUNNER_ROOT/tmp"
  if [[ ! -x $cli || ! -x $sandbox || ! -x $mock_home ]]; then
    if [[ ! -d $checkout/.git ]]; then
      origin=$(cat -- "$LAUNCH_DIR/origin")
      git -C "$HOME_DIR" clone --quiet -- "$origin" "$checkout" \
        || fail 1 "cannot clone the runner's checkout: $checkout"
    fi
    git -C "$checkout" fetch --quiet origin "$commit" || fail 1 "cannot fetch commit $commit into $checkout"
    git -C "$checkout" -c advice.detachedHead=false checkout --quiet --detach "$commit" \
      || fail 1 "cannot check out commit $commit in $checkout"
    ( cd "$checkout/rust" &&
      CARGO_HOME="$RUNNER_ROOT/cargo-home" TMPDIR="$RUNNER_ROOT/tmp" \
      CARGO_BUILD_JOBS=$BUILD_JOBS CARGO_TARGET_DIR="$RUNNER_ROOT/target" \
        nice -n 19 cargo build --locked --release \
          -p charliebot-cli --bin charliebot \
          -p charliebot-improve-sandbox --bin improve-sandbox \
          -p charliebot-mock-home --bin mock-home ) \
      || fail 1 "cannot build the round tools at commit $commit in $checkout"
    install -m 755 -- "$release_dir/charliebot" "$cli"
    install -m 755 -- "$release_dir/improve-sandbox" "$sandbox"
    install -m 755 -- "$release_dir/mock-home" "$mock_home"
  fi
  ln -sfn -- "charliebot-$commit" "$bin_dir/charliebot"
  ln -sfn -- "improve-sandbox-$commit" "$bin_dir/improve-sandbox"
  ln -sfn -- "mock-home-$commit" "$bin_dir/mock-home"
  # The per-commit links pin a round's command lookup while another launch updates the shared links.
  mkdir -p -- "$commit_bin"
  ln -sfn -- "../charliebot-$commit" "$commit_bin/charliebot"
  ln -sfn -- "../improve-sandbox-$commit" "$commit_bin/improve-sandbox"
  ln -sfn -- "../mock-home-$commit" "$commit_bin/mock-home"
  flock -u "$lock_fd"
}

# mock-home writes manifest.json last and renames its staging directory into place only when the home is complete.
ensure_mock_home() {
  local commit=$1
  local mock_home_bin="$RUNNER_ROOT/bin/mock-home-$commit"
  local version mock_home_root mock_home_dir lock_fd output
  if ! version=$("$mock_home_bin" version 2>&1); then
    fail 1 "cannot read mock-home version from $mock_home_bin: ${version:-mock-home version returned no diagnostic}"
  fi
  [[ $version =~ ^[[:alnum:]_.-]+$ && $version != . && $version != .. ]] \
    || fail 1 "mock-home version is not a directory name: $version"
  mock_home_root="$HOME_DIR/improve/mock-home"
  mock_home_dir="$mock_home_root/$version"
  mkdir -p -- "$mock_home_root" || fail 1 "cannot create the shared mock-home directory: $mock_home_root"
  exec {lock_fd}>>"$RUNNER_ROOT/mock-home.lock"
  flock "$lock_fd" || fail 1 "cannot lock the shared mock home: $RUNNER_ROOT/mock-home.lock"

  if [[ -e $mock_home_dir ]]; then
    [[ -d $mock_home_dir ]] || fail 1 "the shared mock home is not a directory: $mock_home_dir"
    [[ -s $mock_home_dir/manifest.json ]] \
      || fail 1 "the shared mock home is incomplete; missing manifest: $mock_home_dir/manifest.json"
    flock -u "$lock_fd" || fail 1 "cannot unlock the shared mock home: $RUNNER_ROOT/mock-home.lock"
    return
  fi

  if ! output=$("$mock_home_bin" generate "$mock_home_dir" 2>&1); then
    fail 1 "cannot generate the shared mock home $mock_home_dir: ${output:-mock-home generate returned no diagnostic}"
  fi
  [[ -s $mock_home_dir/manifest.json ]] \
    || fail 1 "mock-home generate returned success without a complete manifest: $mock_home_dir/manifest.json"
  printf '%s\n' "$output"
  flock -u "$lock_fd" || fail 1 "cannot unlock the shared mock home: $RUNNER_ROOT/mock-home.lock"
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

# Export each round key under the name its tool reads. The review key is CHARLIE_CODE_API_KEY whichever member
# runs the round. charlie-code takes the review option's model and API base from CHARLIE_CODE_MODEL and
# CHARLIE_CODE_API_BASE, in the gateway's form: the model with an `openai/` prefix, and the base with `/v1` appended.
# The member's own credential is its record in the launch's member-credential file: ANTHROPIC_AUTH_TOKEN for a gateway
# member, CLAUDE_CODE_OAUTH_TOKEN for a subscription claude, and for a codex round IMPROVE_CODEX_AUTH, which becomes the
# round's auth.json under CODEX_HOME. The review key comes from the credentials file that prepare wrote. The values
# enter the round's environment here, with no argument and no other file that carries them.
export_round_credentials() {
  local review_key member_key
  review_key=$(credential_key "$IMPROVE_REVIEW_CREDENTIAL")
  [[ -n $review_key ]] \
    || fail 1 "the round's credentials file holds no api_key for the review credential $IMPROVE_REVIEW_CREDENTIAL: $ROUND_HOME/credentials.yaml"
  export CHARLIE_CODE_API_KEY=$review_key
  export CHARLIE_CODE_MODEL="openai/$IMPROVE_REVIEW_MODEL"
  export CHARLIE_CODE_API_BASE="$IMPROVE_REVIEW_API_BASE/v1"
  if [[ $kind == codex ]]; then
    prepare_codex_home
    export CODEX_HOME=$CODEX_HOME_TARGET
    return
  fi
  if member_key=$(member_record_value ANTHROPIC_AUTH_TOKEN); then
    export ANTHROPIC_AUTH_TOKEN=$member_key
  else
    export CLAUDE_CODE_OAUTH_TOKEN=$(member_record_value CLAUDE_CODE_OAUTH_TOKEN)
  fi
}

# A directory the round writes under: a real directory that this runner's user owns. A symbolic link or another
# owner's directory stops the round before it writes a login.
check_runner_directory() {
  local path=$1
  [[ ! -L $path ]] || fail 1 "$path is a symbolic link, so the round does not write its Codex login under it"
  [[ -d $path ]] || fail 1 "$path is not a directory"
  [[ $(stat -c %u -- "$path") == "$(id -u)" ]] || fail 1 "$path is not owned by the runner's user"
}

# Validate an existing Codex path before a start sweep can remove auth.json through it. A missing path is made by the
# runner after this check; a dangling symbolic link still fails the -L check.
check_existing_runner_directory() {
  local path=$1
  [[ ! -L $path ]] || fail 1 "$path is a symbolic link, so the round does not write its Codex login under it"
  if [[ -e $path ]]; then
    check_runner_directory "$path"
  fi
}

# The round's Codex home: the auth.json of this round's member-credential record, mode 600, in a directory this
# round creates. The directory stays after the round; the round's auth.json goes when the round ends.
prepare_codex_home() {
  local auth
  auth=$(member_record_value IMPROVE_CODEX_AUTH) || fail 1 "the launch holds no Codex login: $MEMBER_CREDENTIAL"
  check_runner_directory "$ROUND_TMP"
  if [[ -e $CODEX_HOME_TARGET ]]; then
    check_runner_directory "$CODEX_HOME_TARGET"
  else
    ( umask 077 && mkdir -- "$CODEX_HOME_TARGET" ) || fail 1 "cannot create the Codex home: $CODEX_HOME_TARGET"
    check_runner_directory "$CODEX_HOME_TARGET"
  fi
  # noclobber makes the write create the file: it refuses an existing file or link, so the file is the new one.
  rm -f -- "$CODEX_HOME_TARGET/auth.json"
  ( umask 077 && set -C && printf '%s\n' "$auth" >"$CODEX_HOME_TARGET/auth.json" ) \
    || fail 1 "cannot write the round's Codex login: $CODEX_HOME_TARGET/auth.json"
  chmod 600 "$CODEX_HOME_TARGET/auth.json"
}

# The process ids of this scope that run the round's review: a process whose command line names charlie-code as its
# program, or as the script its interpreter runs. Prints one id per line, and fails when the scope's process list cannot
# be read, because the review's end is then unknown.
scope_review_pids() {
  local cgroup procs pid argument
  local -a arguments=()
  cgroup=$(sed -n 's/^0:://p' /proc/self/cgroup)
  procs="/sys/fs/cgroup$cgroup/cgroup.procs"
  [[ -n $cgroup && -r $procs ]] || return 1
  while read -r pid; do
    mapfile -d '' -t arguments <"/proc/$pid/cmdline" 2>/dev/null || continue
    for argument in "${arguments[@]:0:2}"; do
      if [[ ${argument##*/} == charlie-code ]]; then
        printf '%s\n' "$pid"
        break
      fi
    done
  done <"$procs"
}

run_in_scope() {
  # The marker tells the heartbeat that this launch owns the scope: a refusal leaves no marker,
  # so a heartbeat that closes cannot stop the previous round's scope.
  : >"$LAUNCH_DIR/started"
  empty_tmpdir
  export_round_credentials
  local status=0 review
  "$RUNNER_ROOT/bin/improve-sandbox" "$direction" -- "$kind" "${REWRITTEN[@]}" || status=$?
  record_memory_peak
  # A review that outlives its agent never reaches the pull request, so the round fails by name. Other leftovers keep
  # the handling of a stale scope at the start (above).
  review=$(scope_review_pids | paste -sd ' ') \
    || fail 1 "cannot list the processes of the round's scope, so the review's end cannot be checked"
  if [[ -n $review ]]; then
    printf '%s the review still runs in the scope after the agent exited: charlie-code pid %s; the round fails\n' \
      "$STDERR_PREFIX" "$review" >&2
    (( status != 0 )) || status=1
  fi
  exit "$status"
}

if [[ $mode == prepare ]]; then
  prepare_credentials
  exit 0
fi

if [[ $mode == in_scope ]]; then
  rewrite_command
  run_in_scope
fi

[[ $commit =~ ^[0-9a-f]{40}$ ]] || fail 1 "not a full commit: $commit"

# The round's dependencies, checked before any side effect of the round: the member's login, the review key that
# prepare wrote, and this host's lack of a live profile home. Each failure exits 7 and names the missing item.
check_round_dependencies() {
  [[ -n ${IMPROVE_REVIEW_CREDENTIAL-} ]] || fail 7 'the launch names no review credential'
  [[ -n $(credential_key "$IMPROVE_REVIEW_CREDENTIAL") ]] \
    || fail 7 "the round's credentials file holds no api_key for the review credential $IMPROVE_REVIEW_CREDENTIAL"
  if [[ $kind == claude ]]; then
    { member_record_value ANTHROPIC_AUTH_TOKEN || member_record_value CLAUDE_CODE_OAUTH_TOKEN; } >/dev/null \
      || fail 7 "the round has no Claude login: $MEMBER_CREDENTIAL holds neither ANTHROPIC_AUTH_TOKEN" \
        "nor CLAUDE_CODE_OAUTH_TOKEN"
  else
    member_record_value IMPROVE_CODEX_AUTH >/dev/null \
      || fail 7 "the round has no Codex login: $MEMBER_CREDENTIAL holds no IMPROVE_CODEX_AUTH record"
  fi
  [[ ! -e $HOME_DIR/.charliebot ]] \
    || fail 7 "$HOME_DIR/.charliebot exists; the round's profile home is $ROUND_HOME, and no live profile home may sit beside it"
}

# The write boundary must start before the round does. A kernel below Landlock ABI 6 makes improve-sandbox refuse at
# exec.
check_boundary_starts() {
  local output
  output=$("$RUNNER_ROOT/bin/improve-sandbox" "$direction" -- true 2>&1) \
    || fail 7 "the write boundary does not start, so the round does not start: ${output:-improve-sandbox printed no diagnostic}"
}

# Whether this launch owns the round's Codex auth.json. It turns on right before this launch's scope starts, and off again
# when systemd-run refuses the scope as already loaded: a refused launch never owns the live round's file.
OWNS_CODEX_AUTH=0
cleanup_launch_dir() {
  [[ $LAUNCH_DIR == "$HOME_DIR"/improve/launch/* ]] && rm -rf -- "$LAUNCH_DIR"
  if (( OWNS_CODEX_AUTH )); then
    rm -f -- "$CODEX_HOME_TARGET/auth.json"
  fi
  return 0
}
trap cleanup_launch_dir EXIT

read_launch_env
check_round_dependencies
install_pattern_file
rewrite_command
other_inflight_files >>"$LAUNCH_DIR/inflight-files.txt"
check_clone

# The round works in the direction's root, because `--resume` finds a Claude session by the
# directory. The PATH puts the remote binaries first: claude, codex and charlie-code from ~/.local/bin,
# cargo from ~/.cargo/bin, and the charliebot CLI that the hooks call from the runner's bin.
export CHARLIEBOT_HOME="$ROUND_HOME"
export CLAUDE_CONFIG_DIR="$HOME_DIR/.claude-improve"
export TMPDIR="$ROUND_TMPDIR"
export IMPROVE_LAUNCH_DIR="$LAUNCH_DIR"
export IMPROVE_DIRECTION="$direction"
export PATH="$RUNNER_ROOT/bin:$HOME_DIR/.local/bin:$HOME_DIR/.cargo/bin:${PATH:-/usr/local/bin:/usr/bin:/bin}"
check_existing_runner_directory "$ROUND_TMP"
mkdir -p -- "$ROUND_ROOT/evidence" "$ROUND_ROOT/clc-sessions" "$ROUND_TMP" "$ROUND_TMPDIR" "$ROUND_HOME" "$CLAUDE_CONFIG_DIR"
check_runner_directory "$ROUND_TMP"
check_existing_runner_directory "$CODEX_HOME_TARGET"
if [[ ! -e $ROUND_ROOT/memory-peaks.tsv ]]; then
  printf 'launch\tmemory_peak_bytes\n' >"$ROUND_ROOT/memory-peaks.tsv"
fi
cd "$ROUND_ROOT"

ensure_cli "$commit"
check_boundary_starts
export PATH="$RUNNER_ROOT/bin/$commit:$PATH"

# A failed scope is dead: an OOM kill or another fatal end leaves the unit failed, and systemd-run
# then refuses the same unit name forever, so the runner clears it before the start.
#
# An active scope holds either a live round or a finished one whose scope stayed loaded: on
# 2026-11-01 a detached mock-home helper kept improve-perf.scope active for 36 minutes after its
# round had exited cleanly, and every later fire of the direction refused with `already loaded`.
# A live round keeps the refusal below; a stale scope is stopped first, and the stop takes every
# process in the cgroup, because the leftover process is the reason the unit survived.
if systemctl --user is-failed --quiet "improve-$direction.scope"; then
  systemctl --user reset-failed "improve-$direction.scope"
  printf '%s cleared the failed scope improve-%s.scope\n' "$STDERR_PREFIX" "$direction" >&2
elif systemctl --user is-active --quiet "improve-$direction.scope"; then
  if round_is_live "$direction"; then
    fail 5 "the previous round still runs: unit improve-$direction.scope exists"
  fi
  cgroup=$(systemctl --user show --property=ControlGroup --value "improve-$direction.scope" 2>/dev/null) || cgroup=
  procs=
  if [[ -n $cgroup ]]; then
    procs=$(paste -sd ' ' "/sys/fs/cgroup$cgroup/cgroup.procs" 2>/dev/null) || procs=
  fi
  systemctl --user stop "improve-$direction.scope"
  if [[ -n $procs ]]; then
    printf '%s stopped the stale scope improve-%s.scope: its round is over, and the leftover processes %s went with it\n' \
      "$STDERR_PREFIX" "$direction" "$procs" >&2
  else
    printf '%s stopped the stale scope improve-%s.scope: its round is over\n' "$STDERR_PREFIX" "$direction" >&2
  fi
fi
sweep_launch_files
ensure_mock_home "$commit"
# The scope bounds the round's memory: the round and every process it starts, the trial server
# included, die inside it instead of pushing another owner's build into swap. The scope's stderr
# is held in a file, so a refused scope can be named, and it is printed once the scope ends.
scope_stderr="$LAUNCH_DIR/scope.stderr"
scope_status=0
if [[ $kind == codex ]]; then
  OWNS_CODEX_AUTH=1
fi
systemd-run --user --scope --unit "improve-$direction" \
  -p "MemoryMax=$UNIT_MEMORY_MAX" -p MemorySwapMax=0 -- \
  bash "$LAUNCH_DIR/remote-runner.sh" --in-scope "$launch_id" "$direction" "$kind" 2>"$scope_stderr" || scope_status=$?
cat -- "$scope_stderr" >&2 || true
if (( scope_status != 0 )); then
  if grep -q 'already loaded' "$scope_stderr"; then
    OWNS_CODEX_AUTH=0
    fail 5 "the previous round still runs: unit improve-$direction.scope exists"
  fi
  exit "$scope_status"
fi
