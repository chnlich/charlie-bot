#!/usr/bin/env bash
# Check the event logs of one improve round for a skill file read outside the round's skills directory.
#
#   skill-reads.sh <skills dir> <event log>...
#
# An event log is the stream-json that a round's agent or review writes, one JSON object per line. The check reads
# each tool call's read target: a `file_path`, `notebook_path` or `path` value names the file the call reads, and each
# token of a `command` value is a read only when it is rooted: absolute, `~/`, `$VAR/`, or git's `<ref>:skills/...`.
# A relative `skills/<name>` in a command, such as a commit message or a heredoc body, reads nothing. A path is a skill
# file when it has a `skills/<name>` component. A skill file is allowed only under <skills dir> and holds no `..` step.
# Each other one is printed to stderr as `skill read outside <skills dir>: <path>`. A read target whose tool_use pairs
# with a tool_result carrying is_error true read nothing and is no hit. A tool_use with no result, a result without
# is_error, and a command token count whatever their results say: only a failed result clears its own read target.
#
# Exit codes: 0 every skill read lies under the skills dir; 1 a skill read lies outside it, and each is printed;
# 2 the arguments are wrong, or an event log is unreadable or not JSON, and stderr names it.
set -euo pipefail

usage() {
  printf 'usage: %s <skills dir> <event log>...\n' "$0" >&2
  exit 2
}

(( $# >= 2 )) || usage
skills=${1%/}
shift
[[ -n $skills ]] || { printf 'skill-reads: the skills dir is empty\n' >&2; exit 2; }

read -r -d '' failed_program <<'JQ' || true
[ .. | objects | select(.type == "tool_result") | select(.is_error == true) | .tool_use_id ] | .[]
JQ
read -r -d '' program <<'JQ' || true
def skill_file: test("(^|[/:])skills/[^/]+");
def rooted: test("^(/|~/|[$])") or contains(":skills/");
def allowed: startswith($skills + "/") and (contains("/../") | not);
def path_key: . == "file_path" or . == "notebook_path" or . == "path";
[ $failed | scan("[^\n]+") ] as $failed
| [ .. | objects | select(.type == "tool_use")
    | . as $call | .input | to_entries[]
    | select(.value | type == "string")
    | if (.key | path_key) then
        select(($failed | index($call.id)) == null) | .value
      elif .key == "command" then (.value | scan("[^ \t\n\"'`;|&()<>=]+") | select(rooted))
      else empty end
    | select(skill_file and (allowed | not)) ]
| unique[]
JQ

status=0
for events in "$@"; do
  [[ -r $events ]] || { printf 'skill-reads: cannot read the event log: %s\n' "$events" >&2; exit 2; }
  # The failed ids of the log, one per line: a result line pairs with its call by id, so the main pass, which runs one
  # program per line, takes the ids it collected rather than looking for a result in the call's own line.
  failed=$(jq -r "$failed_program" "$events") || {
    printf 'skill-reads: the event log is not JSON: %s\n' "$events" >&2
    exit 2
  }
  hits=$(jq -r --arg skills "$skills" --arg failed "$failed" "$program" "$events") || {
    printf 'skill-reads: the event log is not JSON: %s\n' "$events" >&2
    exit 2
  }
  if [[ -n $hits ]]; then
    while IFS= read -r path; do
      printf 'skill read outside %s: %s\n' "$skills" "$path" >&2
    done <<<"$hits"
    status=1
  fi
done
exit "$status"
