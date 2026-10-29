#!/usr/bin/env bash
# Launch one improve round on the remote host.
#
#   tools/improve/remote-launch.sh <claude arguments...>
#
# The backend starts this script where it would start `claude`, with claude's arguments,
# environment, stdin and stdout. The script connects to the host named by IMPROVE_REMOTE_HOST
# (host config only, because the repository is public), sends the round's inputs, runs
# tools/improve/remote-runner.sh there and passes claude's stdin and stdout through unchanged.
# No command of the round runs on this host, so a round that forgets a variable, sets a wrong
# path or runs rm cannot reach this host's profile home or runtime clone: on 2026-10-04 a round's
# own probe deleted the whole profile home.
#
# This host reads five inputs and writes one directory:
#   - the instructions file that --append-system-prompt-file names;
#   - the leak pattern file $CHARLIEBOT_HOME/skills_leak_patterns.local.txt;
#   - the in-flight files list, which tools/improve/inflight-files.sh prints here;
#   - the worker session's metadata.json, for task_parent_id;
#   - the cron task file of that parent session, for prompt_file.
# It writes only ~/improve/<direction>/evidence/.
#
# Exit codes: claude's exit code; 1 a local failure, such as a missing input file; 3 the host asks
# for a Tailscale approval or cannot be reached; 4 the direction's clone is missing or is not this
# protocol's clone; 5 the previous round of this direction still runs; 6 the round's direction
# cannot be read. Codes 3 to 6 start no claude and no scope.
#
# The token values reach the host on ssh's standard input only, so no token value appears in any
# process command line or in an input file: the runner writes them into
# ~/improve/home/credentials.yaml, the one file that holds them. CHARLIEBOT_RUN_TOKEN stays here:
# the remote host has no server that it could call.
set -euo pipefail

readonly STDERR_PREFIX='improve launch:'
# The connection gets 4 seconds, so a round that needs an approval fails with exit 3 inside 5.
readonly PREFLIGHT_SECONDS=4
# The copy-back limit. The server stops following 90 seconds after output goes silent, so a slow
# copy stops here and the next launch of this direction copies the rest.
readonly EVIDENCE_COPY_SECONDS=${IMPROVE_EVIDENCE_COPY_SECONDS:-60}
# A call that sends no input of its own adds -n. Without -n, ssh reads this launcher's standard input, which
# carries the round's prompt, and the remote command drops what it does not read. Only the main run passes that
# standard input on to the remote claude.
readonly SSH=(ssh -T -o BatchMode=yes -o ConnectTimeout=10)

fail() {
  local code=$1
  shift
  printf '%s %s\n' "$STDERR_PREFIX" "$*" >&2
  exit "$code"
}

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly SCRIPT_DIR=$script_dir
readonly PROFILE_HOME=${CHARLIEBOT_HOME:-$HOME/.charliebot}
readonly REMOTE_HOST=${IMPROVE_REMOTE_HOST:-}
readonly SESSION_ID=${CHARLIEBOT_SESSION_ID:-}

[[ -n ${HOME:-} ]] || fail 1 'HOME is not set, so the evidence copy has no target'
[[ -n $REMOTE_HOST ]] || fail 1 'IMPROVE_REMOTE_HOST is not set; it names the improve host in host config'
[[ -n $SESSION_ID ]] || fail 6 'CHARLIEBOT_SESSION_ID is not set, so the round has no direction'

# The value of one top-level YAML key in a file, unquoted, without a trailing comment.
yaml_value() {
  local file=$1 key=$2
  sed -n "s/^$key:[[:space:]]*//p" "$file" | head -n 1 | sed 's/[[:space:]]*#.*//; s/^["'\'']//; s/["'\'']$//'
}

