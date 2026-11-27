#!/usr/bin/env bash
# Run one improve round on the remote host.
#
#   remote-runner.sh <launch id> <direction> <CLI commit> <member kind> <member version> <review kind> <review version>
#   remote-runner.sh --in-scope <launch id> <direction> <member kind>
#   remote-runner.sh --prepare <launch id>     (the member's and the review's logins, with their login records, on
#                                               standard input)
#   remote-runner.sh --member-cli <member kind> <member version>     (a probe: install and report, no round)
#   remote-runner.sh --round-state <launch id>                      (the launch's round: running, done <status>, lost)
#   remote-runner.sh --round-output <launch id> <offset>            (the round's output from a byte offset)
#   remote-runner.sh --round-release <launch id>                    (remove the round's record)
#
# The member kind is `claude` or `codex`: the agent the round runs. The member version is the version of that CLI that
# the server host runs; the runner installs exactly that version from the vendor and runs it, and no other CLI. The
# review kind and version name the CLI of the review backend, which the launcher read on its host the same way.
#
# dev/improve/remote-launch.sh sends this file, with the round's arguments and inputs, into
# ~/improve/launch/<launch id>/ and runs the remaining forms over ssh. `--prepare` reads the round's
# records from its standard input and stores both logins in the launch's member-credential file (mode 600), which
# only this launch reads. Two optional records, `MEMBER_LOGIN` and `REVIEW_LOGIN`, carry a subscription login's config
# directory and token expiry instead of a token; `--prepare` writes them to the launch's `logins` file, which the
# server's hold check reads to keep a renewal away from a login a live round holds. The first form checks the round's
# dependencies, prepares the round's environment, then starts the round's scope through systemd-run; the scope runs
# the `--in-scope` form, which makes the round's TMPDIR and removes the TMPDIR directories that earlier scopes made,
# runs the member's agent under the Landlock boundary that limits its writes and signals, and records the scope's
# memory peak. The review's login is written into the launch's review directory once the round has its scope, and the
# review's `charliebot backend run` reads it from there. No round environment holds either login. The probe answer that
# the launcher shipped is the file CHARLIEBOT_TRIAL_PROBE_ANSWER names in the round's environment: a serve trial reads
# the probe member from it, since this host runs no server.
#
# The kit is the dev/ tree of the commit that the round's charliebot is built from. The runner archives that tree into
# the commit's tool directory, and the round reads and runs the kit from there, so a kit change reaches rounds at the
# release switch like the binary does. The launch names the revision its kit files came from, and the start refuses a
# launch whose kit revision differs from the commit: a round runs one revision throughout.
#
# The skills are the skills/ tree of that same commit, archived beside the kit as IMPROVE_SKILLS_DIR, so the rule text a
# round reads matches its kit. The launch names the skills revision, and the start refuses a launch whose skills
# revision differs from the commit, as it does for the kit. The agent's stream-json is also written to the launch's
# round-events.ndjson, which github-post.sh pr-merge reads for skill reads. The round's start stamp, IMPROVE_ROUND_START,
# names its evidence directory, and the runner keeps there the round's facts that a merge outside the round reads: the
# in-flight list, the event log at the round's end and the kit revision (keep_round_facts). A codex round's discovery
# lists skills from the same directory only: codex-skill-roots.sh links its codex home to it and turns off the host's
# skill roots, and the round's start refuses a round whose codex would still list another skill path.
#
# Exit codes: the agent's exit code; 1 a local failure, such as a missing input file or a refused
# scope, or a round whose review still runs in the scope after its agent exited (stderr names the
# review); 4 the direction's clone is missing, is not this protocol's clone, or holds uncommitted work
# that blocks its checkout of origin/main; 5 the previous round of this direction still runs; 7 a round
# dependency is missing (a login, the review's login, the round's home, the write boundary, the browser or a member
# or review CLI), the kit, the skills and the charliebot name different revisions, or a codex round's discovery would
# list a skill outside the round's skills directory, and stderr names it. A member or review CLI exits 7 when the
# vendor install or the installed `--version` check fails, and stderr names the local version and the version the host
# runs. Nothing starts on 4, 5 and 7.
#
# The round reads its direction from this invocation, so the scope name, the boundary and TMPDIR
# name one direction only. Two rounds of one direction never run together: the scope name refuses
# the second one. A killed round leaves its scope in the failed state, so the runner clears a
# failed scope of the same name before the start. The round ends the scope it started: every
# process the round left in it stops, and the runner names them, so a finished round leaves no
# loaded unit behind. A scope a finished round left loaded is stale: a process that outlived the
# round, a test run's child for instance, keeps the unit active. The start reads the scope's own
# processes: a live round's leader in that list refuses the round with exit 5, and anything else
# there is a leftover, so the runner stops the scope, every process in the cgroup included.
#
# The clone sync and check run before the CLI build, so a refused clone starts no build. The runner deletes
# its launch directory when it exits; the round's evidence is a separate directory, which the
# launcher copies back after this script has ended.
#
# The round's record is ~/improve/results/<launch id>/: the runner's standard output and standard error and the exit
# status, written last. The record belongs to the launch, not to the connection that carries it, so a launcher whose
# connection drops reads the round's status and remaining output from it. The `--round-*` forms read the record:
# `--round-state` prints `running` while the runner runs, `done <status>` once the record holds a status, and `lost` when
# the runner ended without one; `--round-output` prints the output after an offset; `--round-release` removes the record
# once the launcher has read it. A record that no launcher released is removed a day after its status. The launcher sends
# this file on standard input to each `--round-*` call, so the calls need no copy of the runner on the host.
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
  printf 'usage: %s <launch id> <direction> <CLI commit> <member kind> <member version> <review kind> <review version>\n' \
    "${BASH_SOURCE[0]}" >&2
  printf '       %s --in-scope <launch id> <direction> <member kind>\n' "${BASH_SOURCE[0]}" >&2
  printf '       %s --prepare <launch id>\n' "${BASH_SOURCE[0]}" >&2
  printf '       %s --member-cli <member kind> <member version>\n' "${BASH_SOURCE[0]}" >&2
  printf '       %s --round-state <launch id>\n' "${BASH_SOURCE[0]}" >&2
  printf '       %s --round-output <launch id> <offset>\n' "${BASH_SOURCE[0]}" >&2
  printf '       %s --round-release <launch id>\n' "${BASH_SOURCE[0]}" >&2
  exit 1
}

