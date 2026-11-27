#!/usr/bin/env bash
# Launch one improve run on the host that can run it.
#
#   dev/improve/remote-launch.sh <agent binary> <agent arguments...>
#
# The server starts this script where it would start the agent, with the agent's arguments, environment, stdin and
# stdout. The basename of the agent binary names the member kind: `claude` or `codex`. Any other kind exits 7 before
# anything is sent.
#
# A round runs on the host named by IMPROVE_REMOTE_HOST (host config only, because the repository is public). The
# script connects there, sends the round's inputs, runs dev/improve/remote-runner.sh and passes the agent's stdout
# through unchanged; the prompt on its stdin reaches the host with its skill roots mapped (map_skill_roots). No command
# of the round runs on this host, so a round that forgets a variable, sets a wrong path or runs rm cannot reach this
# host's profile home or runtime clone: on 2026-10-04 a round's own probe deleted the whole profile home.
#
# The round path reads these inputs on this host and writes one directory:
#   - for claude, the instructions file that --append-system-prompt-file names (codex carries its instructions in its
#     arguments);
#   - the agent binary's `--version` output, which names the member CLI version that the round runs on the host;
#   - the review backend that `backends.calls.improve_review` selects, as `charliebot backend resolve` answers it,
#     and its login: a gateway option's api_key in $CHARLIEBOT_HOME/credentials.yaml, a subscription account's access
#     token, or a codex login in ~/.codex/auth.json;
#   - the first member of the `backends.calls.probe` target that names an api_base, as
#     `charliebot release-helper endpoint-option` answers it: the round's serve trial reads that answer, because the
#     improve host runs no server;
#   - the member's login: the gateway token in the environment, or a subscription member's access token in
#     $CLAUDE_CONFIG_DIR/.credentials.json, or a codex member's login in ~/.codex/auth.json;
#   - the leak pattern file $CHARLIEBOT_HOME/skills_leak_patterns.local.txt;
#   - the in-flight files list, which dev/improve/inflight-files.sh prints here;
#   - the live sessions record, which dev/improve/live-sessions.sh writes from the live server's task tree, including
#     archived rows;
#   - the kit revision: HEAD of this checkout, whose dev/ tree the runner fetches as the round's kit;
#   - the skills revision: HEAD of this checkout, whose skills/ tree the runner archives as the round's skills;
#   - the worker session's metadata.json, for task_parent_id;
#   - the cron task file of that parent session, for prompt_file;
#   - the origin URL of this launcher's checkout, which the runner clones from.
# It writes only ~/improve/<direction>/evidence/.
#
# Exit codes: the agent's exit code; 1 a local failure, such as a missing input file; 3 an ssh step to the improve host
# failed: either the host asks for a login approval, or stderr quotes ssh's error, or the bound that ended the wait
# when ssh printed nothing, or the reattach bound after a dropped connection; 4 the direction's clone is missing, is not
# this protocol's clone, or holds uncommitted work
# that blocks its checkout of origin/main; 5 the previous round of this direction still runs; 6 the round's direction
# cannot be read; 7 a round dependency is missing or dev/improve differs from HEAD, and stderr names it; 8 the member's
# login has less access-token life than a round needs, and stderr names the login and its minutes left; 9 the round's
# runner ended with no status after a dropped connection, and stderr names the launch id. Codes 4 to 8 start no agent
# and no scope, and codes 7 and 8 send nothing to the host. Codes 3 and 9 after a dropped connection come from a round that
# the launch started; the reattach contract in landing.md states when each applies. Code 7 also covers the review backend: an
# unresolved `backends.calls.improve_review`, a missing key or login, or a login with too little life; and the probe: a
# `backends.calls.probe` target with no member that names an api_base. The review never exits 8,
# because the server reads 8 as a member's short token and swaps the member. A code 7 for a member or review CLI means
# this host's `--version` output names no version; the install and its check are the runner's.
#
# The member's login is forwarded for one round: a subscription member sends its access token, the one the Claude CLI
# or Codex reads, and the refresh token stays on this host. The token reaches the host on ssh's standard input only, so
# no token value appears in any process command line or in an input file. The runner writes it into the launch's
# member-credential file (mode 600), which the runner removes when the round ends. CHARLIEBOT_RUN_TOKEN stays here:
# the remote host has no server that it could call. The review backend's login travels the same way, and the runner
# writes it into the launch's review directory for the review's own process; the round's environment never holds it.
set -euo pipefail

readonly STDERR_PREFIX='improve launch:'
# The connection gets 4 seconds, so a round that needs an approval fails with exit 3 inside 5.
readonly PREFLIGHT_SECONDS=4
# The copy-back limit. The server stops following 90 seconds after output goes silent, so a slow
# copy stops here and the next launch of this direction copies the rest.
readonly EVIDENCE_COPY_SECONDS=${IMPROVE_EVIDENCE_COPY_SECONDS:-60}
# The reattach bounds. A launch whose connection dropped asks the host every REATTACH_POLL_SECONDS, and stops waiting once the
# host has stayed unreachable for REATTACH_SECONDS. A host call that runs past REATTACH_CALL_SECONDS counts as unreachable.
readonly REATTACH_SECONDS=${IMPROVE_REATTACH_SECONDS:-1800}
readonly REATTACH_POLL_SECONDS=${IMPROVE_REATTACH_POLL_SECONDS:-30}
readonly REATTACH_CALL_SECONDS=60
# A call that sends no input of its own adds -n. Without -n, ssh reads this launcher's standard input, which
# carries the round's prompt, and the remote command drops what it does not read. Only the main run passes that
# standard input on to the remote agent.
readonly SSH=(ssh -T -o BatchMode=yes -o ConnectTimeout=10)

