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
# script connects there, sends the round's inputs, runs dev/improve/remote-runner.sh and passes the agent's stdin and
# stdout through unchanged. No command of the round runs on this host, so a round that forgets a variable, sets a wrong
# path or runs rm cannot reach this host's profile home or runtime clone: on 2026-10-04 a round's own probe deleted the
# whole profile home.
#
# The round path reads these inputs on this host and writes one directory:
#   - for claude, the instructions file that --append-system-prompt-file names (codex carries its instructions in its
#     arguments);
#   - the review option's credential name, model and API base in $CHARLIEBOT_HOME/config.yaml, and that
#     credential's api_key in $CHARLIEBOT_HOME/credentials.yaml;
#   - the leak pattern file $CHARLIEBOT_HOME/skills_leak_patterns.local.txt;
#   - the in-flight files list, which dev/improve/inflight-files.sh prints here;
#   - the worker session's metadata.json, for task_parent_id;
#   - the cron task file of that parent session, for prompt_file;
#   - the origin URL of this launcher's checkout, which the runner clones from.
# It writes only ~/improve/<direction>/evidence/.
#
# Exit codes: the agent's exit code; 1 a local failure, such as a missing input file; 3 the host asks for a Tailscale
# approval or cannot be reached; 4 the direction's clone is missing or is not this protocol's clone; 5 the previous round
# of this direction still runs; 6 the round's direction cannot be read; 7 a round dependency is missing, and stderr names
# it. Codes 3 to 7 start no agent and no scope.
#
# The token values reach the host on ssh's standard input only, so no token value appears in any process command line or
# in an input file: the runner writes them into ~/improve/home/credentials.yaml, the one file that holds them.
# CHARLIEBOT_RUN_TOKEN stays here: the remote host has no server that it could call.
set -euo pipefail

readonly STDERR_PREFIX='improve launch:'
# The connection gets 4 seconds, so a round that needs an approval fails with exit 3 inside 5.
readonly PREFLIGHT_SECONDS=4
# The copy-back limit. The server stops following 90 seconds after output goes silent, so a slow
# copy stops here and the next launch of this direction copies the rest.
readonly EVIDENCE_COPY_SECONDS=${IMPROVE_EVIDENCE_COPY_SECONDS:-60}
# A call that sends no input of its own adds -n. Without -n, ssh reads this launcher's standard input, which
# carries the round's prompt, and the remote command drops what it does not read. Only the main run passes that
# standard input on to the remote agent.
readonly SSH=(ssh -T -o BatchMode=yes -o ConnectTimeout=10)
# The review option: the backend option whose model, API base and credential the round's review uses.
# This script is the only place that names it.
readonly REVIEW_OPTION='claude-code-kimi-k3.1-pro'

fail() {
  local code=$1
  shift
  printf '%s %s\n' "$STDERR_PREFIX" "$*" >&2
  exit "$code"
}

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly SCRIPT_DIR=$script_dir
readonly HOME_DIR=${HOME:-}
[[ $HOME_DIR == /* ]] || fail 1 'HOME must be an absolute path'
readonly PROFILE_HOME=${CHARLIEBOT_HOME:-$HOME_DIR/.charliebot}
[[ $PROFILE_HOME == /* ]] || fail 1 'CHARLIEBOT_HOME must be an absolute path'
readonly CRON_DIR="$PROFILE_HOME/config.d/cron.d"
readonly REMOTE_HOST=${IMPROVE_REMOTE_HOST:-}
readonly SESSION_ID=${CHARLIEBOT_SESSION_ID:-}
readonly TEMP_ROOT=${TMPDIR:-/tmp}
[[ $TEMP_ROOT == /* ]] || fail 1 'TMPDIR must be an absolute path'
[[ -n $REMOTE_HOST ]] || fail 1 'IMPROVE_REMOTE_HOST is not set; it names the improve host in host config'
[[ -n $SESSION_ID ]] || fail 6 'CHARLIEBOT_SESSION_ID is not set, so the round has no direction'

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

# The review option's fields from config.yaml, one `name<TAB>value` line each for the names in `names`. The options
# are list items: an item starts at a `  - ` line and its fields sit four spaces in. A top-level key or a two-space key
# ends the item before it.
review_option_fields() {
  awk -v id="$REVIEW_OPTION" -v names='model api_base credential' '
    function take(text,   key, value) {
      if (current == 0 || !match(text, /^[A-Za-z_]+:/)) return
      key = substr(text, 1, RLENGTH - 1)
      value = substr(text, RLENGTH + 1)
      sub(/^[ \t]+/, "", value)
      sub(/[ \t]+#.*$/, "", value)
      field[current, key] = value
    }
    BEGIN { wanted_count = split(names, wanted, " ") }
    /^  - / { items++; current = items; take(substr($0, 5)); next }
    /^    / { take(substr($0, 5)); next }
    /^[^ ]/ || /^  [^ ]/ { current = 0 }
    END {
      for (item = 1; item <= items; item++) {
        if (field[item, "id"] != id) continue
        for (i = 1; i <= wanted_count; i++)
          if ((item, wanted[i]) in field) print wanted[i] "\t" field[item, wanted[i]]
      }
    }
  ' "$PROFILE_HOME/config.yaml"
}

# The api_key under one top-level section of a credentials file, found by the section's name.
section_api_key() {
  awk -v section="$2" '
    /^[^ ]/ { inside = ($0 == section ":") }
    inside && /^  api_key:/ { value = $0; sub(/^  api_key:[ \t]*/, "", value); print value; exit }
  ' "$1"
}