mode=run
commit=
kind=
member_version=
review_kind=
review_version=
offset=
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
  --member-cli)
    [[ $# -eq 3 ]] || usage
    mode=member_cli
    launch_id=
    direction=
    kind=$2
    member_version=$3
    ;;
  --round-state)
    [[ $# -eq 2 ]] || usage
    mode=round_state
    launch_id=$2
    direction=
    ;;
  --round-release)
    [[ $# -eq 2 ]] || usage
    mode=round_release
    launch_id=$2
    direction=
    ;;
  --round-output)
    [[ $# -eq 3 ]] || usage
    mode=round_output
    launch_id=$2
    direction=
    offset=$3
    ;;
  '')
    usage
    ;;
  *)
    [[ $# -eq 7 ]] || usage
    launch_id=$1
    direction=$2
    commit=$3
    kind=$4
    member_version=$5
    review_kind=$6
    review_version=$7
    ;;
esac
if [[ $mode != member_cli ]]; then
  # The id starts with an alphanumeric, so no id is `.` or `..`: a record or launch directory named by one would be
  # the improve root itself.
  [[ $launch_id =~ ^[0-9A-Za-z][0-9A-Za-z._-]*$ ]] || fail 1 "not a launch id: $launch_id"
fi
if [[ $mode == round_output ]]; then
  [[ $offset =~ ^[0-9]+$ ]] || fail 1 "not a byte offset: $offset"
fi
if [[ $mode == run || $mode == in_scope ]]; then
  [[ $direction =~ ^[a-z][a-z0-9_]*$ ]] || fail 1 "not an improve direction: $direction"
fi

readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly HOME_DIR=${HOME:-}
[[ $HOME_DIR == /* ]] || fail 1 'HOME must be an absolute path'
readonly LAUNCH_DIR="$HOME_DIR/improve/launch/$launch_id"
readonly RESULTS_ROOT="$HOME_DIR/improve/results"
readonly RESULT_DIR="$RESULTS_ROOT/$launch_id"
# The round's event log: the stream-json its agent writes, which the scope keeps beside the launch's other files.
readonly ROUND_EVENTS="$LAUNCH_DIR/round-events.ndjson"
if [[ $mode == run || $mode == in_scope ]]; then
  member_kinds_file="$LAUNCH_DIR/member_kinds.txt"
  [[ -r $member_kinds_file ]] || fail 1 "the member kind list is missing or unreadable: $member_kinds_file"
  grep -q '[^[:space:]]' "$member_kinds_file" || fail 1 "the member kind list is empty: $member_kinds_file"
  grep -Fxq -- "$kind" "$member_kinds_file" || fail 1 "not a member kind: $kind"
fi
if [[ $mode == run ]]; then
  grep -Fxq -- "$review_kind" "$member_kinds_file" || fail 1 "not a review CLI kind: $review_kind"
fi
readonly ROUND_HOME="$HOME_DIR/improve/home"
readonly RUNNER_ROOT="$HOME_DIR/improve/runner"
# The login records of this launch: the member's record and the review's record that `--prepare` wrote, each `NAME=value`
# with a NUL after it.
readonly MEMBER_CREDENTIAL="$LAUNCH_DIR/member-credential"
# The logins a live round holds: one `<config dir>TAB<expiresAt ms>` line per subscription login this launch uses. The
# server's hold check reads it from here; it carries no token.
readonly LOGINS="$LAUNCH_DIR/logins"
[[ $LAUNCH_DIR == "$HOME_DIR"/improve/launch/* ]] || fail 1 "the launch directory is not under the improve root: $LAUNCH_DIR"
if [[ $mode == run || $mode == in_scope ]]; then
  [[ -d $LAUNCH_DIR ]] || fail 1 "the launch directory is missing: $LAUNCH_DIR"
  readonly ROUND_ROOT="$HOME_DIR/improve/$direction"
  readonly ROUND_TMP="/tmp/improve-$direction"
  readonly ROUND_TMPDIR="/tmp/improve-$direction-tmp"
  # The CODEX_HOME of a codex round. It sits under ROUND_TMP, which the write boundary already allows; only its
  # auth.json lives for one round.
  readonly CODEX_HOME_TARGET="$ROUND_TMP/codex-home"
  # The CODEX_HOME of a codex review, beside the member's: the review's login sits there for its one run.
  readonly REVIEW_CODEX_HOME="$ROUND_TMP/review-codex-home"
fi

# The variables the launcher forwarded. The launcher writes only these names, and a name outside
# the list stops the round: an unlisted name would reach the agent from an input nobody reviewed.
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

# Read the round's records from standard input and store them in this launch's member-credential file, which no other
# launch reads. The member's record is a login or a gateway token; the review's record is the review backend's login.
# Each record is selected by its name, and the records arrive on standard input so no key or token reaches a command
# line. Neither record joins the round environment. The optional `MEMBER_LOGIN` and `REVIEW_LOGIN` records carry a
# subscription login's config directory and token expiry, and they become the launch's `logins` file, which the
# server's hold check reads. That file carries no token, so it holds only these two records.
prepare_credentials() {
  local record name member_record= review_record= member_login= review_login=
  umask 077
  : >"$MEMBER_CREDENTIAL"
  while IFS= read -r -d '' record; do
    name=${record%%=*}
    case $name in
      CLAUDE_CODE_OAUTH_TOKEN | ANTHROPIC_AUTH_TOKEN | IMPROVE_CODEX_AUTH)
        [[ -z $member_record ]] || fail 1 'the launch sent more than one member record'
        member_record=$name
        ;;
      REVIEW_ANTHROPIC_AUTH_TOKEN | REVIEW_CLAUDE_CODE_OAUTH_TOKEN | REVIEW_CODEX_AUTH)
        [[ -z $review_record ]] || fail 1 'the launch sent more than one review record'
        review_record=$name
        ;;
      MEMBER_LOGIN)
        [[ -z $member_login ]] || fail 1 'the launch sent more than one member login record'
        member_login=${record#*=}
        continue
        ;;
      REVIEW_LOGIN)
        [[ -z $review_login ]] || fail 1 'the launch sent more than one review login record'
        review_login=${record#*=}
        continue
        ;;
      *) fail 1 "the launch sent an unexpected variable name: $name" ;;
    esac
    printf '%s\0' "$record" >>"$MEMBER_CREDENTIAL"
  done
  [[ -n $member_record ]] || fail 1 'the launch sent no member record'
  [[ -n $review_record ]] || fail 1 'the launch sent no review record'
  # The logins file appears whole or not at all: a prepare that refuses a record leaves no file, and a launch with no
  # subscription login keeps none, so the server's hold check reads the file that this call completed.
  if [[ -z $member_login && -z $review_login ]]; then
    rm -f -- "$LOGINS"
  else
    local staged="$LOGINS.partial"
    : >"$staged"
    [[ -z $member_login ]] || login_line "$member_login" MEMBER_LOGIN >>"$staged"
    [[ -z $review_login ]] || login_line "$review_login" REVIEW_LOGIN >>"$staged"
    mv -f -- "$staged" "$LOGINS"
  fi
}

# One `logins` line from a login record: `<config dir>TAB<expiresAt ms>`. A record without both stops the prepare.
login_line() {
  local value=$1 name=$2 dir expires
  [[ $value == *$'\t'* ]] || fail 1 "the launch's $name record names no config directory and token expiry"
  dir=${value%%$'\t'*}
  expires=${value#*$'\t'}
  [[ -n $dir ]] || fail 1 "the launch's $name record names no config directory"
  [[ $expires =~ ^[0-9]+$ ]] || fail 1 "the launch's $name record names no whole-number token expiry"
  printf '%s\t%s\n' "$dir" "$expires"
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
# checkout, so its claim is the work of the branch that checkout carries — the paths that the checked-out branch's
# unmerged commits change, and the clone's dirty paths, while that round runs or a process runs in the clone, each
# printed in the list's form against the main ref this scan fetched into the clone. A branch the checkout does not
# carry claims no path, and its open pull request, when one exists, is already on the launcher's list.
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

# Whether a live round of one direction runs anywhere on the host. The clone scan calls this for the
# other directions, and the start's login sweep for its own. It also counts the wrapper, which runs
# before its scope starts; the start itself reads the scope's own processes
# (scope_list_holds_round), because only a process inside the scope can hold the unit.
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
    done 2>/dev/null <"$proc" || continue
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

# The processes of one direction's scope: the pids the unit's cgroup holds, as one space-separated line. Every
# process the scope holds is in that list, and a process outside it cannot hold the unit. Fails when the unit's
# cgroup cannot be read, so the caller decides what an unreadable scope means.
scope_processes() {
  local cgroup procs
  cgroup=$(systemctl --user show --property=ControlGroup --value "improve-$1.scope" 2>/dev/null) || cgroup=
  [[ -n $cgroup && -r /sys/fs/cgroup$cgroup/cgroup.procs ]] || return 1
  procs=$(paste -sd ' ' "/sys/fs/cgroup$cgroup/cgroup.procs") || return 1
  printf '%s' "$procs"
}

# Whether one direction's scope holds a live round: a process in that list runs the round's leader,
# `bash <launch dir>/remote-runner.sh --in-scope <launch id> <direction> <member kind>`. A round runs inside its
# scope from its first moment, so its leader is in the list while the round runs. This is narrower than
# round_is_live, which also counts a wrapper that has not started its scope yet: such a wrapper holds no scope, so
# it is no evidence that one holds a round.
scope_list_holds_round() {
  local live_direction=$1 pid
  local -a arguments=()
  for pid in $2; do
    [[ -r /proc/$pid/cmdline ]] || continue
    arguments=()
    while IFS= read -r -d '' argument; do
      arguments+=("$argument")
    done 2>/dev/null <"/proc/$pid/cmdline" || continue
    (( ${#arguments[@]} >= 5 )) || continue
    [[ ${arguments[1]} == "$HOME_DIR"/improve/launch/*/remote-runner.sh ]] || continue
    [[ ${arguments[2]} == --in-scope ]] || continue
    [[ ${arguments[4]} == "$live_direction" ]] || continue
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
    done 2>/dev/null <"$proc" || continue
    for argument in "${arguments[@]}"; do
      [[ $argument == "$launch" ]] && return 0
    done
  done
  return 1
}

# Remove what an earlier launch left behind when it died: the login records of a launch whose processes are gone, and
# the round's Codex auth.json files when no live round holds the direction. Every file here holds a login, so they do
# not wait for a later start of their own launch. This runs before the round's scope starts, so a live round's files stay.
sweep_launch_files() {
  local member review launch
  for member in "$HOME_DIR"/improve/launch/*/member-credential; do
    [[ -f $member ]] || continue
    launch=$(basename -- "$(dirname -- "$member")")
    if ! launch_is_referenced "$launch"; then
      rm -f -- "$member"
    fi
  done
  for review in "$HOME_DIR"/improve/launch/*/review/login; do
    [[ -f $review ]] || continue
    launch=$(basename -- "$(dirname -- "$(dirname -- "$review")")")
    if ! launch_is_referenced "$launch"; then
      rm -f -- "$review"
    fi
  done
  if ! round_is_live "$direction"; then
    rm -f -- "$CODEX_HOME_TARGET/auth.json" "$REVIEW_CODEX_HOME/auth.json"
  fi
}

# The hooks of a round call `charliebot`, and the round's boundary and mock data come from the same
# commit. The three programs build together from the commit this host's runtime clone runs, in a
# checkout the runner owns. Their build inputs are the runner's own too: CARGO_HOME, TMPDIR, and the
# origin that the launcher sent from the runtime clone. No round's write allow-list covers any of
# them, so a round cannot change the code that runs outside its boundary. The kit, the dev/ tree
# of the same commit, is copied beside them by install_commit_kit.
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
  local lock_fd
  mkdir -p -- "$RUNNER_ROOT"
  exec {lock_fd}>>"$RUNNER_ROOT/build.lock"
  flock "$lock_fd" || fail 1 "cannot lock the runner's build state: $RUNNER_ROOT/build.lock"
  mkdir -p -- "$bin_dir" "$RUNNER_ROOT/cargo-home" "$RUNNER_ROOT/tmp"
  if [[ ! -x $cli || ! -x $sandbox || ! -x $mock_home ]]; then
    ensure_runner_checkout
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
  # The round's gh shim and the scanner it runs come from this commit, so a round checks its writes with the code of
  # the CLI it runs. The kit, the dev/ tree of this commit, is the code the round reads and runs, and the skills, the
  # skills/ tree of this commit, are the rule text it reads. Both are copied once per commit: a later round cannot
  # change the copy that a live round runs.
  if [[ ! -f $commit_bin/gh || ! -f $commit_bin/check-skills-host-leak.sh || ! -f $commit_bin/leak-patterns.sh \
    || ! -d $commit_bin/kit/dev || ! -d $commit_bin/skills ]]; then
    ensure_runner_checkout
    git -C "$checkout" fetch --quiet "$(cat -- "$LAUNCH_DIR/origin")" "$commit" \
      || fail 1 "cannot fetch commit $commit from the launcher's origin into $checkout"
    install_commit_file "$commit" dev/improve/gh-shim.sh "$commit_bin/gh" 755
    install_commit_file "$commit" dev/check-skills-host-leak.sh "$commit_bin/check-skills-host-leak.sh" 755
    install_commit_file "$commit" dev/leak-patterns.sh "$commit_bin/leak-patterns.sh" 644
    install_commit_tree "$commit" dev "$commit_bin/kit/dev"
    install_commit_tree "$commit" skills "$commit_bin/skills"
  fi
  prune_commit_entries "$commit"
  flock -u "$lock_fd"
}

# The runner's checkout, cloned from the launcher's origin on the first use. The checkout only builds the round tools.
ensure_runner_checkout() {
  if [[ ! -d $RUNNER_ROOT/charlie-bot/.git ]]; then
    git -C "$HOME_DIR" clone --quiet -- "$(cat -- "$LAUNCH_DIR/origin")" "$RUNNER_ROOT/charlie-bot" \
      || fail 1 "cannot clone the runner's checkout: $RUNNER_ROOT/charlie-bot"
  fi
}

# Copy one file of a commit into the commit's tool directory. The file is read from the commit's objects, so the runner's
# checkout keeps its own state. The copy is staged beside its destination and renamed over it.
install_commit_file() {  # <commit> <path in the commit> <destination> <mode>
  local staged
  staged=$(mktemp -- "$3.XXXXXXXX") || fail 1 "cannot make a staged copy beside $3"
  git -C "$RUNNER_ROOT/charlie-bot" show "$1:$2" > "$staged" \
    && chmod "$4" -- "$staged" && mv -f -- "$staged" "$3" \
    || { rm -f -- "$staged"; fail 1 "cannot install $2 of commit $1 as $3"; }
}

# Copy one directory tree of a commit to a destination, read from the commit's objects. The tree is staged beside its
# destination and renamed over it, so an interrupted copy never passes for a finished tree.
install_commit_tree() {  # <commit> <directory in the commit> <destination>
  local staged
  mkdir -p -- "$(dirname -- "$3")" || fail 1 "cannot make the directory of $3"
  staged=$(mktemp -d -- "$(dirname -- "$3")/.tree.XXXXXXXX") || fail 1 "cannot make a staged copy beside $3"
  git -C "$RUNNER_ROOT/charlie-bot" archive --format=tar "$1" "$2" | tar -x -C "$staged" \
    && rm -rf -- "$3" && mv -T -- "$staged/$2" "$3" \
    || { rm -rf -- "$staged"; fail 1 "cannot install $2 of commit $1 as $3"; }
  rm -rf -- "$staged"
}

# The arguments after the script's path of each live process that runs remote-runner.sh, one line per process, each line
# wrapped in spaces. A round's wrapper holds its commit, member kind and member version for as long as the round runs,
# and a probe holds its member kind and version, so these lines show which installed artifacts a live process uses.
live_runner_arguments() {
  local proc argument
  local -a arguments=()
  for proc in /proc/[0-9]*/cmdline; do
    [[ -r $proc ]] || continue
    arguments=()
    while IFS= read -r -d '' argument; do
      arguments+=("$argument")
    done 2>/dev/null <"$proc" || continue
    (( ${#arguments[@]} >= 2 )) || continue
    [[ ${arguments[1]} == */remote-runner.sh ]] || continue
    printf ' %s \n' "${arguments[*]:2}"
  done
}

# Whether a live process runs remote-runner.sh with every one of the given words among its arguments.
runner_names() {
  local line word all
  while IFS= read -r line; do
    all=1
    for word in "$@"; do
      [[ $line == *" $word "* ]] || all=0
    done
    if (( all )); then
      return 0
    fi
  done < <(live_runner_arguments)
  return 1
}

# The member CLI version line that a binary prints for `--version`, or nothing when no binary exists at that path. A
# binary that exists and fails prints its own error on this runner's stderr, and the caller names the version as none.
installed_member_version() {
  local binary=$1 output
  [[ -x $binary ]] || return 0
  output=$("$binary" --version </dev/null) || return 0
  printf '%s' "${output%%$'\n'*}"
}

# The line that `--version` prints for one member CLI version. claude prints `X.Y.Z (Claude Code)`,
# codex prints `codex-cli X.Y.Z`.
member_version_line() {
  case $1 in
    claude) printf '%s (Claude Code)' "$2" ;;
    codex) printf 'codex-cli %s' "$2" ;;
  esac
}

# Exit 1 unless the file's sha256 digest is the one the vendor publishes for it.
check_sha256() {
  local file=$1 expected=$2 actual
  actual=$(sha256sum -- "$file" | cut -d' ' -f1) || return 1
  if [[ $actual != "$expected" ]]; then
    printf '%s %s has sha256 %s, and the vendor publishes %s\n' "$STDERR_PREFIX" "$file" "$actual" "$expected" >&2
    return 1
  fi
}

# Codex: the whole standalone package of the release `rust-v<version>`, unpacked with its layout. Tool calls start
# bin/codex-code-mode-host and bin/codex-exec-server beside bin/codex, and codex-path/ and codex-resources/ sit beside
# them, so the install copies nothing out of the package. The digest is the one GitHub publishes for the package asset.
install_codex() {
  local version=$1 arch=$2 stage=$3
  local asset="codex-package-$arch-unknown-linux-musl.tar.gz"
  local digest
  local release="https://api.github.com/repos/openai/codex/releases/tags/rust-v$version"
  digest=$(curl -fsSL --connect-timeout 30 --max-time 300 "$release" \
    | jq -r --arg name "$asset" '.assets[] | select(.name == $name) | .digest // empty') || return 1
  if [[ $digest != sha256:* ]]; then
    printf '%s the release rust-v%s publishes no sha256 digest for %s\n' "$STDERR_PREFIX" "$version" "$asset" >&2
    return 1
  fi
  curl -fsSL --connect-timeout 30 --max-time 900 -o "$stage/package.tar.gz" \
    "https://github.com/openai/codex/releases/download/rust-v$version/$asset" || return 1
  check_sha256 "$stage/package.tar.gz" "${digest#sha256:}" || return 1
  tar -xzf "$stage/package.tar.gz" -C "$stage" || return 1
  rm -f -- "$stage/package.tar.gz" || return 1
}

# Claude: the native build that the Claude Code release bucket publishes for the version. The version's manifest names
# each platform's binary and its sha256 digest.
install_claude() {
  local version=$1 arch=$2 stage=$3
  local platform base digest
  case $arch in
    aarch64) platform=linux-arm64 ;;
    x86_64) platform=linux-x64 ;;
    *)
      printf '%s the Claude Code release has no build for %s\n' "$STDERR_PREFIX" "$arch" >&2
      return 1
      ;;
  esac
  base="https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819"
  base="$base/claude-code-releases/$version"
  digest=$(curl -fsSL --connect-timeout 30 --max-time 300 "$base/manifest.json" \
    | jq -r --arg platform "$platform" '.platforms[$platform].checksum // empty') || return 1
  if [[ ! $digest =~ ^[0-9a-f]{64}$ ]]; then
    printf '%s the Claude Code %s manifest publishes no sha256 digest for %s\n' \
      "$STDERR_PREFIX" "$version" "$platform" >&2
    return 1
  fi
  curl -fsSL --connect-timeout 30 --max-time 900 -o "$stage/claude" "$base/$platform/claude" || return 1
  check_sha256 "$stage/claude" "$digest" || return 1
  mkdir -- "$stage/bin" || return 1
  mv -- "$stage/claude" "$stage/bin/claude" || return 1
  chmod 755 -- "$stage/bin/claude" || return 1
}

# Install one member CLI version from the vendor into the stage directory, which then holds bin/<kind>.
install_member_cli() {
  local kind=$1 version=$2 stage=$3 arch
  arch=$(uname -m) || return 1
  case $kind in
    codex) install_codex "$version" "$arch" "$stage" ;;
    claude) install_claude "$version" "$arch" "$stage" ;;
  esac
}

