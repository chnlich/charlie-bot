#!/usr/bin/env bash
# The single entry point for a round's text-carrying GitHub writes.
#
# Every pull-request title, body, comment, commit message and branch name that a
# round sends goes through here. The script first scans the text for the exact
# value of each credential the round holds, then with the leak rules
# (dev/check-skills-host-leak.sh), and sends nothing on a hit. A leak-rule hit
# is rewritten by the caller and resubmitted. A credential hit stops the round:
# the scanner names the kind, and nothing is rewritten.
#
# The push subcommand's diffs are checked by the pre-push hook of the clone
# (dev/git-hooks/pre-push), which this script runs through git push.
#
# Calls that carry no round-written text stay bare gh: gh pr update-branch,
# gh run rerun, and the read-only calls. A merge goes through pr-merge, which
# refuses a web page change whose evidence directory fails the screenshot check
# or whose pull request body names no View line, and carries the View lines into
# the squash commit message.
#
# pr-edit does not call gh pr edit: on gh 2.45 that fails on the Projects
# (classic) lookup. It reads the PR's URL with gh pr view, then sends the
# checked title and body with gh api -X PATCH on the REST pull endpoint.
#
# usage:
#   github-post.sh pr-create --title <title> --body-file <file> [gh pr create args...]
#   github-post.sh pr-comment --body-file <file> [gh pr comment args...]
#   github-post.sh pr-close --comment-file <file> [--delete-branch] [gh pr close args...]
#   github-post.sh pr-edit [--title <title>] [--body-file <file>] [--repo <owner/repo>] [<PR>]
#   github-post.sh pr-merge --evidence-dir <dir> <PR> [--repo <owner/repo>]
#   github-post.sh push [git push args...]
#
# Exit codes: 0 sent, 1 leak hit (nothing sent), 2 setup refusal (nothing sent),
# 3 credential hit (nothing sent; the round stops).
set -euo pipefail

usage() {
  sed -n '/^# usage:/,/^# Exit codes:/p' "$0" >&2
}

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
scanner="$repo_root/dev/check-skills-host-leak.sh"

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT

# The pattern file lives in the profile home, resolved the way
# dev/leak-patterns.sh resolves it: CHARLIEBOT_HOME when set (blank counts as
# unset), ~/.charliebot otherwise, a leading ~ in the value is HOME. Missing or
# unreadable is a refusal, not a silent pass: nothing goes out unchecked.
profile_home=${CHARLIEBOT_HOME:-$HOME/.charliebot}
patterns_file="${profile_home/#\~/$HOME}/skills_leak_patterns.local.txt"
if [[ ! -r "$patterns_file" ]]; then
  printf 'Leak pattern file is absent or unreadable: %s\n' "$patterns_file" >&2
  exit 2
fi
export EXTRA_PATTERNS_FILE="$patterns_file"

# The credential values of this process, read once: a setup refusal (exit 2) sends nothing.
credential_values="$tmp_dir/credential-values"
"$scanner" --credential-values > "$credential_values" || exit 2