fail() {
  local code=$1
  shift
  printf '%s %s\n' "$STDERR_PREFIX" "$*" >&2
  exit "$code"
}

# The output markers of a host that waits for the user's approval. One classifier owns a host's login state: `classify`
# in rust/crates/features/host_auth/src/probe.rs maps a probe's output and return code to a status, `awaits_approval`
# there names the statuses that wait for approval, and rust/crates/features/cron/src/host_gate.rs holds a fire on them.
# No CLI exposes that verdict, so this launcher carries the same markers, and a change to either list belongs in both
# files.
readonly APPROVAL_MARKERS=(
  'Okta verification required'
  'activate\?user_code='
  'Permission denied \(keyboard-interactive\)'
  'requires an additional check'
  'To authenticate, visit:'
  'Host key verification failed'
)

# Whether a failed ssh call's output carries evidence that the host waits for a login approval.
host_awaits_approval() {
  local IFS='|' pattern
  pattern=${APPROVAL_MARKERS[*]}
  grep -Eq -- "$pattern" <<<"${1-}"
}

# The bounded evidence of a failed ssh call: up to five lines and 400 characters of what it printed, or `no_output`
# when it printed nothing.
ssh_failure_evidence() {
  local output=$1 no_output=$2
  if [[ -n $output ]]; then
    printf '%s' "$output" | head -n 5 | cut -c1-400
  else
    printf '%s' "$no_output"
  fi
}

# Exit 3 for a failed ssh call to the improve host, with the cause its evidence supports. An approval marker keeps the
# /host-auth instruction; any other failure states that the ssh call failed and quotes ssh's error, or the bound that
# ended the wait when ssh printed nothing. `phase` names the step, and `no_output` describes the case where the call
# printed nothing.
fail_ssh_call() {
  local phase=$1 output=$2 no_output=$3 evidence
  evidence=$(ssh_failure_evidence "$output" "$no_output")
  if host_awaits_approval "$output"; then
    if [[ -n $output ]]; then
      printf '%s\n' "$evidence" >&2
    fi
    fail 3 "the improve host $REMOTE_HOST asks for a login approval; approve the connection on the /host-auth page"
  fi
  fail 3 "the ssh call to the improve host $REMOTE_HOST failed at the $phase: $evidence"
}

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly SCRIPT_DIR=$script_dir
source "$SCRIPT_DIR/live-sessions.sh"
readonly HOME_DIR=${HOME:-}
[[ $HOME_DIR == /* ]] || fail 1 'HOME must be an absolute path'
readonly PROFILE_HOME=${CHARLIEBOT_HOME:-$HOME_DIR/.charliebot}
[[ $PROFILE_HOME == /* ]] || fail 1 'CHARLIEBOT_HOME must be an absolute path'
readonly CRON_DIR="$PROFILE_HOME/config.d/cron.d"
readonly REMOTE_HOST=${IMPROVE_REMOTE_HOST:-}
readonly SESSION_ID=${CHARLIEBOT_SESSION_ID:-}
readonly TEMP_ROOT=${TMPDIR:-/tmp}
[[ $TEMP_ROOT == /* ]] || fail 1 'TMPDIR must be an absolute path'
[[ -n $REMOTE_HOST ]] || fail 7 'IMPROVE_REMOTE_HOST is not set; it names the improve host in host config'
[[ -n $SESSION_ID ]] || fail 6 'CHARLIEBOT_SESSION_ID is not set, so the round has no direction'
# The least access-token life, in seconds, that a round may start with. The server sets it beside IMPROVE_REMOTE_HOST.
readonly TOKEN_MIN_SECONDS=${IMPROVE_TOKEN_MIN_SECONDS:-}
[[ $TOKEN_MIN_SECONDS =~ ^[0-9]+$ ]] || fail 7 'IMPROVE_TOKEN_MIN_SECONDS is not set to a whole number of seconds'
[[ $REATTACH_SECONDS =~ ^[0-9]+$ ]] || fail 1 'IMPROVE_REATTACH_SECONDS is not a whole number of seconds'
[[ $REATTACH_POLL_SECONDS =~ ^[0-9]+$ ]] || fail 1 'IMPROVE_REATTACH_POLL_SECONDS is not a whole number of seconds'

# The value of one top-level YAML key in a file, unquoted, without a trailing comment.
yaml_value() {
  local file=$1 key=$2
  sed -n "s/^$key:[[:space:]]*//p" "$file" | head -n 1 | sed 's/[[:space:]]*#.*//; s/^["'\'']//; s/["'\'']$//'
}