# The member CLI that a round runs: the version of its kind that the server host runs, installed from the vendor into
# $RUNNER_ROOT/members/<kind>-<version>/. The round's PATH puts that bin first, so no other CLI on this host runs. The
# build lock serializes the installs of rounds that start together. The install goes into a stage directory that is
# renamed into place, so an interrupted install never passes for a finished one. A version that cannot be installed, or
# whose installed binary reports another version, exits 7 and names the local version and the version the host runs.
ensure_member_cli() {
  local kind=$1 version=$2
  local members="$RUNNER_ROOT/members"
  local name="$kind-$version"
  local lock_fd stage reported expected
  [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail 7 "the member CLI version is not x.y.z: '$version'"
  mkdir -p -- "$RUNNER_ROOT" || fail 1 "cannot create the runner root: $RUNNER_ROOT"
  exec {lock_fd}>>"$RUNNER_ROOT/build.lock"
  flock "$lock_fd" || fail 1 "cannot lock the runner's build state: $RUNNER_ROOT/build.lock"
  mkdir -p -- "$members" || fail 1 "cannot create the member CLI directory: $members"
  if [[ ! -x $members/$name/bin/$kind ]]; then
    stage=$(mktemp -d "$members/.stage-$name.XXXXXXXX") || fail 1 "cannot make an install directory under $members"
    if ! install_member_cli "$kind" "$version" "$stage"; then
      rm -rf -- "$stage"
      fail 7 "member CLI $kind: local version $version, improve host version none; the vendor install failed"
    fi
    mv -T -- "$stage" "$members/$name" || fail 1 "cannot move the install into $members/$name"
  fi
  expected=$(member_version_line "$kind" "$version")
  reported=$(installed_member_version "$members/$name/bin/$kind")
  [[ $reported == "$expected" ]] || fail 7 \
    "member CLI $kind: local version $version, improve host version ${reported:-none}; the CLI reports another version"
  ln -sfn -- "$name" "$members/.current-$kind" || fail 1 "cannot record $name as the current $kind"
  prune_member_cache
  flock -u "$lock_fd" || fail 1 "cannot unlock the runner's build state: $RUNNER_ROOT/build.lock"
}

# Remove the member CLI directories that no live process names, except the last version of each kind: that one stays
# so the next round of its kind reuses it without a download. The stage directories are removed too, because only a
# lock holder creates them.
prune_member_cache() {
  local members="$RUNNER_ROOT/members" dir name kind
  rm -rf -- "$members"/.stage-*
  for dir in "$members"/*-*; do
    [[ -d $dir ]] || continue
    name=${dir##*/}
    kind=${name%%-*}
    if [[ $(readlink -- "$members/.current-$kind") == "$name" ]]; then
      continue
    fi
    if runner_names "$kind" "${name#*-}"; then
      continue
    fi
    rm -rf -- "$dir"
  done
}