# The direction is the name of the task session's prompt file: `tools/improve/perf.md` gives
# `perf`. Every link is checked here because the direction names the scope, the boundary and
# TMPDIR: a guessed direction would write another direction's directories.
resolve_direction() {
  local metadata="$PROFILE_HOME/sessions/$SESSION_ID/metadata.json"
  [[ -f $metadata ]] || fail 6 "the worker session's metadata.json is missing: $metadata"
  local parent
  parent=$(sed -n 's/^[[:space:]]*"task_parent_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$metadata" | head -n 1)
  [[ -n $parent ]] || fail 6 "the worker session's metadata.json names no task_parent_id: $metadata"
  local candidate session prompt
  for candidate in "$PROFILE_HOME"/config.d/cron.d/*.yaml; do
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
  fail 6 "no cron task file under $PROFILE_HOME/config.d/cron.d names session_id $parent"
}

# Copy the evidence that the previous launch of this direction did not finish copying. A killed
# launch leaves evidence on the host, and the next launch brings it back before the round starts.
copy_evidence() {
  local limit=$1
  local local_evidence="$HOME/improve/$direction/evidence"
  mkdir -p -- "$local_evidence"
  local remote_evidence='$HOME/improve/'"$direction"'/evidence'
  set +e
  timeout --kill-after=5 "$limit" bash -c '
    set -o pipefail
    ssh -n -T -o BatchMode=yes -o ConnectTimeout=10 "$1" \
      "if [ -d \"$2\" ]; then tar -C \"$2\" -cf - .; else tar -cf - --files-from=/dev/null; fi" \
      | tar -C "$3" -xf -
  ' copy "$REMOTE_HOST" "$remote_evidence" "$local_evidence" 3>&-
  local status
  status=$?
  set -e
  if (( status != 0 )); then
    printf '%s the evidence copy stopped (status %s); the next launch copies the rest\n' "$STDERR_PREFIX" "$status" >&2
  fi
}

# ---------- 1. Preflight ----------
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

# ---------- 2. Direction ----------
if ! direction=$(resolve_direction); then
  exit 6
fi

# ---------- 3. Send ----------
launch_id=$(date -u +%Y%m%dT%H%M%SZ)-$$-$RANDOM
payload_parent=${TMPDIR:-/tmp}
payload=$(mktemp -d "$payload_parent/improve-launch.XXXXXXXX") || fail 1 'cannot make the launch directory'
cleanup_payload() {
  [[ $payload == "$payload_parent"/improve-launch.* ]] && rm -rf -- "$payload"
  return 0
}
trap cleanup_payload EXIT

instructions=
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
[[ -f $instructions ]] || fail 1 "the instructions file is missing: $instructions"
cp -- "$instructions" "$payload/instructions.md"
# Each host path maps to the name of its copy in the launch directory; the runner prefixes the
# launch directory, so this host never needs the remote home path.
printf '%s\t%s\n' "$instructions" instructions.md >"$payload/paths"
printf '%s\0' "$@" >"$payload/args"

pattern_file="$PROFILE_HOME/skills_leak_patterns.local.txt"
[[ -r $pattern_file ]] || fail 1 "the leak pattern file is missing: $pattern_file"
cp -- "$pattern_file" "$payload/skills_leak_patterns.local.txt"
cp -- "$SCRIPT_DIR/remote-runner.sh" "$payload/remote-runner.sh"
cp -- "$SCRIPT_DIR/landlock_exec.py" "$payload/landlock_exec.py"

# The in-flight list comes from this host: the worktrees of other lines exist only here.
if ! "$SCRIPT_DIR/inflight-files.sh" "improve/$direction/" >"$payload/inflight-files.txt"; then
  fail 1 'cannot list the in-flight files on this host'
fi

# The forwarded variables: the gateway and model settings of this backend, which start with
# ANTHROPIC_ or CLAUDE_, and the two Bash timeouts the round needs. CLAUDE_CONFIG_DIR is the
# worker's own config directory here, and CHARLIEBOT_RUN_TOKEN authenticates to a server that the
# remote host does not have. The two tokens travel on the `--prepare` channel instead of this
# file, so the launch directory holds no token. Values go into a file, never a command line.
forwarded_name() {
  case $1 in
    ANTHROPIC_AUTH_TOKEN | ANTHROPIC_API_KEY) return 1 ;;
    CLAUDE_CONFIG_DIR | CHARLIEBOT_RUN_TOKEN) return 1 ;;
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

# The gateway tokens go to the host on this channel's standard input alone, and the runner writes
# them straight into the round's credentials file. This host holds them in its environment only.
tokens=()
if [[ -n ${ANTHROPIC_AUTH_TOKEN:-} ]]; then
  tokens+=("ANTHROPIC_AUTH_TOKEN=$ANTHROPIC_AUTH_TOKEN")
fi
if [[ -n ${ANTHROPIC_API_KEY:-} ]]; then
  tokens+=("ANTHROPIC_API_KEY=$ANTHROPIC_API_KEY")
fi
(( ${#tokens[@]} > 0 )) || fail 1 'the environment carries no ANTHROPIC_AUTH_TOKEN and no ANTHROPIC_API_KEY'
set +e
printf '%s\0' "${tokens[@]}" |
  "${SSH[@]}" "$REMOTE_HOST" "bash \$HOME/improve/launch/$launch_id/remote-runner.sh --prepare $launch_id" 3>&-
prepare_status=$?
set -e
if (( prepare_status == 255 )); then
  fail 3 "cannot reach the improve host $REMOTE_HOST; approve the connection on the /host-auth page"
fi
(( prepare_status == 0 )) || fail 1 "the runner could not write the round's credentials.yaml on $REMOTE_HOST"

# ---------- 4. Start ----------
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
  "bash \$HOME/improve/launch/$launch_id/remote-runner.sh $launch_id $direction $cli_commit" 3>&-
run_status=$?
set -e

# ---------- 5. End ----------
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