# ---------- 1. The member kind and the review inputs ----------
# Every check in this section runs before the first ssh call, so a missing input sends nothing and starts nothing.
agent_binary=${1-}
kind=${agent_binary##*/}
case $kind in
  claude | codex) ;;
  *) fail 7 "the member kind '$kind' is not supported: a round runs claude or codex" ;;
esac
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

# The member token is the gateway token that the server put in the environment. A claude member whose environment
# names a gateway (ANTHROPIC_BASE_URL) must carry it. A subscription member carries none: its login stays on the host.
member_token=${ANTHROPIC_AUTH_TOKEN-}
if [[ $kind == claude && -n ${ANTHROPIC_BASE_URL-} && -z $member_token ]]; then
  fail 7 'the round has no member token: the gateway member sent no ANTHROPIC_AUTH_TOKEN'
fi

declare -A review=()
[[ -r $PROFILE_HOME/config.yaml ]] || fail 7 "the review option's config.yaml is missing: $PROFILE_HOME/config.yaml"
while IFS=$'\t' read -r field value; do
  review[$field]=$value
done < <(review_option_fields)
for field in model api_base credential; do
  [[ -n ${review[$field]-} ]] || fail 7 "the review option $REVIEW_OPTION names no $field in $PROFILE_HOME/config.yaml"
done
[[ -r $PROFILE_HOME/credentials.yaml ]] || fail 7 "the review credential's file is missing: $PROFILE_HOME/credentials.yaml"
review_key=$(section_api_key "$PROFILE_HOME/credentials.yaml" "${review[credential]}")
[[ -n $review_key ]] || fail 7 "credentials.yaml holds no api_key for the review credential ${review[credential]}"

# ---------- 2. Round preflight ----------
set +e
preflight_output=$(timeout "$PREFLIGHT_SECONDS" "${SSH[@]}" -n "$REMOTE_HOST" true 2>&1)
preflight_status=$?
set -e
if (( preflight_status != 0 )); then
  if [[ -n $preflight_output ]]; then
    printf '%s\n' "$preflight_output" >&2
  fi
  fail 3 "cannot reach the improve host $REMOTE_HOST; approve the connection on the /host-auth page"
fi

# ---------- 3. Direction ----------
if ! direction=$(resolve_direction); then
  exit 6
fi

# ---------- 4. Send ----------
launch_id=$(date -u +%Y%m%dT%H%M%SZ)-$$-$RANDOM
payload_parent=$TEMP_ROOT
payload=$(mktemp -d "$payload_parent/improve-launch.XXXXXXXX") || fail 1 'cannot make the launch directory'
cleanup_payload() {
  [[ $payload == "$payload_parent"/improve-launch.* ]] && rm -rf -- "$payload"
  return 0
}
trap cleanup_payload EXIT

# Each host path maps to the name of its copy in the launch directory; the runner prefixes the
# launch directory, so this host never needs the remote home path.
: >"$payload/paths"
if [[ $kind == claude ]]; then
  cp -- "$instructions" "$payload/instructions.md"
  printf '%s\t%s\n' "$instructions" instructions.md >"$payload/paths"
fi
printf '%s\0' "$@" >"$payload/args"

pattern_file="$PROFILE_HOME/skills_leak_patterns.local.txt"
[[ -r $pattern_file ]] || fail 1 "the leak pattern file is missing: $pattern_file"
cp -- "$pattern_file" "$payload/skills_leak_patterns.local.txt"
cp -- "$SCRIPT_DIR/remote-runner.sh" "$payload/remote-runner.sh"
# The runner sources the in-flight rule from this copy, so its clone scan uses
# the same claimant and path functions as the list above.
cp -- "$SCRIPT_DIR/inflight-files.sh" "$payload/inflight-files.sh"
# The runner clones its own checkout from this origin, so the round tools are built from the
# runtime clone's repository and not from the direction clone's configuration.
runtime_origin=$(git -C "$SCRIPT_DIR/../.." remote get-url origin 2>/dev/null) || runtime_origin=
[[ -n $runtime_origin ]] || fail 1 'cannot read the origin of this launcher checkout'
printf '%s\n' "$runtime_origin" >"$payload/origin"