# Remove the round tools of the commits that no live process names, except the commit this round runs. A commit's tools
# rebuild when a round next needs them, so only live rounds and the current commit keep theirs.
prune_commit_entries() {
  local keep=$1 bin_dir="$RUNNER_ROOT/bin" entry name commit
  for entry in "$bin_dir"/*; do
    name=${entry##*/}
    if [[ $name =~ ^(charliebot|improve-sandbox|mock-home)-([0-9a-f]{40})$ ]]; then
      commit=${BASH_REMATCH[2]}
    elif [[ $name =~ ^[0-9a-f]{40}$ ]]; then
      commit=$name
    else
      continue
    fi
    if [[ $commit == "$keep" ]]; then
      continue
    fi
    if runner_names "$commit"; then
      continue
    fi
    rm -rf -- "$entry"
  done
}

# The probe form: install one member CLI version on this host and print the version that the installed CLI reports. It
# reads no launch and starts no round.
member_cli_probe() {
  local binary
  [[ -r $SCRIPT_DIR/member_kinds.txt ]] || fail 1 "the member kind list is missing: $SCRIPT_DIR/member_kinds.txt"
  grep -Fxq -- "$kind" "$SCRIPT_DIR/member_kinds.txt" || fail 7 "not a member kind: $kind"
  ensure_member_cli "$kind" "$member_version"
  binary="$RUNNER_ROOT/members/$kind-$member_version/bin/$kind"
  printf 'improve runner: member CLI %s %s is installed at %s and reports: %s\n' \
    "$kind" "$member_version" "$binary" "$(installed_member_version "$binary")"
}