# The direction is the name of the task session's prompt file: `dev/improve/perf.md` gives
# `perf`. Every link is checked here because the direction names the scope, the boundary and
# TMPDIR: a guessed direction would write another direction's directories.
resolve_direction() {
  local metadata="$PROFILE_HOME/sessions/$SESSION_ID/metadata.json"
  [[ -f $metadata ]] || fail 6 "the worker session's metadata.json is missing: $metadata"
  local parent
  parent=$(sed -n 's/^[[:space:]]*"task_parent_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$metadata" | head -n 1)
  [[ -n $parent ]] || fail 6 "the worker session's metadata.json names no task_parent_id: $metadata"
  local candidate session prompt
  for candidate in "$CRON_DIR"/*.yaml; do
    [[ -f $candidate ]] || continue
    session=$(yaml_value "$candidate" session_id)
    [[ $session == "$parent" ]] || continue
    prompt=$(yaml_value "$candidate" prompt_file)
    [[ -n $prompt ]] || fail 6 "the task file names no prompt_file: $candidate"
    local name
    name=$(basename -- "$prompt" .md)
    [[ $name =~ ^[a-z][a-z0-9_]*$ ]] || fail 6 "the prompt file names no improve direction: $prompt"
    printf '%s' "$name"
    return
  done
  fail 6 "no cron task file under $CRON_DIR names session_id $parent"
}

# Copy the evidence that the previous launch of this direction did not finish copying. A killed
# launch leaves evidence on the host, and the next launch brings it back before the round starts.
# The host lists its tree first, and only the entries that the local copy lacks transfer: an absent
# directory, a file whose local copy is absent or of another size, and any other kind of entry.
# A copy that the bound stops therefore leaves the local tree closer, and the next copy transfers
# only what is still missing; a tree that is already complete costs one listing.
copy_evidence() {
  local limit=$1
  local local_evidence="$HOME_DIR/improve/$direction/evidence"
  mkdir -p -- "$local_evidence"
  local remote_evidence='$HOME/improve/'"$direction"'/evidence'
  local work
  work=$(mktemp -d "$TEMP_ROOT/improve-evidence.XXXXXXXX") || fail 1 'cannot make the evidence copy directory'
  local status=0
  set +e
  timeout --kill-after=5 "$limit" bash -c '
    set -euo pipefail
    # The local tree as kind, size and relative path, each field NUL-terminated, so a name with a
    # space or a newline stays one field.
    find "$3" -mindepth 1 -printf "%y\0%s\0%P\0" >"$4"
    declare -A sizes=()
    while IFS= read -r -d "" kind && IFS= read -r -d "" size && IFS= read -r -d "" name; do
      if [[ $kind == f ]]; then sizes["$name"]=$size; fi
    done <"$4"
    ssh -n -T -o BatchMode=yes -o ConnectTimeout=10 "$1" \
      "if [ -d \"$2\" ]; then find \"$2\" -mindepth 1 -printf \"%y\0%s\0%P\0\"; fi" >"$5"
    : >"$6"
    while IFS= read -r -d "" kind && IFS= read -r -d "" size && IFS= read -r -d "" name; do
      if [[ $kind == d ]]; then
        [[ -d "$3/$name" ]] || printf "%s\0" "$name" >>"$6"
      elif [[ $kind == f ]]; then
        [[ ${sizes["$name"]-} == "$size" ]] || printf "%s\0" "$name" >>"$6"
      else
        printf "%s\0" "$name" >>"$6"
      fi
    done <"$5"
    if [[ -s $6 ]]; then
      ssh -T -o BatchMode=yes -o ConnectTimeout=10 "$1" \
        "tar -C \"$2\" -cf - --null --no-recursion -T -" <"$6" | tar -C "$3" -xf -
    fi
  ' copy "$REMOTE_HOST" "$remote_evidence" "$local_evidence" \
        "$work/local.list" "$work/remote.list" "$work/transfer.list" 3>&-
  status=$?
  set -e
  rm -rf -- "$work"
  if (( status != 0 )); then
    printf '%s the evidence copy stopped (status %s); the next launch copies the rest\n' "$STDERR_PREFIX" "$status" >&2
  fi
}

# Whether a bounded host call did not answer: ssh's 255, or timeout's 124 and 137 for a call that ran past its bound.
host_unanswered() {
  (( $1 == 255 || $1 == 124 || $1 == 137 ))
}

# Exit 3 once the host has stayed unreachable past REATTACH_SECONDS since its last answer, which `$1` holds as a
# SECONDS reading. The message names the bound, the launch id and where the round's result is on the host.
check_reattach_bound() {
  (( SECONDS - $1 < REATTACH_SECONDS )) && return 0
  fail 3 "the improve host $REMOTE_HOST stayed unreachable for $REATTACH_SECONDS s after the connection of launch $launch_id" \
    "dropped, so this launch stopped waiting; the round runs on the host until it ends. Its result is" \
    "~/improve/results/$launch_id/status and its output ~/improve/results/$launch_id/output on $REMOTE_HOST."
}

# The connection that carried the round dropped (ssh exit 255), and the round may still run on the host under its scope. The
# launch waits for the round's end: it reads the round's state until the state holds a status, copies the output that its
# stream lacks, and sets run_status to the round's exit status. An unreachable host past the bound fails with 3, and a round
# whose runner ended with no status fails with 9. The launch never sends the round again.
reattach_round() {
  local since=$SECONDS state call_status
  while :; do
    call_status=0
    state=$(timeout --kill-after=5 "$REATTACH_CALL_SECONDS" "${SSH[@]}" "$REMOTE_HOST" \
      "bash -s -- --round-state $launch_id" <"$SCRIPT_DIR/remote-runner.sh" \
      2>"$failure_dir/reattach.stderr") || call_status=$?
    if host_unanswered "$call_status"; then
      check_reattach_bound "$since"
      sleep "$REATTACH_POLL_SECONDS"
      continue
    fi
    (( call_status == 0 )) || fail 3 "the round's state on $REMOTE_HOST failed for launch $launch_id: " \
      "$(ssh_failure_evidence "$(<"$failure_dir/reattach.stderr")" 'no output')"
    since=$SECONDS
    case $state in
      running)
        sleep "$REATTACH_POLL_SECONDS"
        ;;
      lost)
        copy_evidence "$EVIDENCE_COPY_SECONDS"
        fail 9 "the round of launch $launch_id left no result on $REMOTE_HOST: its runner ended with no status. Its record," \
          "if the runner started, is ~/improve/results/$launch_id/ (output and stderr); its scope is improve-$direction.scope."
        ;;
      "done "*)
        run_status=${state#done }
        break
        ;;
      *)
        fail 3 "the round's state on $REMOTE_HOST names no state for launch $launch_id: '$state'"
        ;;
    esac
  done
  # The stream file holds exactly the bytes this launch passed on, so each call resumes at its size and a call that the
  # bound stops leaves the next call to finish the rest.
  while :; do
    call_status=0
    timeout --kill-after=5 "$REATTACH_CALL_SECONDS" "${SSH[@]}" "$REMOTE_HOST" \
      "bash -s -- --round-output $launch_id $(stat -c %s -- "$round_stream")" <"$SCRIPT_DIR/remote-runner.sh" \
      2>"$failure_dir/reattach.stderr" | tee -a -- "$round_stream" || call_status=$?
    if (( call_status == 0 )); then
      break
    fi
    if host_unanswered "$call_status"; then
      check_reattach_bound "$since"
      sleep "$REATTACH_POLL_SECONDS"
      continue
    fi
    fail 3 "the round's output on $REMOTE_HOST failed for launch $launch_id: " \
      "$(ssh_failure_evidence "$(<"$failure_dir/reattach.stderr")" 'no output')"
  done
}

# Remove the launch's round record on the host once the launch has the round's result. A release that fails leaves the
# record to the host's sweep, and the launch names that instead of failing.
release_host_record() {
  local status=0
  timeout --kill-after=5 "$REATTACH_CALL_SECONDS" "${SSH[@]}" "$REMOTE_HOST" \
    "bash -s -- --round-release $launch_id" <"$SCRIPT_DIR/remote-runner.sh" \
    2>"$failure_dir/release.stderr" || status=$?
  if (( status != 0 )); then
    printf '%s the round record of launch %s stays on %s (status %s); the host sweeps it a day after its status\n' \
      "$STDERR_PREFIX" "$launch_id" "$REMOTE_HOST" "$status" >&2
  fi
}

# The api_key under one top-level section of a credentials file, found by the section's name.
section_api_key() {
  awk -v section="$2" '
    /^[^ ]/ { inside = ($0 == section ":") }
    inside && /^  api_key:/ { value = $0; sub(/^  api_key:[ \t]*/, "", value); print value; exit }
  ' "$1"
}