# check_text <field> <file>: scan the file as one text, first for the credential values, then with the leak rules.
# The scanner output above names the hit: a credential hit prints its kind and field, a leak-rule hit the matched lines.
check_text() {
  local field=$1 file=$2 status
  set +e
  "$scanner" --credential-check "$credential_values" "$field" < "$file"
  status=$?
  set -e
  if [[ "$status" -eq 3 ]]; then
    printf 'credential check hit in %s; nothing was sent. The round stops here: the kind named above is the reason.\n' \
      "$field" >&2
    exit 3
  fi
  [[ "$status" -eq 0 ]] || exit "$status"
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
    # The PATCH sends only the title and body, and takes its PR from the selector and --repo.
    # Any other argument would be dropped unsent, so it is refused by name before any check or call.
    pr_selector='' pr_repo=''
    set -- "${orig_args[@]}"
    while [[ $# -gt 0 ]]; do
      case "$1" in
        --title|-t|--body-file|-F) shift 2 ;;
        --repo|-R)
          [[ $# -ge 2 && -n "$2" ]] || { printf 'Refused %s without a repo value.\n' "$1" >&2; exit 2; }
          pr_repo=$2
          shift 2
          ;;
        --repo=*)
          pr_repo=${1#--repo=}
          [[ -n "$pr_repo" ]] || { printf 'Refused --repo without a repo value.\n' >&2; exit 2; }
          shift
          ;;
        -*)
          printf 'Refused %s: pr-edit sends only --title, --body-file, --repo and a PR selector.\n' "${1%%=*}" >&2
          exit 2
          ;;
        *)
          [[ -n "$1" ]] || { printf 'Refused an empty PR selector.\n' >&2; exit 2; }
          [[ -z "$pr_selector" ]] || { printf 'Refused a second PR selector: pr-edit takes one.\n' >&2; exit 2; }
          pr_selector=$1
          shift
          ;;
      esac
    done
    [[ -z "$title" ]] || check_string title "$title"
    [[ -z "$body_file" ]] || { require_readable "$body_file" body; check_text body "$body_file"; }
    # gh pr view resolves the PR from the same selector and --repo that gh pr edit took. Its URL carries the
    # owner, repo and number for the REST call. Without a selector, gh picks the PR of the current branch.
    view_args=(pr view)
    [[ -z "$pr_selector" ]] || view_args+=("$pr_selector")
    [[ -z "$pr_repo" ]] || view_args+=(--repo "$pr_repo")
    pr_url=$(gh "${view_args[@]}" --json url --jq .url)
    [[ "$pr_url" =~ ^https://[^/]+/([^/]+)/([^/]+)/pull/([0-9]+)$ ]] || {
      printf 'Cannot read the owner, repo and number from the PR URL gh printed: %s\n' "$pr_url" >&2
      exit 2
    }
    patch_args=(api -X PATCH "repos/${BASH_REMATCH[1]}/${BASH_REMATCH[2]}/pulls/${BASH_REMATCH[3]}")
    [[ -z "$title" ]] || patch_args+=(-f "title=$title")
    [[ -z "$body_file" ]] || patch_args+=(-F "body=@$body_file")
    gh "${patch_args[@]}" > /dev/null
    printf '%s\n' "$pr_url"
    ;;
  pr-merge)
    # The round's merge. A web page change passes the screenshot check of
    # `charliebot screenshots check`, which reads the changed paths on stdin, and
    # its pull request body must carry a View line. The check prints a
    # `web page change:` line exactly when the change alters a web page, so this
    # script keeps no second copy of the rule. The View lines go into the squash
    # commit message as trailers, which the release switch reads back to capture
    # the live page.
    evidence_dir='' pr_selector='' pr_repo=''
    set -- "${orig_args[@]}"
    while [[ $# -gt 0 ]]; do
      case "$1" in
        --evidence-dir)
          [[ $# -ge 2 && -n "$2" ]] || { printf 'Refused --evidence-dir without a directory.\n' >&2; exit 2; }
          evidence_dir=$2
          shift 2
          ;;
        --evidence-dir=*)
          evidence_dir=${1#--evidence-dir=}
          [[ -n "$evidence_dir" ]] || { printf 'Refused --evidence-dir without a directory.\n' >&2; exit 2; }
          shift
          ;;
        --repo|-R)
          [[ $# -ge 2 && -n "$2" ]] || { printf 'Refused %s without a repo value.\n' "$1" >&2; exit 2; }
          pr_repo=$2
          shift 2
          ;;
        --repo=*)
          pr_repo=${1#--repo=}
          [[ -n "$pr_repo" ]] || { printf 'Refused --repo without a repo value.\n' >&2; exit 2; }
          shift
          ;;
        -*)
          printf 'Refused %s: pr-merge takes --evidence-dir, --repo and one PR selector.\n' "${1%%=*}" >&2
          exit 2
          ;;
        *)
          [[ -n "$1" ]] || { printf 'Refused an empty PR selector.\n' >&2; exit 2; }
          [[ -z "$pr_selector" ]] || { printf 'Refused a second PR selector: pr-merge takes one.\n' >&2; exit 2; }
          pr_selector=$1
          shift
          ;;
      esac
    done
    [[ -n "$pr_selector" ]] || { usage; exit 2; }
    repo_args=()
    [[ -z "$pr_repo" ]] || repo_args=(--repo "$pr_repo")
    changed=$(gh pr diff --name-only "$pr_selector" "${repo_args[@]}") || {
      printf 'Cannot read the changed paths of PR %s; nothing was merged.\n' "$pr_selector" >&2
      exit 2
    }
    # `charliebot` is the round's own build (dev/improve/remote-runner.sh builds it), so it carries
    # the window sizes this check reads.
    check_args=(screenshots check)
    [[ -z "$evidence_dir" ]] || check_args+=(--dir "$evidence_dir")
    set +e
    web_change=$(printf '%s\n' "$changed" | charliebot "${check_args[@]}")
    check_status=$?
    set -e
    if [[ "$check_status" -ne 0 ]]; then
      if [[ -z "$evidence_dir" ]]; then
        printf 'PR %s changes a web page, so pr-merge needs --evidence-dir naming the round evidence directory; nothing was merged.\n' \
          "$pr_selector" >&2
      else
        printf 'PR %s did not pass the screenshot check; nothing was merged.\n' "$pr_selector" >&2
      fi
      exit 2
    fi
    merge_args=(pr merge --squash "$pr_selector" "${repo_args[@]}")
    if [[ -n "$web_change" ]]; then
      pr_title=$(gh pr view "$pr_selector" "${repo_args[@]}" --json title --jq .title) || {
        printf 'Cannot read the title of PR %s; nothing was merged.\n' "$pr_selector" >&2
        exit 2
      }
      gh pr view "$pr_selector" "${repo_args[@]}" --json body --jq .body > "$tmp_dir/pr-body" || {
        printf 'Cannot read the body of PR %s; nothing was merged.\n' "$pr_selector" >&2
        exit 2
      }
      sed -n 's/^View:[[:space:]]*//p' "$tmp_dir/pr-body" > "$tmp_dir/views"
      if [[ ! -s "$tmp_dir/views" ]]; then
        printf 'PR %s changes a web page, so its body must carry a View: line; nothing was merged.\n' "$pr_selector" >&2
        exit 2
      fi
      # The View lines move to the message's end, so the squash commit carries each exactly once, as a trailer
      # block; a duplicate line would capture the live page twice at the release switch.
      { sed '/^View:/d' "$tmp_dir/pr-body"; printf '\n'; sed 's/^/View: /' "$tmp_dir/views"; } > "$tmp_dir/merge-body"
      check_string title "$pr_title"
      check_text body "$tmp_dir/merge-body"
      merge_args+=(--subject "$pr_title" --body-file "$tmp_dir/merge-body")
    fi
    gh "${merge_args[@]}"
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
      check_text "commit $sha message" "$tmp_dir/field"
    done <<< "$commits"
    # The pre-push hook checks each pushed diff. Its refusal exits git's push with 1, so the hook also writes the
    # credential hit to this file, and the round's exit names the hit with code 3.
    hit_file="$tmp_dir/credential-hit"
    push_status=0
    CHARLIEBOT_CREDENTIAL_HIT_FILE="$hit_file" git push "${orig_args[@]}" || push_status=$?
    if [[ -s "$hit_file" ]]; then
      cat -- "$hit_file" >&2
      printf 'nothing was pushed; the round stops here: the kind named above is the reason.\n' >&2
      exit 3
    fi
    exit "$push_status"
    ;;
  *)
    usage
    exit 2
    ;;
esac