# mock-home writes manifest.json last and renames its staging directory into place only when the home is complete. The
# shared home is the task-tree shape, whose boot marker lets a serve trial's start skip each session that holds no open
# task, run or worktree, so the start logs no node failure.
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

  if ! output=$("$mock_home_bin" generate "$mock_home_dir" --shape serve-task-tree 2>&1); then
    fail 1 "cannot generate the shared mock home $mock_home_dir: ${output:-mock-home generate returned no diagnostic}"
  fi
  [[ -s $mock_home_dir/manifest.json ]] \
    || fail 1 "mock-home generate returned success without a complete manifest: $mock_home_dir/manifest.json"
  printf '%s\n' "$output"
  flock -u "$lock_fd" || fail 1 "cannot unlock the shared mock home: $RUNNER_ROOT/mock-home.lock"
}

# Make this scope's TMPDIR: a directory that mktemp creates under the session TMPDIR. A killed round leaves its
# copies in the directories that earlier scopes made, so those directories go, and nothing else in the session
# TMPDIR is removed. The scope name keeps another round of this direction out, so no live round uses them.
prepare_scope_tmpdir() {
  local earlier tmpdir
  for earlier in "$ROUND_TMPDIR"/scope.*; do
    [[ -d $earlier && ! -L $earlier ]] || continue
    rm -rf -- "$earlier" || fail 1 "cannot remove the earlier scope's TMPDIR: $earlier"
  done
  tmpdir=$(mktemp -d "$ROUND_TMPDIR/scope.XXXXXXXX") || fail 1 "cannot make this scope's TMPDIR under $ROUND_TMPDIR"
  export TMPDIR=$tmpdir
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

# Export the member's own credential under the name its CLI reads. The review's login is not exported: it reaches only the
# review's own process, through the file that write_review_login writes. The member's credential is its record in the
# launch's member-credential file: ANTHROPIC_AUTH_TOKEN for a gateway member, CLAUDE_CODE_OAUTH_TOKEN for a subscription
# claude, and for a codex round IMPROVE_CODEX_AUTH, which becomes the round's auth.json under CODEX_HOME. The values
# enter the round's environment here, with no argument and no other file that carries them.
export_round_credentials() {
  local member_key
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

# Write the review's login where its `charliebot backend run --launch <launch>/review` reads it. `review/login` holds the
# one NAME=VALUE line that the review's process environment gets, mode 600. A gateway review's key goes into the round's
# credentials file under the credential that review/option.json names, because the gateway backend reads its key from
# that file; its login then stays empty. Runs in the scope, before the agent starts, so the scope's own launch owns the
# files it writes.
write_review_login() {
  local login="$LAUNCH_DIR/review/login" value credential staged
  umask 077
  if value=$(member_record_value REVIEW_ANTHROPIC_AUTH_TOKEN); then
    credential=$(jq -er '.option.credential' "$LAUNCH_DIR/review/option.json") \
      || fail 1 "the review backend names no credential: $LAUNCH_DIR/review/option.json"
    staged=$(mktemp -- "$ROUND_HOME/credentials.yaml.XXXXXXXX") \
      || fail 1 'cannot make the credentials file in the round home'
    printf '%s:\n  api_key: %s\n' "$credential" "$value" >"$staged"
    mv -f -- "$staged" "$ROUND_HOME/credentials.yaml"
    : >"$login"
  elif value=$(member_record_value REVIEW_CLAUDE_CODE_OAUTH_TOKEN); then
    printf 'CLAUDE_CODE_OAUTH_TOKEN=%s\n' "$value" >"$login"
  elif value=$(member_record_value REVIEW_CODEX_AUTH); then
    write_codex_home "$REVIEW_CODEX_HOME" "$value"
    prepare_codex_skill_roots "$REVIEW_CODEX_HOME"
    printf 'CODEX_HOME=%s\n' "$REVIEW_CODEX_HOME" >"$login"
  else
    fail 7 "the launch holds no review login: $MEMBER_CREDENTIAL"
  fi
  chmod 600 -- "$login"
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

# A CODEX_HOME directory that this runner's user makes under the round's TMP, holding the Codex login as its auth.json,
# mode 600. The directory stays after the round; the auth.json goes when the round ends. The first argument is the
# directory and the second the auth.json text.
write_codex_home() {
  local home=$1 auth=$2
  check_runner_directory "$ROUND_TMP"
  if [[ -e $home ]]; then
    check_runner_directory "$home"
  else
    ( umask 077 && mkdir -- "$home" ) || fail 1 "cannot create the Codex home: $home"
    check_runner_directory "$home"
  fi
  # noclobber makes the write create the file: it refuses an existing file or link, so the file is the new one.
  rm -f -- "$home/auth.json"
  ( umask 077 && set -C && printf '%s\n' "$auth" >"$home/auth.json" ) \
    || fail 1 "cannot write the round's Codex login: $home/auth.json"
  chmod 600 "$home/auth.json"
}

# The round's own Codex home: the member's Codex login record, written for a codex member's round.
prepare_codex_home() {
  local auth
  auth=$(member_record_value IMPROVE_CODEX_AUTH) || fail 1 "the launch holds no Codex login: $MEMBER_CREDENTIAL"
  write_codex_home "$CODEX_HOME_TARGET" "$auth"
  prepare_codex_skill_roots "$CODEX_HOME_TARGET"
}

# The process ids of this scope that run the round's review: a process whose command line runs `charliebot backend run`,
# the command that step 6 of the landing protocol starts. The charliebot named is the program, or the script its
# interpreter runs. Prints one id per line, and fails when the scope's process list cannot be read, because the review's
# end is then unknown.
scope_review_pids() {
  local cgroup procs pid index program
  local -a arguments=()
  cgroup=$(sed -n 's/^0:://p' /proc/self/cgroup)
  procs="/sys/fs/cgroup$cgroup/cgroup.procs"
  [[ -n $cgroup && -r $procs ]] || return 1
  while read -r pid; do
    mapfile -d '' -t arguments <"/proc/$pid/cmdline" 2>/dev/null || continue
    for index in 0 1; do
      program=${arguments[index]-}
      if [[ ${program##*/} == charliebot && ${arguments[index + 1]-} == backend && ${arguments[index + 2]-} == run ]]; then
        printf '%s\n' "$pid"
        break
      fi
    done
  done <"$procs"
}

# Whether this launch's wrapper runs: its run form, or its in-scope form that the wrapper's systemd-run starts. The check
# reads the command lines, so the state query that names the launch id is not the wrapper.
launch_wrapper_runs() {
  local proc argument
  local -a arguments=()
  for proc in /proc/[0-9]*/cmdline; do
    [[ -r $proc ]] || continue
    arguments=()
    while IFS= read -r -d '' argument; do
      arguments+=("$argument")
    done 2>/dev/null <"$proc" || continue
    (( ${#arguments[@]} >= 3 )) || continue
    [[ ${arguments[1]} == "$LAUNCH_DIR/remote-runner.sh" ]] || continue
    if [[ ${arguments[2]} == "$launch_id" ]]; then
      return 0
    fi
    if [[ ${arguments[2]} == --in-scope && ${arguments[3]-} == "$launch_id" ]]; then
      return 0
    fi
  done
  return 1
}

# The round's state, as the launcher's reattach reads it: `running` while the wrapper runs, `done <status>` once the record
# holds the status, and `lost` when the wrapper has ended with no status. The wrapper is checked first: it writes its status
# before it ends, so a wrapper that has ended already has its status in place.
print_round_state() {
  if launch_wrapper_runs; then
    printf 'running\n'
  elif [[ -s $RESULT_DIR/status ]]; then
    printf 'done %s\n' "$(<"$RESULT_DIR/status")"
  else
    printf 'lost\n'
  fi
}

# The round's output from the byte offset that the launcher names, to the end of the record's output.
print_round_output() {
  [[ -f $RESULT_DIR/output ]] || fail 1 "the round's record holds no output: $RESULT_DIR/output"
  tail -c "+$(( offset + 1 ))" -- "$RESULT_DIR/output"
}

# Remove this launch's record once the launcher has read the round's result. Only a record under the results root goes.
release_round_record() {
  [[ $RESULT_DIR == "$RESULTS_ROOT"/?* ]] || fail 1 "the round's record is not under the results root: $RESULT_DIR"
  rm -rf -- "$RESULT_DIR"
}

# Remove the records of rounds whose status is a day old. A launcher releases its record once it has read the result, so
# this sweep takes the records that no launcher read.
sweep_round_records() {
  local status
  while IFS= read -r -d '' status; do
    rm -rf -- "${status%/status}"
  done < <(find "$RESULTS_ROOT" -mindepth 2 -maxdepth 2 -name status -mtime +1 -print0)
}

# Open this launch's record and send the wrapper's standard output and standard error through it. Each tee writes the
# record and then the connection. A tee keeps writing the record after the connection closes its end, so a dropped
# connection neither stops the wrapper nor reaches the round.
open_round_record() {
  ( umask 077 && mkdir -p -- "$RESULTS_ROOT" ) || fail 1 "cannot create the round records directory: $RESULTS_ROOT"
  sweep_round_records
  ( umask 077 && mkdir -- "$RESULT_DIR" ) || fail 1 "cannot make the round's record: $RESULT_DIR"
  exec > >(tee --output-error=warn-nopipe -a -- "$RESULT_DIR/output")
  record_out_tee=$!
  exec 2> >(tee --output-error=warn-nopipe -a -- "$RESULT_DIR/stderr" >&2)
  record_err_tee=$!
}

# Close this launch's record: the wrapper's streams end, the tees write what they hold, and the exit status lands last, so
# a reader that finds the status finds the whole output. A tee that failed leaves no status, which reads as `lost`.
finish_round_record() {  # <exit status>
  local status=$1 out_status=0 err_status=0
  [[ -n ${record_out_tee-} ]] || return 0
  exec >&- 2>&-
  wait "$record_out_tee" || out_status=$?
  wait "$record_err_tee" || err_status=$?
  (( out_status == 0 && err_status == 0 )) || return 0
  printf '%s\n' "$status" >"$RESULT_DIR/status.partial"
  mv -f -- "$RESULT_DIR/status.partial" "$RESULT_DIR/status"
}

# round_evidence_dir: the round's evidence directory, which its start stamp names. The launch sets the stamp before
# its scope starts, so the scope and the launch's end name the same directory.
round_evidence_dir() {
  printf '%s\n' "$ROUND_ROOT/evidence/$IMPROVE_ROUND_START"
}

run_in_scope() {
  # The round's evidence directory is new for each round, so an earlier round's facts are never overwritten. It is made
  # before the marker, so a refused directory leaves no marker and the launch keeps no facts.
  [[ ! -e $(round_evidence_dir) ]] || fail 1 "the round's evidence directory already exists: $(round_evidence_dir)"
  mkdir -- "$(round_evidence_dir)"
  # The marker tells the heartbeat that this launch owns the scope: a refusal leaves no marker,
  # so a heartbeat that closes cannot stop the previous round's scope.
  : >"$LAUNCH_DIR/started"
  # The scope admits one round of the direction, so no build of it runs during the trim. The marker comes first, so a
  # signal during the trim ends the scope as it does during the round.
  source "$LAUNCH_DIR/target-cache.sh"
  trim_target_cache "$HOME_DIR" "$direction"
  prepare_scope_tmpdir
  export_round_credentials
  write_review_login
  local status=0 review
  # The agent's stream goes to the event log as well as to the runner's stdout: pr-merge reads the round's tool calls
  # from it. The log exists even when the agent writes nothing.
  : >>"$ROUND_EVENTS"
  "$RUNNER_ROOT/bin/improve-sandbox" "$direction" -- "$kind" "${REWRITTEN[@]}" | tee -a "$ROUND_EVENTS" || status=$?
  record_memory_peak
  # A review that outlives its agent never reaches the pull request, so the round fails by name. Other leftovers keep
  # the handling of a stale scope at the start (above).
  review=$(scope_review_pids | paste -sd ' ') \
    || fail 1 "cannot list the processes of the round's scope, so the review's end cannot be checked"
  if [[ -n $review ]]; then
    printf '%s the review still runs in the scope after the agent exited: charliebot backend run pid %s; the round fails\n' \
      "$STDERR_PREFIX" "$review" >&2
    (( status != 0 )) || status=1
  fi
  exit "$status"
}

if [[ $mode == prepare ]]; then
  prepare_credentials
  exit 0
fi

if [[ $mode == member_cli ]]; then
  member_cli_probe
  exit 0
fi

if [[ $mode == in_scope ]]; then
  rewrite_command
  source "$LAUNCH_DIR/codex-skill-roots.sh"
  run_in_scope
fi

case $mode in
  round_state) print_round_state; exit 0 ;;
  round_output) print_round_output; exit 0 ;;
  round_release) release_round_record; exit 0 ;;
esac

[[ $commit =~ ^[0-9a-f]{40}$ ]] || fail 1 "not a full commit: $commit"

# The kit and the skills that the round reads and the charliebot that it calls come from one revision. The launch names
# the revision that each came from; a launch that names another revision than the commit of its charliebot stops the
# round before any side effect, and stderr names both revisions. The arguments are the launch's revision file and
# what that file holds.
check_launch_revision() {
  local revision
  [[ -r $LAUNCH_DIR/$1 ]] || fail 1 "the launch holds no $2 revision: $LAUNCH_DIR/$1"
  revision=$(<"$LAUNCH_DIR/$1")
  [[ $revision == "$commit" ]] || fail 7 \
    "the launch names revision $revision for the $2 and $commit for charliebot; a round needs one revision for both"
}

# The round's dependencies, checked before any side effect of the round: the review backend's record and login, the
# probe answer, the member's login, and this host's lack of a live profile home. Each failure exits 7 and names the
# missing item.
check_round_dependencies() {
  [[ -f $LAUNCH_DIR/review/option.json ]] \
    || fail 7 "the launch holds no review backend record: $LAUNCH_DIR/review/option.json"
  [[ -f $LAUNCH_DIR/probe/option.json ]] \
    || fail 7 "the launch holds no probe answer: $LAUNCH_DIR/probe/option.json"
  { member_record_value REVIEW_ANTHROPIC_AUTH_TOKEN || member_record_value REVIEW_CLAUDE_CODE_OAUTH_TOKEN \
    || member_record_value REVIEW_CODEX_AUTH; } >/dev/null \
    || fail 7 "the round has no review login: $MEMBER_CREDENTIAL holds no REVIEW_ record"
  if [[ $kind == claude ]]; then
    { member_record_value ANTHROPIC_AUTH_TOKEN || member_record_value CLAUDE_CODE_OAUTH_TOKEN; } >/dev/null \
      || fail 7 "the round has no Claude login: $MEMBER_CREDENTIAL holds neither ANTHROPIC_AUTH_TOKEN" \
        "nor CLAUDE_CODE_OAUTH_TOKEN"
  else
    member_record_value IMPROVE_CODEX_AUTH >/dev/null \
      || fail 7 "the round has no Codex login: $MEMBER_CREDENTIAL holds no IMPROVE_CODEX_AUTH record"
  fi
  [[ ! -e $HOME_DIR/.charliebot ]] \
    || fail 7 "a live profile home sits on this host: move $HOME_DIR/.charliebot off the host user's home," \
      "for example into a dated scratch directory; the round's profile home $ROUND_HOME needs no change"
}

# The write boundary must start before the round does. A kernel below Landlock ABI 6 makes improve-sandbox refuse at
# exec.
check_boundary_starts() {
  local output
  output=$("$RUNNER_ROOT/bin/improve-sandbox" "$direction" -- true 2>&1) \
    || fail 7 "the write boundary does not start, so the round does not start: ${output:-improve-sandbox printed no diagnostic}"
}

# The Playwright build whose Chrome every round tool starts. Build 1248 is Google Chrome for Testing 156.0.8078.4, the
# build the improve host holds under the runner's own home. A Playwright update that drops this build stops every
# round at its start, which names the path that is missing.
readonly BROWSER_BUILD='chromium-1248'

# The environment inputs through which repository code and round tools pick a Chrome binary. The round environment sets
# each of them to the round's browser. landing.md's "The round's browser" section lists the same names: change both
# together.
readonly ROUND_BROWSER_INPUTS=(
  IMPROVE_BROWSER
  LIVE_SHOT_CHROME
  POOR_NETWORK_CHROME
  CHARLIEBOT_TEST_CHROME_BIN
  CHROME_BIN
  README_CAPTURE_CHROME
)

# The round's browser: the one executable Chrome of the pinned build under the runner's own home. A round takes no
# browser from PATH, because the system chromium is a snap wrapper, and snap-confine cannot start under the boundary.
resolve_round_browser() {
  local build_dir="$HOME_DIR/.cache/ms-playwright/$BROWSER_BUILD" candidate
  local -a found=()
  for candidate in "$build_dir"/chrome-linux*/chrome; do
    [[ -x $candidate ]] && found+=("$candidate")
  done
  case ${#found[@]} in
    1) printf '%s' "${found[0]}" ;;
    0) fail 7 "the round's browser is missing: no executable Chrome at $build_dir/chrome-linux*/chrome" ;;
    *) fail 7 "the round's browser is ambiguous: ${found[*]}" ;;
  esac
}

# Start the round's browser once under the write boundary, with the round's TMPDIR, and refuse the round when it does
# not start: stderr names the path and the error. The profile is a directory this check makes and removes, so the check
# leaves no file behind. A headless launch that prints the page's HTML and exits 0 is a start.
check_browser_starts() {  # <browser>
  local browser=$1 scratch output status=0 error
  scratch=$(mktemp -d -- "$ROUND_TMPDIR/browser-check.XXXXXXXX") \
    || fail 1 "cannot make the browser check's directory under $ROUND_TMPDIR"
  output=$(timeout 60 "$RUNNER_ROOT/bin/improve-sandbox" "$direction" -- "$browser" --headless=new --no-sandbox \
    --disable-gpu --no-first-run --user-data-dir="$scratch/profile" --dump-dom about:blank 2>"$scratch/stderr") \
    || status=$?
  error=$(tail -n 4 -- "$scratch/stderr" | tr '\n' ' ')
  rm -rf -- "$scratch"
  if (( status != 0 )) || [[ $output != *'<html'* ]]; then
    fail 7 "the round's browser does not start: $browser exited $status: ${error:-no stderr}"
  fi
}

# End this launch's scope. A round can leave processes behind it -- a test run whose child outlived it, a review
# that outlived its agent -- and a process left in the scope keeps the unit loaded after the round, so the next
# fire of the direction finds the unit. The scope ends with the round: every process in its cgroup stops, and the
# message names them. Only the launch that started the scope ends it: the leader's `started` marker proves this
# launch reached the scope, and a refused launch leaves no marker, so a live round's scope, another direction's,
# and the trial server of another round stay.
end_round_scope() {
  local procs
  [[ -e $LAUNCH_DIR/started ]] || return 0
  systemctl --user is-active --quiet "improve-$direction.scope" || return 0
  procs=$(scope_processes "$direction") || procs=
  if ! systemctl --user stop "improve-$direction.scope"; then
    printf "%s cannot end the round's scope improve-%s.scope\n" "$STDERR_PREFIX" "$direction" >&2
    return 0
  fi
  if [[ -n $procs ]]; then
    printf "%s ended the round's scope improve-%s.scope: the processes %s it left went with it\n" \
      "$STDERR_PREFIX" "$direction" "$procs" >&2
  else
    printf "%s ended the round's scope improve-%s.scope\n" "$STDERR_PREFIX" "$direction" >&2
  fi
}

# keep_round_facts: copy the round's facts from the launch directory into the round's evidence directory, which outlives
# the launch directory. A merge outside the round reads these copies: the in-flight list as the round read it, the event
# log as it stands at the round's end, and the kit revision, which names the round's skills.
keep_round_facts() {
  local fact
  for fact in inflight-files.txt round-events.ndjson kit-revision; do
    cp -- "$LAUNCH_DIR/$fact" "$(round_evidence_dir)/$fact" || return 1
  done
}

# Whether this launch owns the round's Codex auth.json files, the member's and the review's. It turns on right before this
# launch's scope starts, and off again when systemd-run refuses the scope as already loaded: a refused launch never owns
# the live round's files.
OWNS_CODEX_AUTH=0
SCOPE_PID=
cleanup_launch_dir() {
  local exit_status=$?
  end_round_scope
  if [[ -n $SCOPE_PID ]]; then
    kill "$SCOPE_PID" 2>/dev/null || true
    wait "$SCOPE_PID" 2>/dev/null || true
  fi
  # The scope has ended, so the event log is the round's final one. A failed copy is named here, and a merge outside the
  # round then refuses the fact that is missing.
  if [[ -e $LAUNCH_DIR/started ]]; then
    keep_round_facts || printf '%s could not keep the round facts in %s\n' "$STDERR_PREFIX" "$(round_evidence_dir)" >&2
  fi
  [[ $LAUNCH_DIR == "$HOME_DIR"/improve/launch/* ]] && rm -rf -- "$LAUNCH_DIR"
  if (( OWNS_CODEX_AUTH )); then
    rm -f -- "$CODEX_HOME_TARGET/auth.json" "$REVIEW_CODEX_HOME/auth.json"
  fi
  finish_round_record "$exit_status"
  return 0
}
open_round_record
trap cleanup_launch_dir EXIT
# A signal this wrapper can handle ends the round the way the round's own end does: the scope ends, so the member
# and every process it started stop.
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

check_launch_revision kit-revision kit
check_launch_revision skills-revision skills

# The clone's identity, sync and pre-push check. The launch copies the file as it copies the in-flight rule.
[[ -r $LAUNCH_DIR/clone-check.sh ]] \
  || fail 1 "the clone check is missing: $LAUNCH_DIR/clone-check.sh"
source "$LAUNCH_DIR/clone-check.sh"
read_launch_env
check_round_dependencies
install_pattern_file
rewrite_command
# The call runs in a subshell. A function that exits inside a redirection keeps the redirection's saved output open in this
# shell, so the round record's output would never reach its end and the wrapper would wait on it forever.
( other_inflight_files ) >>"$LAUNCH_DIR/inflight-files.txt" || exit 1
sync_clone
check_clone

# The round works in the direction's root, because `--resume` finds a Claude session by the
# directory. The PATH puts the round's member CLI first, from ensure_member_cli, then the review's CLI, so
# claude and codex resolve to the versions the server host runs and never to a copy in ~/.local/bin. Then
# come the round tools of this commit, the runner's bin with the charliebot CLI that the hooks and the review
# call, and cargo from ~/.cargo/bin.
export CHARLIEBOT_HOME="$ROUND_HOME"
# The CLI reads its checkout (prompts, gh wrapper, ssh guard) from this variable and fails without it. The round's clone
# is that checkout at origin/main. The launcher forwards no such variable: the launching host's checkout path does not
# exist on the improve host.
export CHARLIEBOT_REPO_ROOT="$ROUND_ROOT/charlie-bot"
export CLAUDE_CONFIG_DIR="$HOME_DIR/.claude-improve"
export TMPDIR="$ROUND_TMPDIR"
export IMPROVE_LAUNCH_DIR="$LAUNCH_DIR"
# The serve trial's probe member: this host runs no server to resolve backends.calls.probe, so the trial reads the answer
# that the launcher resolved and shipped.
export CHARLIEBOT_TRIAL_PROBE_ANSWER="$LAUNCH_DIR/probe/option.json"
export IMPROVE_DIRECTION="$direction"
export PATH="$RUNNER_ROOT/bin:$HOME_DIR/.local/bin:$HOME_DIR/.cargo/bin:${PATH:-/usr/local/bin:/usr/bin:/bin}"
check_existing_runner_directory "$ROUND_TMP"
mkdir -p -- "$ROUND_ROOT/evidence" "$ROUND_TMP" "$ROUND_TMPDIR" "$ROUND_HOME" "$CLAUDE_CONFIG_DIR"
check_runner_directory "$ROUND_TMP"
check_existing_runner_directory "$CODEX_HOME_TARGET"
if [[ ! -e $ROUND_ROOT/memory-peaks.tsv ]]; then
  printf 'launch\tmemory_peak_bytes\n' >"$ROUND_ROOT/memory-peaks.tsv"
fi
cd "$ROUND_ROOT"

ensure_cli "$commit"
# The round reads and runs the kit from this commit's tool directory, and the same commit's charliebot is first on PATH.
export IMPROVE_KIT_REVISION=$commit
export IMPROVE_KIT_DIR="$RUNNER_ROOT/bin/$commit/kit"
export IMPROVE_SKILLS_DIR="$RUNNER_ROOT/bin/$commit/skills"
ensure_member_cli "$kind" "$member_version"
ensure_member_cli "$review_kind" "$review_version"
check_boundary_starts
ROUND_BROWSER=$(resolve_round_browser)
check_browser_starts "$ROUND_BROWSER"
for browser_input in "${ROUND_BROWSER_INPUTS[@]}"; do
  export "$browser_input=$ROUND_BROWSER"
done
export PATH="$RUNNER_ROOT/members/$kind-$member_version/bin:$RUNNER_ROOT/members/$review_kind-$review_version/bin:$RUNNER_ROOT/bin/$commit:$PATH"

# A failed scope is dead: an OOM kill or another fatal end leaves the unit failed, and systemd-run
# then refuses the same unit name forever, so the runner clears it before the start.
#
# An active scope holds a live round of the direction, or a finished one whose scope stayed loaded.
# The scope's own process list decides: a round runs inside its scope from its first moment, so the
# round's leader is in the cgroup while the round runs, and a process outside the scope cannot hold
# the unit at all. A live round's leader keeps the refusal below; anything else in the list is a
# leftover -- on 2026-11-01 a detached mock-home helper, and on 2026-11-23 a test run whose two
# children outlived the round -- so the scope is stopped, every process in the cgroup included.
if systemctl --user is-failed --quiet "improve-$direction.scope"; then
  systemctl --user reset-failed "improve-$direction.scope"
  printf '%s cleared the failed scope improve-%s.scope\n' "$STDERR_PREFIX" "$direction" >&2
elif systemctl --user is-active --quiet "improve-$direction.scope"; then
  if ! procs=$(scope_processes "$direction"); then
    # The unit can end between the two checks: then it holds no round and nothing is left to stop.
    if systemctl --user is-active --quiet "improve-$direction.scope"; then
      fail 1 "cannot read the processes of improve-$direction.scope, so the launch cannot tell a live round from a leftover"
    fi
    procs=
  fi
  if scope_list_holds_round "$direction" "$procs"; then
    fail 5 "the previous round still runs: unit improve-$direction.scope exists"
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
# The round's start stamp names its evidence directory, and the agent reads it as IMPROVE_ROUND_START.
IMPROVE_ROUND_START=$(date -u +%Y%m%dT%H%MZ)
export IMPROVE_ROUND_START
# The scope bounds the round's memory: the round and every process it starts, the trial server
# included, die inside it instead of pushing another owner's build into swap. The scope's stderr
# is held in a file, so a refused scope can be named, and it is printed once the scope ends.
scope_stderr="$LAUNCH_DIR/scope.stderr"
scope_status=0
if [[ $kind == codex || $review_kind == codex ]]; then
  OWNS_CODEX_AUTH=1
fi
# Background + wait lets HUP, INT and TERM traps fire during the round instead of waiting for systemd-run to exit.
systemd-run --user --scope --unit "improve-$direction" \
  -p "MemoryMax=$UNIT_MEMORY_MAX" -p MemorySwapMax=0 -- \
  bash "$LAUNCH_DIR/remote-runner.sh" --in-scope "$launch_id" "$direction" "$kind" <&0 2>"$scope_stderr" &
SCOPE_PID=$!
wait "$SCOPE_PID" || scope_status=$?
SCOPE_PID=
cat -- "$scope_stderr" >&2 || true
if (( scope_status != 0 )); then
  if grep -q 'already loaded' "$scope_stderr"; then
    OWNS_CODEX_AUTH=0
    fail 5 "the previous round still runs: unit improve-$direction.scope exists"
  fi
  exit "$scope_status"
fi