# The in-flight list comes from this host: the worktrees of other lines exist only here.
if ! "$SCRIPT_DIR/inflight-files.sh" "improve/$direction/" >"$payload/inflight-files.txt"; then
  fail 1 'cannot list the in-flight files on this host'
fi

# The forwarded variables: the gateway and model settings of this backend, which start with
# ANTHROPIC_ or CLAUDE_, and the two Bash timeouts the round needs. CLAUDE_CONFIG_DIR is the
# worker's own config directory here, CLAUDE_CODE_OAUTH_TOKEN is the host's own login, and
# CHARLIEBOT_RUN_TOKEN authenticates to a server that the remote host does not have. The tokens
# travel on the `--prepare` channel instead of this file, so the launch directory holds no token.
# Values go into a file, never a command line.
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

cli_commit=$(git -C "$SCRIPT_DIR/../.." rev-parse HEAD 2>/dev/null) || cli_commit=
[[ -n $cli_commit ]] || fail 1 'cannot read the commit of this launcher checkout'

copy_evidence "$EVIDENCE_COPY_SECONDS"

# One ssh carries every input as a tar stream on its standard input, so no input reaches an
# argument. The tar holds no token: the next call sends those.
set +e
tar -C "$payload" -cf - . |
  "${SSH[@]}" "$REMOTE_HOST" "umask 077; mkdir -p \"\$HOME/improve/launch/$launch_id\" && tar -C \"\$HOME/improve/launch/$launch_id\" -xf -" \
    3>&-
send_status=$?
set -e
(( send_status == 0 )) || fail 3 "cannot send the round's inputs to $REMOTE_HOST; approve the connection on the /host-auth page"

# The review key, the review option's settings and the member token, when the server sent one, go to the host on this
# channel's standard input alone. The runner selects each value by its name and writes the keys into the round's
# credentials file. The launching shell holds them in memory until this call sends them.
records=(
  "CHARLIE_CODE_API_KEY=$review_key"
  "IMPROVE_REVIEW_CREDENTIAL=${review[credential]}"
  "IMPROVE_REVIEW_MODEL=${review[model]}"
  "IMPROVE_REVIEW_API_BASE=${review[api_base]}"
)
if [[ -n $member_token ]]; then
  records+=("ANTHROPIC_AUTH_TOKEN=$member_token")
fi
set +e
printf '%s\0' "${records[@]}" |
  "${SSH[@]}" "$REMOTE_HOST" "bash \$HOME/improve/launch/$launch_id/remote-runner.sh --prepare $launch_id" 3>&-
prepare_status=$?
set -e
if (( prepare_status == 255 )); then
  fail 3 "cannot reach the improve host $REMOTE_HOST; approve the connection on the /host-auth page"
fi
(( prepare_status == 0 )) || fail 1 "the runner could not write the round's credentials.yaml on $REMOTE_HOST"

# ---------- 5. Start ----------
# The heartbeat reads its standard input on the remote side. EOF tells it that the launcher is
# gone, and only then does it stop the scope: a Tailscale connection that is killed leaves remote
# processes alive, so the launcher's death cannot be detected on the host any other way.
# The FIFO's write end lives in this process alone, so any exit, SIGKILL included, closes it.
heartbeat_command="cat >/dev/null; if [ -e \"\$HOME/improve/launch/$launch_id/started\" ]; then systemctl --user stop improve-$direction.scope; fi"
mkfifo "$payload/heartbeat"
"${SSH[@]}" "$REMOTE_HOST" "$heartbeat_command" <"$payload/heartbeat" &
heartbeat_pid=$!
exec 3>"$payload/heartbeat"

set +e
"${SSH[@]}" "$REMOTE_HOST" \
  "bash \$HOME/improve/launch/$launch_id/remote-runner.sh $launch_id $direction $cli_commit $kind" 3>&-
run_status=$?
set -e

# ---------- 6. End ----------
copy_evidence "$EVIDENCE_COPY_SECONDS"

exec 3>&-
# The heartbeat's stop runs on the host; this bound only keeps a dead connection from holding the
# launcher open.
for (( attempt = 0; attempt < 50; attempt++ )); do
  kill -0 "$heartbeat_pid" 2>/dev/null || break
  sleep 0.1
done
kill "$heartbeat_pid" 2>/dev/null || true
wait "$heartbeat_pid" 2>/dev/null || true
exit "$run_status"