# The version a member CLI reports for `--version`, from the binary on this host: claude prints `X.Y.Z (Claude Code)`
# and codex prints `codex-cli X.Y.Z`. The binary gets /dev/null as standard input, because the round's prompt is on that
# one. Sets cli_version_result; exits 7 when the binary fails or names no version.
cli_version_of() {
  local kind=$1 binary=$2 output line version_pattern
  case $kind in
    claude) version_pattern='^([0-9]+\.[0-9]+\.[0-9]+) \(Claude Code\)$' ;;
    codex) version_pattern='^codex-cli ([0-9]+\.[0-9]+\.[0-9]+)$' ;;
    *) fail 7 "the member kind '$kind' has no version format in this launcher" ;;
  esac
  output=$("$binary" --version </dev/null) \
    || fail 7 "cannot run $binary --version on this host, so the round's $kind CLI version is unknown"
  line=${output%%$'\n'*}
  [[ $line =~ $version_pattern ]] || fail 7 "$binary --version names no version: '$line'"
  cli_version_result=${BASH_REMATCH[1]}
}

# The Codex login file as a round forwards it: the four fields the CLI reads, with an empty refresh token, so the refresh
# token stays on this host. Sets codex_record to that JSON and codex_seconds_left to the access token's remaining life.
read_codex_login() {
  local login=$1 expiry
  [[ -f $login ]] || fail 7 "the Codex login is missing: $login"
  codex_record=$(jq -ce '
    if ([.auth_mode, .last_refresh, .tokens.access_token, .tokens.id_token, .tokens.account_id]
        | all(type == "string" and length > 0))
    then {auth_mode, last_refresh,
          tokens: {access_token: .tokens.access_token, id_token: .tokens.id_token,
                   account_id: .tokens.account_id, refresh_token: ""}}
    else error("the login lacks a field") end' "$login" 2>/dev/null) \
    || fail 7 "the Codex login is not a complete auth.json: $login"
  expiry=$(jq -r '.tokens.access_token | split(".")[1] | @base64d | fromjson | .exp' "$login" 2>/dev/null) \
    || expiry=
  [[ $expiry =~ ^[0-9]+$ ]] || fail 7 "the Codex access token names no numeric exp: $login"
  codex_seconds_left=$(( expiry - $(date +%s) ))
}

# A subscription login file of a Claude account: its access token and the token's remaining life in seconds. Sets
# subscription_token and subscription_seconds_left; exits 7 when the file names no token or no whole-number expiry.
read_subscription_login() {
  local login=$1 fields expires_ms
  [[ -f $login ]] || fail 7 "the Claude login is missing: $login"
  fields=$(jq -r '
    .claudeAiOauth
    | select((.accessToken | type) == "string" and (.accessToken | length) > 0 and (.expiresAt | type) == "number")
    | [.accessToken, (.expiresAt | tostring)] | @tsv' "$login" 2>/dev/null) \
    || fail 7 "the Claude login is not readable JSON: $login"
  [[ -n $fields ]] || fail 7 "the Claude login names no access token or no numeric expiresAt: $login"
  subscription_token=${fields%%$'\t'*}
  expires_ms=${fields#*$'\t'}
  [[ $expires_ms =~ ^[0-9]+$ ]] || fail 7 "the Claude login names no whole-number expiresAt: $login"
  subscription_seconds_left=$(( expires_ms / 1000 - $(date +%s) ))
}

# Exit with `code` when a login's access token lives fewer seconds than a round needs. The arguments are the seconds left,
# the login file, the exit code and the login's name. The message gives whole minutes and no token.
check_token_life() {
  local seconds_left=$1 login_file=$2 code=$3 name=$4
  if (( seconds_left < TOKEN_MIN_SECONDS )); then
    fail "$code" "token-short: the $name login in $login_file has $(( seconds_left > 0 ? seconds_left / 60 : 0 )) min of access-token" \
      "life left; a round needs $(( TOKEN_MIN_SECONDS / 60 )) min. No round started."
  fi
}

# ---------- 1. The member kind and the review inputs ----------
# Every check in this section runs before the first ssh call, so a missing input sends nothing and starts nothing.
agent_binary=${1-}
kind=${agent_binary##*/}
member_kinds_file="$SCRIPT_DIR/member_kinds.txt"
[[ -r $member_kinds_file ]] || fail 1 "the member kind list is missing or unreadable: $member_kinds_file"
grep -q '[^[:space:]]' "$member_kinds_file" || fail 1 "the member kind list is empty: $member_kinds_file"
grep -Fxq -- "$kind" "$member_kinds_file" || fail 7 "the member kind '$kind' is not supported: a round runs claude or codex"
shift

instructions=
if [[ $kind == claude ]]; then
  expect_value=0
  for argument in "$@"; do
    if (( expect_value )); then
      instructions=$argument
      expect_value=0
      continue
    fi
    if [[ $argument == --append-system-prompt-file ]]; then
      expect_value=1
    fi
  done
  [[ -n $instructions ]] || fail 1 'the command names no --append-system-prompt-file, so the round has no instructions file'
  [[ $instructions == /* ]] || fail 1 'the instructions file path must be absolute'
  [[ -f $instructions ]] || fail 1 "the instructions file is missing: $instructions"
fi

# ---------- 1a. The member CLI version ----------
# The round runs the version of this member CLI that this host runs: the agent binary's own `--version` output. The
# runner installs that version on the improve host and runs no other CLI there.
cli_version_of "$kind" "$agent_binary"
member_version=$cli_version_result

# The member token is the gateway token that the server put in the environment. A claude member whose environment
# names a gateway (ANTHROPIC_BASE_URL) must carry it. A subscription member carries none: section 1b reads its login.
member_token=${ANTHROPIC_AUTH_TOKEN-}
if [[ $kind == claude && -n ${ANTHROPIC_BASE_URL-} && -z $member_token ]]; then
  fail 7 'the round has no member token: the gateway member sent no ANTHROPIC_AUTH_TOKEN'
fi

# ---------- 1b. The member's login ----------
# The member's record goes to the host on the prepare channel. A gateway member sends its token. A subscription member
# sends its access token, which the host's round uses for its own life only: this section reads it once here, and checks
# its life before the first ssh call. A login with less life than IMPROVE_TOKEN_MIN_SECONDS exits 8 and sends nothing.
if [[ $kind == codex ]]; then
  read_codex_login "$HOME_DIR/.codex/auth.json"
  check_token_life "$codex_seconds_left" "$HOME_DIR/.codex/auth.json" 8 codex
  member_record="IMPROVE_CODEX_AUTH=$codex_record"
elif [[ -n $member_token ]]; then
  member_record="ANTHROPIC_AUTH_TOKEN=$member_token"
else
  [[ -n ${CLAUDE_CONFIG_DIR-} ]] \
    || fail 7 'the round has no Claude login: the subscription member names no CLAUDE_CONFIG_DIR'
  read_subscription_login "$CLAUDE_CONFIG_DIR/.credentials.json"
  check_token_life "$subscription_seconds_left" "$CLAUDE_CONFIG_DIR/.credentials.json" 8 claude
  member_record="CLAUDE_CODE_OAUTH_TOKEN=$subscription_token"
fi

# ---------- 1c. The review backend and its login ----------
# `backends.calls.improve_review` names the review backend. The server resolves it here, before the first ssh call, and
# the answer goes to the host as the launch's review/option.json. An unset call class exits 7 and the message names it.
if ! review_answer=$(charliebot backend resolve --call improve_review 2>&1); then
  fail 7 "cannot resolve the review backend of backends.calls.improve_review: $review_answer"
fi
review_id=$(jq -er '.option.id' <<<"$review_answer") \
  || fail 7 "the resolve answer of backends.calls.improve_review names no option id: $review_answer"
review_type=$(jq -er '.option.type' <<<"$review_answer") \
  || fail 7 "the resolve answer of backends.calls.improve_review names no option type: $review_answer"
review_account_dir=$(jq -r '.claude_config_dir // empty' <<<"$review_answer")
case $review_type in
  cc-anthropic-compatible)
    # The gateway key is the review's login. The host's round reads it from its credentials file, which the runner
    # writes from this record.
    review_kind=claude
    [[ -r $PROFILE_HOME/credentials.yaml ]] \
      || fail 7 "the review credential's file is missing: $PROFILE_HOME/credentials.yaml"
    review_credential=$(jq -er '.option.credential' <<<"$review_answer") \
      || fail 7 "the review backend $review_id names no credential"
    review_key=$(section_api_key "$PROFILE_HOME/credentials.yaml" "$review_credential")
    [[ -n $review_key ]] || fail 7 "credentials.yaml holds no api_key for the review credential $review_credential"
    review_record="REVIEW_ANTHROPIC_AUTH_TOKEN=$review_key"
    ;;
  cc-claude)
    review_kind=claude
    [[ -n $review_account_dir ]] || fail 7 "the review backend $review_id names no account directory"
    read_subscription_login "$review_account_dir/.credentials.json"
    check_token_life "$subscription_seconds_left" "$review_account_dir/.credentials.json" 7 'review claude'
    review_record="REVIEW_CLAUDE_CODE_OAUTH_TOKEN=$subscription_token"
    ;;
  codex)
    review_kind=codex
    read_codex_login "$HOME_DIR/.codex/auth.json"
    check_token_life "$codex_seconds_left" "$HOME_DIR/.codex/auth.json" 7 'review codex'
    review_record="REVIEW_CODEX_AUTH=$codex_record"
    ;;
  *)
    fail 7 "the review backend $review_id has type $review_type, which the improve host has no login form for"
    ;;
esac
review_binary=$(command -v "$review_kind") \
  || fail 7 "the review's $review_kind CLI is not on this host's PATH, so its version is unknown"
cli_version_of "$review_kind" "$review_binary"
review_version=$cli_version_result
printf '%s review backend %s (%s)\n' "$STDERR_PREFIX" "$review_id" "$review_type" >&2

# ---------- 1d. The probe answer ----------
# `backends.calls.probe` names the target a serve trial takes its model endpoint from, and the trial needs a member
# with an api_base. The helper walks the target's members in config order and answers the first one that names an
# api_base; a target with no such member fails here, by name. The improve host runs no server, so the round's trial
# reads this answer from the launch: CHARLIEBOT_TRIAL_PROBE_ANSWER names its file.
if ! probe_answer=$(CHARLIEBOT_HOME="$PROFILE_HOME" charliebot release-helper endpoint-option "$PROFILE_HOME" probe 2>&1); then
  fail 7 "cannot resolve an endpoint member of backends.calls.probe: $probe_answer"
fi
jq -e '.option.id' <<<"$probe_answer" >/dev/null \
  || fail 7 "the probe answer of backends.calls.probe names no option id: $probe_answer"

# ---------- 1e. The kit revision ----------
# The kit files that this launch sends are copies of this checkout, and the launch names HEAD as their revision. A
# checkout whose dev/improve differs from HEAD would send files of another revision, so the launch exits 7 here.
cli_commit=$(git -C "$SCRIPT_DIR/../.." rev-parse HEAD 2>/dev/null) || cli_commit=
[[ -n $cli_commit ]] || fail 1 'cannot read the commit of this launcher checkout'
[[ -z $(git -C "$SCRIPT_DIR/../.." status --porcelain -- dev/improve) ]] \
  || fail 7 "dev/improve differs from HEAD $cli_commit in this checkout, so the round's kit is not at that revision"

# ---------- 2. Round preflight ----------
set +e
preflight_output=$(timeout "$PREFLIGHT_SECONDS" "${SSH[@]}" -n "$REMOTE_HOST" true 2>&1)
preflight_status=$?
set -e
if (( preflight_status != 0 )); then
  fail_ssh_call 'preflight' "$preflight_output" "no answer within ${PREFLIGHT_SECONDS} s"
fi

# ---------- 3. Direction ----------
if ! direction=$(resolve_direction); then
  exit 6
fi

# The skill roots that a round's instructions file and prompt name map to $IMPROVE_SKILLS_DIR, the skills of the round's
# kit, which the runner exports. These are a host's `~/.claude/skills` and `~/.agents/skills`, its
# `$CHARLIEBOT_HOME/skills`, and a relative `skills/` at the start of a word. Git's `<ref>:skills/...` names a commit's
# tree, so its text stays. This maps text only: a codex round's discovery of skills, which codex does on its own from
# the host's roots, is confined by the runner's codex-skill-roots.sh. It reads standard input and writes standard
# output, so a file and a stream both pass through.
map_skill_roots() {
  sed -E \
    -e 's#~/\.(claude|agents)/skills#$IMPROVE_SKILLS_DIR#g' \
    -e 's#\$CHARLIEBOT_HOME/skills#$IMPROVE_SKILLS_DIR#g' \
    -e 's#(^|[[:space:]`"(])skills/#\1$IMPROVE_SKILLS_DIR/#g'
}

# ---------- 4. Send ----------
launch_id=$(date -u +%Y%m%dT%H%M%SZ)-$$-$RANDOM
payload_parent=$TEMP_ROOT
payload=$(mktemp -d "$payload_parent/improve-launch.XXXXXXXX") || fail 1 'cannot make the launch directory'
# The ssh calls that send the launch write their stderr here, outside the payload: a log inside the payload would be
# tarred with it.
failure_dir=$(mktemp -d "$payload_parent/improve-failure.XXXXXXXX") \
  || { rm -rf -- "$payload"; fail 1 'cannot make the failure log directory'; }
# The round's standard output as this launch passed it on. A reattach resumes from its size.
round_stream=$failure_dir/round.stdout
cleanup_payload() {
  [[ $payload == "$payload_parent"/improve-launch.* ]] && rm -rf -- "$payload"
  [[ $failure_dir == "$payload_parent"/improve-failure.* ]] && rm -rf -- "$failure_dir"
  return 0
}
trap cleanup_payload EXIT

# Each host path maps to the name of its copy in the launch directory; the runner prefixes the
# launch directory, so this host never needs the remote home path.
: >"$payload/paths"
if [[ $kind == claude ]]; then
  map_skill_roots <"$instructions" >"$payload/instructions.md"
  printf '%s\t%s\n' "$instructions" instructions.md >"$payload/paths"
fi
printf '%s\0' "$@" >"$payload/args"

pattern_file="$PROFILE_HOME/skills_leak_patterns.local.txt"
[[ -r $pattern_file ]] || fail 1 "the leak pattern file is missing: $pattern_file"
cp -- "$pattern_file" "$payload/skills_leak_patterns.local.txt"
cp -- "$SCRIPT_DIR/remote-runner.sh" "$payload/remote-runner.sh"
# The revision of the kit the runner fetches from this launcher's origin; the runner refuses the launch unless its
# charliebot is built from this commit.
printf '%s\n' "$cli_commit" >"$payload/kit-revision"
# The revision of the skills the runner archives from that commit. It is this checkout's HEAD as well. The runner
# refuses the launch unless the kit revision, this one and its charliebot name the same commit.
printf '%s\n' "$cli_commit" >"$payload/skills-revision"
# The review backend's answer: the runner reads the review's backend record from it, and the answer holds no key.
mkdir -- "$payload/review"
printf '%s\n' "$review_answer" >"$payload/review/option.json"
# The probe answer, in the same form: the round's serve trial reads it from probe/option.json.
mkdir -- "$payload/probe"
printf '%s\n' "$probe_answer" >"$payload/probe/option.json"
cp -- "$member_kinds_file" "$payload/member_kinds.txt"
# The runner sources the in-flight rule from this copy, so its clone scan uses
# the same claimant and path functions as the list above.
cp -- "$SCRIPT_DIR/inflight-files.sh" "$payload/inflight-files.sh"
# The runner sources the clone's sync and check from this copy.
cp -- "$SCRIPT_DIR/clone-check.sh" "$payload/clone-check.sh"
# The runner sources a codex round's skill roots from this copy: its codex home lists only the round's skills.
cp -- "$SCRIPT_DIR/codex-skill-roots.sh" "$payload/codex-skill-roots.sh"
# The runner sources the round's target cache trim from this copy, in the round's scope before its agent starts.
cp -- "$SCRIPT_DIR/target-cache.sh" "$payload/target-cache.sh"
# The runner clones its own checkout from this origin, so the round tools are built from the
# runtime clone's repository and not from the direction clone's configuration.
runtime_origin=$(git -C "$SCRIPT_DIR/../.." remote get-url origin 2>/dev/null) || runtime_origin=
[[ -n $runtime_origin ]] || fail 1 'cannot read the origin of this launcher checkout'
printf '%s\n' "$runtime_origin" >"$payload/origin"

# The in-flight list comes from this host: the worktrees of other lines exist only here.
inflight_stderr="$failure_dir/inflight-files.stderr"
if ! "$SCRIPT_DIR/inflight-files.sh" "improve/$direction/" >"$payload/inflight-files.txt" 2>"$inflight_stderr"; then
  cat -- "$inflight_stderr" >&2
  fail 1 'cannot list the in-flight files on this host'
fi
if ! sed -n '/^inflight-files: dropped worktree /p' "$inflight_stderr" >"$payload/inflight-files-notices.txt"; then
  cat -- "$inflight_stderr" >&2
  fail 1 'cannot collect dropped worktree notices'
fi
cat -- "$inflight_stderr" >&2

# The live sessions record, which only this host can read: the live server is reachable from here alone. A failed read
# sends no record and names why. The round still starts, and a web page PR of the round that names a session stops at
# pr-create for the missing record.
if ! live_sessions_record "$payload/live-sessions.tsv" 2>"$failure_dir/live-sessions.stderr"; then
  cat -- "$failure_dir/live-sessions.stderr" >&2
  printf '%s no live sessions record was sent for this round\n' "$STDERR_PREFIX" >&2
fi

# The forwarded variables: the gateway and model settings of this backend, which start with
# ANTHROPIC_ or CLAUDE_, and the two Bash timeouts the round needs. CLAUDE_CONFIG_DIR is the
# worker's own config directory here, CLAUDE_CODE_OAUTH_TOKEN is the member's login, which section 1b
# reads, and CHARLIEBOT_RUN_TOKEN authenticates to a server that the remote host does not have. The
# tar payload holds no token: the member's login travels on the later `--prepare` channel and is stored
# in the host's per-launch member-credential file. Values go into a file, never a command line.
forwarded_name() {
  case $1 in
    ANTHROPIC_AUTH_TOKEN | ANTHROPIC_API_KEY) return 1 ;;
    CLAUDE_CONFIG_DIR | CLAUDE_CODE_OAUTH_TOKEN | CHARLIEBOT_RUN_TOKEN) return 1 ;;
    ANTHROPIC_* | CLAUDE_* | BASH_DEFAULT_TIMEOUT_MS | BASH_MAX_TIMEOUT_MS | CHARLIEBOT_SESSION_ID) return 0 ;;
    *) return 1 ;;
  esac
}
: >"$payload/env"
while IFS= read -r name; do
  forwarded_name "$name" || continue
  printf '%s\0' "$name=${!name}" >>"$payload/env"
done < <(compgen -e | LC_ALL=C sort)

copy_evidence "$EVIDENCE_COPY_SECONDS"

# One ssh carries every input as a tar stream on its standard input, so no input reaches an
# argument. The tar holds no token: the next call sends those.
set +e
tar -C "$payload" -cf - . |
  "${SSH[@]}" "$REMOTE_HOST" "umask 077; mkdir -p \"\$HOME/improve/launch/$launch_id\" && tar -C \"\$HOME/improve/launch/$launch_id\" -xf -" \
    3>&- 2>"$failure_dir/send.err"
send_status=$?
set -e
if (( send_status != 0 )); then
  send_output=
  [[ -s $failure_dir/send.err ]] && send_output=$(<"$failure_dir/send.err")
  if (( send_status == 255 )); then
    fail_ssh_call 'input send' "$send_output" "ssh exited $send_status"
  fi
  # The host answered and the send itself failed: quote its error and name no approval page.
  fail 3 "cannot send the round's inputs to $REMOTE_HOST (exit $send_status):" \
    "$(ssh_failure_evidence "$send_output" 'no output')"
fi

# The heartbeat carries this launch id as its own argument. The host's next-start sweep uses that live process to keep
# this launch's member-credential file between `--prepare` and the wrapper's start. Start it before preparing that
# file, so the sweep sees an owner before the credential exists. The heartbeat stops no scope: when its connection ends,
# the host sees the same end as a dropped connection, and a dropped connection never stops a round.
heartbeat_command="bash -c 'cat >/dev/null' improve-heartbeat $launch_id $direction"
mkfifo "$payload/heartbeat"
"${SSH[@]}" "$REMOTE_HOST" "$heartbeat_command" <"$payload/heartbeat" &
heartbeat_pid=$!
exec 3>"$payload/heartbeat"

# The member's record and the review's record from sections 1b and 1c go to the host on this channel's standard input
# alone. The runner selects each record by its name and writes both into the launch's member-credential file. The
# launching shell holds them in memory until this call sends them.
records=(
  "$member_record"
  "$review_record"
)
set +e
printf '%s\0' "${records[@]}" |
  "${SSH[@]}" "$REMOTE_HOST" "bash \$HOME/improve/launch/$launch_id/remote-runner.sh --prepare $launch_id" 3>&- \
    2>"$failure_dir/prepare.err"
prepare_status=$?
set -e
if (( prepare_status == 255 )); then
  prepare_output=
  [[ -s $failure_dir/prepare.err ]] && prepare_output=$(<"$failure_dir/prepare.err")
  fail_ssh_call 'credential write' "$prepare_output" "ssh exited $prepare_status"
fi
if (( prepare_status != 0 )); then
  [[ -s $failure_dir/prepare.err ]] && cat -- "$failure_dir/prepare.err" >&2
  fail 1 "the runner could not write the round's credentials on $REMOTE_HOST"
fi

# ---------- 5. Start ----------
# The round's prompt is on this launcher's standard input, and the main run sends it to the agent with the skill roots
# mapped. The start's output passes through the stream file, so the launch knows the bytes it has passed on when a
# connection drops.
: >"$round_stream"
set +e
map_skill_roots | "${SSH[@]}" "$REMOTE_HOST" \
  "bash \$HOME/improve/launch/$launch_id/remote-runner.sh $launch_id $direction $cli_commit $kind $member_version $review_kind $review_version" \
  3>&- | tee -a -- "$round_stream"
run_status=${PIPESTATUS[1]}
set -e
# A dropped connection leaves the round running on the host, so the launch reattaches to the round's record. A start that
# returned its status leaves nothing to reattach to.
if (( run_status == 255 )); then
  reattach_round
fi
release_host_record

# ---------- 6. End ----------
copy_evidence "$EVIDENCE_COPY_SECONDS"

exec 3>&-
# The heartbeat stops nothing on the host, so this bound only keeps a dead connection from holding the
# launcher open.
for (( attempt = 0; attempt < 50; attempt++ )); do
  kill -0 "$heartbeat_pid" 2>/dev/null || break
  sleep 0.1
done
kill "$heartbeat_pid" 2>/dev/null || true
wait "$heartbeat_pid" 2>/dev/null || true
exit "$run_status"
