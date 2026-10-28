#!/usr/bin/env bash
# The single entry point for a round's text-carrying GitHub writes.
#
# Every pull-request title, body, comment, commit message and branch name that a
# round sends goes through here: the script first scans the text with the full
# leak rules (tools/check-skills-host-leak.sh --stdin) and sends nothing on a
# hit. Text that the scanner refused is rewritten by the caller and resubmitted,
# so what reaches GitHub has always passed the check.
#
# Calls that carry no round-written text stay bare gh: gh pr merge,
# gh pr update-branch, gh run rerun, and the read-only calls.
#
# usage:
#   github-post.sh pr-create --title <title> --body-file <file> [gh pr create args...]
#   github-post.sh pr-comment --body-file <file> [gh pr comment args...]
#   github-post.sh pr-close --comment-file <file> [--delete-branch] [gh pr close args...]
#   github-post.sh pr-edit [--title <title>] [--body-file <file>] [gh pr edit args...]
#   github-post.sh push [git push args...]
#
# Exit codes: 0 sent, 1 leak hit (nothing sent), 2 setup refusal (nothing sent).
set -euo pipefail

usage() {
  sed -n '/^# usage:/,/^# Exit codes:/p' "$0" >&2
}

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
scanner="$repo_root/tools/check-skills-host-leak.sh"

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT

# The pattern file lives in the profile home, resolved the way
# tools/leak-patterns.sh resolves it. Missing or unreadable is a refusal, not a
# silent pass: nothing goes out unchecked.
patterns_file=$(PYTHONPATH="$repo_root${PYTHONPATH:+:$PYTHONPATH}" python3 -c \
  'from src.infra import home; print(home.charliebot_home_dir() / "skills_leak_patterns.local.txt")') || {
  printf 'Cannot resolve the profile home for the leak pattern file; nothing was sent.\n' >&2
  exit 2
}
if [[ ! -r "$patterns_file" ]]; then
  printf 'Leak pattern file is absent or unreadable: %s\n' "$patterns_file" >&2
  exit 2
fi
export EXTRA_PATTERNS_FILE="$patterns_file"

# check_text <field> <file>: scan the file as one text.
# A hit names the field; the scanner output above carries the matched lines.
check_text() {
  local field=$1 file=$2 status
  set +e
  "$scanner" --stdin < "$file"
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    return 0
  fi
  if [[ "$status" -eq 1 ]]; then
    printf 'host-leak check hit in %s; nothing was sent\n' "$field" >&2
    exit 1
  fi
  exit "$status"
}

check_string() {  # check_string <field> <text>
  local field=$1 text=$2
  printf '%s\n' "$text" > "$tmp_dir/field"
  check_text "$field" "$tmp_dir/field"
}

require_readable() {  # require_readable <path> <what>
  if [[ ! -r "$1" ]]; then
    printf 'Cannot read %s file: %s\n' "$2" "$1" >&2
    exit 2
  fi
}

[[ $# -ge 1 ]] || { usage; exit 2; }
subcommand=$1
shift
orig_args=("$@")
title='' body_file='' comment_file=''

# Text reaches gh only through the checked fields. An inline text argument would
# go to gh unchecked, so it is refused before any call; its value is not echoed.
while [[ $# -gt 0 ]]; do
  case "$1" in
    --title|-t) title=$2; shift 2 ;;
    --body-file|-F) body_file=$2; shift 2 ;;
    --comment-file) comment_file=$2; shift 2 ;;
    --body|-b|--comment|-c|--title=*|--body=*|--body-file=*|--comment-file=*|--comment=*|-[btcF]?*)
      printf 'Refused an inline text argument; pass text with --title, --body-file or --comment-file.\n' >&2
      exit 2
      ;;
    *) shift ;;
  esac
done

case "$subcommand" in
  pr-create)
    [[ -n "$title" && -n "$body_file" ]] || { usage; exit 2; }
    require_readable "$body_file" body
    check_string title "$title"
    check_text body "$body_file"
    gh pr create "${orig_args[@]}"
    ;;
  pr-comment)
    [[ -n "$body_file" ]] || { usage; exit 2; }
    require_readable "$body_file" body
    check_text body "$body_file"
    gh pr comment "${orig_args[@]}"
    ;;
  pr-close)
    [[ -n "$comment_file" ]] || { usage; exit 2; }
    require_readable "$comment_file" comment
    check_text body "$comment_file"
    gh_args=()
    skip_next=0
    for arg in "${orig_args[@]}"; do
      if [[ $skip_next -eq 1 ]]; then
        skip_next=0
        continue
      fi
      if [[ "$arg" == --comment-file ]]; then
        skip_next=1
        continue
      fi
      gh_args+=("$arg")
    done
    gh pr close "${gh_args[@]}" --comment "$(cat "$comment_file")"
    ;;
  pr-edit)
    [[ -n "$title" || -n "$body_file" ]] || { usage; exit 2; }
    [[ -z "$title" ]] || check_string title "$title"
    [[ -z "$body_file" ]] || { require_readable "$body_file" body; check_text body "$body_file"; }
    gh pr edit "${orig_args[@]}"
    ;;
  push)
    # The pushed branch name (each refspec side) and the current branch, then
    # every commit message in origin/main..HEAD. The commit text lands on main
    # through the squash merge, so it is checked like a pull-request body.
    current_branch=$(git branch --show-current)
    [[ -z "$current_branch" ]] || check_string 'branch name' "$current_branch"
    value_opts='^(-o|--push-option|--receive-pack|--exec|--repo)$'
    positional=()
    skip_next=0
    for arg in "${orig_args[@]}"; do
      if [[ $skip_next -eq 1 ]]; then
        skip_next=0
        continue
      fi
      if [[ "$arg" =~ $value_opts ]]; then
        skip_next=1
        continue
      fi
      [[ "$arg" == -* ]] || positional+=("$arg")
    done
    specs=()
    if [[ ${#positional[@]} -ge 2 ]]; then
      specs=("${positional[@]:1}")
    elif [[ ${#positional[@]} -eq 1 && "${positional[0]}" != *:* ]]; then
      specs=("${positional[0]}")
    fi
    for spec in "${specs[@]}"; do
      sides=("${spec%%:*}")
      if [[ "$spec" == *:* ]]; then
        sides+=("${spec#*:}")
      fi
      for side in "${sides[@]}"; do
        side=${side#+}
        side=${side#refs/heads/}
        [[ -z "$side" ]] || check_string 'branch name' "$side"
      done
    done
    # A failed listing must stop the push: a process substitution would hide the failure and push unchecked.
    commits=$(git rev-list origin/main..HEAD) || {
      printf 'Cannot list origin/main..HEAD for the commit check; nothing was pushed.\n' >&2
      exit 2
    }
    while IFS= read -r sha; do
      [[ -n "$sha" ]] || continue
      git show -s --format=%B "$sha" > "$tmp_dir/field"
      check_text "commit $sha" "$tmp_dir/field"
    done <<< "$commits"
    git push "${orig_args[@]}"
    ;;
  *)
    usage
    exit 2
    ;;
esac
