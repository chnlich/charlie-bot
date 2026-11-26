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
# The push subcommand checks the pushed text in-process before calling git push — names, commits, diffs,
# decompressed blobs and docs — and the pre-push hook of the clone (dev/git-hooks/pre-push) applies the same
# checks inside git push, so a clone without the hooks path is still covered.
#
# Calls that carry no round-written text stay bare gh: gh api -X PUT
# "repos/{owner}/{repo}/pulls/<PR>/update-branch", gh run rerun, and the read-only calls.
# The gh shim checks their arguments. gh 2.45 has no gh pr update-branch command,
# so branch updates use the REST endpoint. A merge goes through pr-merge, which
# refuses a draft pull request, a diff that changes a claimed path, a web page change whose evidence directory fails
# the screenshot check, whose body lacks a full-URL View line, or whose View value
# does not start with `/`, and carries the View lines into the squash commit message.
# For a web page change, the review record (--review-record: the review's events.ndjson, kept by landing.md Step 6
# as review-events.ndjson in the round's evidence directory) must show a read of each screenshot the body names.
# It also refuses a merge unless the newest verdict comment names the pull request's head sha and reports no
# issues. pr-comment --verdict-head writes that comment: its first line names the head the review read, and the
# call refuses any head that is not the pull request's full head sha as GitHub reads it now, before it posts.
# pr-merge also refuses a merge whose round event log or review record reads a skill file outside the round's skills
# directory; dev/improve/skill-reads.sh names each such path. Inside a round (IMPROVE_LAUNCH_DIR is set) the round's
# facts are the live launch directory and IMPROVE_SKILLS_DIR. A merge outside a round takes them from the evidence
# directory that --evidence-dir names, where the round kept its copies (see merge_facts).
# After the merge, pr-merge reads the pull request back from GitHub and prints its number, reviewed head, merge commit
# and state; a readback that does not show the merge at the reviewed head exits 4.
#
# pr-edit does not call gh pr edit: on gh 2.45 that fails on the Projects
# (classic) lookup. It reads the PR's URL with gh pr view, then sends the
# checked title and body with gh api -X PATCH on the REST pull endpoint.
#
# pr-create and pr-merge check the side record citations of the body (dev/improve/side-record.sh): a line that
# contains `Side record:` outside the accepted form is refused. The cited records are checked against the round's
# evidence directory when they receive --evidence-dir, and a body that cites a record without it is refused.
# pr-create catches a bad citation before the pull request exists. pr-merge runs the check on the body as it will
# merge, so an edit made after creation is checked too; pr-edit does not check.
#
# pr-create of the code-style direction (IMPROVE_DIRECTION=code_style) refuses a body whose rule lines do not quote the
# cited lines of the pinned Google style guide in the kit's dev/improve/google-styleguide/, and a body with no rule
# line (dev/improve/code-style-rules.sh). Other directions' pr-create takes no such check.
#
# pr-create and pr-merge also refuse a diff that changes a path another branch claims in the round's in-flight list,
# inflight-files.txt, the list Step 3 reads: the launch directory's inside a round, the evidence directory's outside
# one. A claim that the list marks absent-on-origin-main does
# not refuse. The pull request's own head branch claims nothing for itself. A refusal names each claimed path with its
# claimant and sends nothing.
#
# pr-create also refuses a diff that deletes or renames a path which the head's tree still names outside recorded
# data, or names a recorded line that the body does not name as <path>:<line> (dev/improve/moved-references.sh). A
# refusal names each line and sends nothing. pr-merge does not repeat this check.
#
# usage:
#   github-post.sh pr-create --title <title> --body-file <file> [--evidence-dir <dir>] [--draft] [gh pr create args...]
#   github-post.sh pr-comment --body-file <file> [--verdict-head <sha>] [gh pr comment args...]
#   github-post.sh pr-close --comment-file <file> [--delete-branch] [gh pr close args...]
#   github-post.sh pr-edit [--title <title>] [--body-file <file>] [--repo <owner/repo>] [<PR>]
#   github-post.sh pr-merge [--evidence-dir <dir>] [--review-record <file>] <PR> [--repo <owner/repo>]
#   github-post.sh push [git push args...]
#
# Exit codes: 0 sent, 1 leak hit (nothing sent), 2 setup refusal (nothing sent),
# 3 credential hit (nothing sent; the round stops), 4 merge sent but its readback does not confirm it.
set -euo pipefail

usage() {
  sed -n '/^# usage:/,/^# Exit codes:/p' "$0" >&2
}

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
scanner="$repo_root/dev/check-skills-host-leak.sh"
source "$repo_root/dev/push-text-lists.sh"
source "$repo_root/dev/improve/side-record.sh"
source "$repo_root/dev/improve/code-style-rules.sh"
source "$repo_root/dev/improve/moved-references.sh"

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT

# The local pattern file must hold a pattern before any text leaves: missing, unreadable or empty outside
# comments is a refusal, not a silent pass, so nothing goes out with its host rules silently dropped.
tmp_patterns="$tmp_dir/patterns"
: > "$tmp_patterns"
source "$repo_root/dev/leak-patterns.sh"
load_host_patterns --require
export EXTRA_PATTERNS_FILE="$extra_patterns_file"

# The credential values of this process, read once: a setup refusal (exit 2) sends nothing.
credential_values="$tmp_dir/credential-values"
"$scanner" --credential-values > "$credential_values" || exit 2

# check_text <field> <file>: scan the file as one text, first for the credential values, then with the leak rules.
# Each scanner run writes its output to the check log. The log reaches stderr only when the check refuses: a credential
# hit names its kind there, a leak-rule hit its matched lines. A passing check prints nothing, so stdout carries the
# subcommand's own result alone.
check_text() {
  local field=$1 file=$2 status
  local log="$tmp_dir/check.log"
  set +e
  "$scanner" --credential-check "$credential_values" "$field" < "$file" > "$log"
  status=$?
  set -e
  if [[ "$status" -eq 3 ]]; then
    cat -- "$log" >&2
    printf 'credential check hit in %s; nothing was sent. The round stops here: the kind named above is the reason.\n' \
      "$field" >&2
    exit 3
  fi
  [[ "$status" -eq 0 ]] || { cat -- "$log" >&2; exit "$status"; }
  set +e
  "$scanner" --require-local-patterns --stdin < "$file" > "$log"
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    return 0
  fi
  cat -- "$log" >&2
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

# check_inflight_claims <changed paths file> <head branch> <in-flight list>: return 1 after naming each path that the
# diff changes and another branch claims on a path origin/main has. A claim the list marks absent-on-origin-main does not
# refuse. The list is the round's in-flight list, the file the runner writes at launch and Step 3 reads, so the check
# and the round's own avoidance share one source. The head branch never refuses itself.
check_inflight_claims() {
  local changed=$1 head=$2 list=$3
  local -A changed_paths=()
  local path claimed branch absent found=0
  require_readable "$list" "in-flight list"
  while IFS= read -r path; do
    [[ -n $path ]] || continue
    changed_paths["$path"]=1
  done < "$changed"
  while IFS=$'\t' read -r claimed branch absent; do
    [[ -n $claimed && -n ${changed_paths["$claimed"]:-} ]] || continue
    [[ $branch != "$head" ]] || continue
    [[ $absent != absent-on-origin-main ]] || continue
    printf 'claimed path %s, claimed by %s\n' "$claimed" "$branch" >&2
    found=1
  done < "$list"
  [[ $found -eq 0 ]]
}

# without_evidence_dir: set gh_args to the call's arguments without each --evidence-dir option, which this script reads.
without_evidence_dir() {
  local skip_next=0 arg
  gh_args=()
  for arg in "${orig_args[@]}"; do
    if [[ $skip_next -eq 1 ]]; then
      skip_next=0
      continue
    fi
    case "$arg" in
      --evidence-dir) skip_next=1 ;;
      --evidence-dir=*) ;;
      *) gh_args+=("$arg") ;;
    esac
  done
}

# check_verdict_head <head> <gh pr comment args...>: exit 2 unless <head> is the full 40-character sha that GitHub
# reads as the head of the pull request the call names. An empty or short head, or the sha of an older commit,
# sends nothing. The refusal names the given head and the pull request's head.
check_verdict_head() {
  local head=$1 selector='' repo='' pr_head target view_args
  shift
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --verdict-head|--body-file|-F) shift 2 ;;
      --repo|-R)
        [[ $# -ge 2 && -n "$2" ]] || { printf 'Refused %s without a repo value.\n' "$1" >&2; exit 2; }
        repo=$2
        shift 2
        ;;
      --repo=*)
        repo=${1#--repo=}
        [[ -n "$repo" ]] || { printf 'Refused --repo without a repo value.\n' >&2; exit 2; }
        shift
        ;;
      -*) shift ;;
      *)
        [[ -n "$1" ]] || { printf 'Refused an empty PR selector.\n' >&2; exit 2; }
        [[ -z "$selector" ]] || { printf 'Refused a second PR selector: pr-comment takes one.\n' >&2; exit 2; }
        selector=$1
        shift
        ;;
    esac
  done
  # gh pr view reads the pull request that gh pr comment names, and the current branch's when the call names none.
  target="PR ${selector:-(current branch)}"
  view_args=(pr view)
  [[ -z "$selector" ]] || view_args+=("$selector")
  [[ -z "$repo" ]] || view_args+=(--repo "$repo")
  pr_head=$(gh "${view_args[@]}" --json headRefOid --jq .headRefOid) || {
    printf 'Cannot read the head of %s; nothing was sent.\n' "$target" >&2
    exit 2
  }
  if [[ ! "$head" =~ ^[0-9a-f]{40}$ || "$head" != "$pr_head" ]]; then
    printf 'Refused --verdict-head "%s": the head of %s is %s, and a verdict names that full 40-character sha; ' \
      "$head" "$target" "$pr_head" >&2
    printf 'nothing was sent.\n' >&2
    exit 2
  fi
}

# merge_facts: set facts_dir and skills_dir, the round facts that pr-merge's claim check and skill check read. Inside a
# round they are the live launch directory and IMPROVE_SKILLS_DIR. Outside a round they are the copies that the round
# kept in the evidence directory that --evidence-dir names: the in-flight list, the event log and the kit revision. The
# kit revision names the skills directory of this host's runner. Each missing fact, or a missing skills directory, stops
# the merge and names it. This runs before any call to GitHub, so nothing is sent.
merge_facts() {
  local fact revision
  if [[ -n ${IMPROVE_LAUNCH_DIR-} ]]; then
    [[ -n ${IMPROVE_SKILLS_DIR-} ]] || {
      printf 'Refused pr-merge: IMPROVE_LAUNCH_DIR and IMPROVE_SKILLS_DIR name the round; nothing was merged.\n' >&2
      exit 2
    }
    facts_dir=$IMPROVE_LAUNCH_DIR
    skills_dir=$IMPROVE_SKILLS_DIR
    return 0
  fi
  [[ -n $evidence_dir ]] || {
    printf 'Refused pr-merge outside a round: --evidence-dir must name the round evidence directory; ' >&2
    printf 'nothing was merged.\n' >&2
    exit 2
  }
  for fact in inflight-files.txt round-events.ndjson kit-revision; do
    [[ -r "$evidence_dir/$fact" ]] || {
      printf 'Refused pr-merge outside a round: the evidence directory lacks %s; nothing was merged.\n' \
        "$evidence_dir/$fact" >&2
      exit 2
    }
  done
  revision=$(<"$evidence_dir/kit-revision")
  [[ $revision =~ ^[0-9a-f]{40}$ ]] || {
    printf 'Refused pr-merge: %s does not hold a full kit revision; nothing was merged.\n' \
      "$evidence_dir/kit-revision" >&2
    exit 2
  }
  # The runner keeps each kit revision's skills at ~/improve/runner/bin/<revision>/skills
  # (dev/improve/remote-runner.sh).
  skills_dir="$HOME/improve/runner/bin/$revision/skills"
  [[ -d $skills_dir ]] || {
    printf 'Refused pr-merge: this host has no skills directory for kit revision %s: %s; nothing was merged.\n' \
      "$revision" "$skills_dir" >&2
    exit 2
  }
  facts_dir=$evidence_dir
}

[[ $# -ge 1 ]] || { usage; exit 2; }
subcommand=$1
shift
orig_args=("$@")
title='' body_file='' comment_file='' verdict_head='' verdict_requested=0 evidence_dir=''

# Text reaches gh only through the checked fields. An inline text argument would
# go to gh unchecked, so it is refused before any call; its value is not echoed.
while [[ $# -gt 0 ]]; do
  case "$1" in
    --title|-t) title=$2; shift 2 ;;
    --body-file|-F) body_file=$2; shift 2 ;;
    --comment-file) comment_file=$2; shift 2 ;;
    --verdict-head)
      [[ $# -ge 2 ]] || { printf 'Refused --verdict-head without a head value; nothing was sent.\n' >&2; exit 2; }
      verdict_requested=1
      verdict_head=$2
      shift 2
      ;;
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
    # The code-style direction names each rule it applies from the kit's pinned copy of its guide.
    if [[ ${IMPROVE_DIRECTION-} == code_style ]]; then
      code_style_check_body "$body_file" || exit 2
    fi
    # A draft waits for its measurements, so it may cite no record yet; its cited records still pass the check.
    # The head is the branch the call names. Without --head, gh takes the current branch, and so does this check.
    draft=0
    pr_head=$(git branch --show-current) || {
      printf 'Cannot read the current branch for the claim check; nothing was sent.\n' >&2
      exit 2
    }
    set -- "${orig_args[@]}"
    while [[ $# -gt 0 ]]; do
      case "$1" in
        --draft|-d)
          draft=1
          shift
          ;;
        --head|-H)
          [[ $# -ge 2 && -n "$2" ]] || { printf 'Refused %s without a branch.\n' "$1" >&2; exit 2; }
          pr_head=$2
          shift 2
          ;;
        --head=*)
          pr_head=${1#--head=}
          [[ -n "$pr_head" ]] || { printf 'Refused --head without a branch.\n' >&2; exit 2; }
          shift
          ;;
        *) shift ;;
      esac
    done
    side_record_check_body "$evidence_dir" "$body_file" "$draft" || exit 2
    # The claim check reads the diff the pull request will show: its head against origin/main, the round's base.
    git diff --name-only "origin/main...$pr_head" > "$tmp_dir/changed" || {
      printf 'Cannot read the changed paths of %s for the claim check; nothing was sent.\n' "$pr_head" >&2
      exit 2
    }
    [[ -n ${IMPROVE_LAUNCH_DIR:-} ]] || {
      printf 'Cannot check the claims of the pull request: IMPROVE_LAUNCH_DIR is unset; nothing was sent or merged.\n' \
        >&2
      exit 2
    }
    check_inflight_claims "$tmp_dir/changed" "$pr_head" "$IMPROVE_LAUNCH_DIR/inflight-files.txt" || {
      printf 'PR changes a path that live work claims; nothing was sent.\n' >&2
      exit 2
    }
    moved_references_check "$tmp_dir" "$pr_head" "$body_file" || exit 2
    without_evidence_dir
    gh pr create "${gh_args[@]}"
    ;;
  pr-comment)
    [[ -n "$body_file" ]] || { usage; exit 2; }
    require_readable "$body_file" body
    # A verdict comment opens with the full head sha its review read: pr-merge reads that first line back.
    if [[ $verdict_requested -eq 1 ]]; then
      check_verdict_head "$verdict_head" "${orig_args[@]}"
      { printf 'Review verdict on %s\n\n' "$verdict_head"; cat -- "$body_file"; } > "$tmp_dir/verdict-body"
      body_file=$tmp_dir/verdict-body
    fi
    check_text body "$body_file"
    # gh receives the checked file, so the body it posts is the text that passed; the flags that name the
    # body and the verdict head are removed from the arguments first.
    gh_args=()
    skip_next=0
    for arg in "${orig_args[@]}"; do
      if [[ $skip_next -eq 1 ]]; then
        skip_next=0
        continue
      fi
      case "$arg" in
        --verdict-head|--body-file|-F) skip_next=1 ;;
        *) gh_args+=("$arg") ;;
      esac
    done
    gh pr comment "${gh_args[@]}" --body-file "$body_file"
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
    # its pull request body must carry a full-URL View line starting with `/`. The check prints a
    # `web page change:` line exactly when the change alters a web page, so this
    # script keeps no second copy of the rule. The check also reads the review record, which must
    # show a read of each screenshot the body names. The View lines go into the squash
    # commit message as trailers, which the release switch reads back to capture
    # the live page.
    evidence_dir='' review_record='' pr_selector='' pr_repo=''
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
        --review-record)
          [[ $# -ge 2 && -n "$2" ]] || { printf 'Refused --review-record without a file.\n' >&2; exit 2; }
          review_record=$2
          shift 2
          ;;
        --review-record=*)
          review_record=${1#--review-record=}
          [[ -n "$review_record" ]] || { printf 'Refused --review-record without a file.\n' >&2; exit 2; }
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
          printf 'Refused %s: pr-merge takes --evidence-dir, --review-record, --repo and one PR selector.\n' \
            "${1%%=*}" >&2
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
    merge_facts
    repo_args=()
    [[ -z "$pr_repo" ]] || repo_args=(--repo "$pr_repo")
    # A draft waits for its measurements, so this check stays ahead of every read and merge of the pull request.
    draft_state=$(gh pr view "$pr_selector" "${repo_args[@]}" --json isDraft --jq .isDraft) || {
      printf 'Cannot read the draft state of PR %s; nothing was merged.\n' "$pr_selector" >&2
      exit 2
    }
    case "$draft_state" in
      false) ;;
      true)
        printf 'PR %s is a draft: a draft waits for its measurements; nothing was merged.\n' "$pr_selector" >&2
        exit 2
        ;;
      *)
        printf 'Cannot read the draft state of PR %s; nothing was merged.\n' "$pr_selector" >&2
        exit 2
        ;;
    esac
    # The merge head carries a clean verdict: the newest verdict comment names the head sha and reports no issues.
    # A verdict on an older head, or one that reports findings, refuses the merge.
    merge_head=$(gh pr view "$pr_selector" "${repo_args[@]}" --json headRefOid --jq .headRefOid) || {
      printf 'Cannot read the head of PR %s; nothing was merged.\n' "$pr_selector" >&2
      exit 2
    }
    gh pr view "$pr_selector" "${repo_args[@]}" --json comments \
      --jq '[.comments[] | select(.body | startswith("Review verdict on "))] | last | .body // ""' \
      > "$tmp_dir/verdict" || {
      printf 'Cannot read the comments of PR %s; nothing was merged.\n' "$pr_selector" >&2
      exit 2
    }
    verdict_named=$(sed -n '1s/^Review verdict on //p' "$tmp_dir/verdict")
    if [[ -z "$verdict_named" ]]; then
      printf 'PR %s has no review verdict comment; nothing was merged.\n' "$pr_selector" >&2
      exit 2
    fi
    if [[ "$verdict_named" != "$merge_head" ]]; then
      printf 'PR %s: its newest review verdict names %s, not the head %s; nothing was merged.\n' \
        "$pr_selector" "$verdict_named" "$merge_head" >&2
      exit 2
    fi
    if ! grep -qxF 'Code review: no issues found.' "$tmp_dir/verdict" ||
      grep -qE '^- .*\(confidence [0-9]+\)' "$tmp_dir/verdict"; then
      printf 'PR %s: the review verdict on head %s reports findings; nothing was merged.\n' \
        "$pr_selector" "$merge_head" >&2
      exit 2
    fi
    # The body is read once: the side record check and the View lines below both use this copy, which is the
    # text the squash commit carries.
    gh pr view "$pr_selector" "${repo_args[@]}" --json body --jq .body > "$tmp_dir/pr-body" || {
      printf 'Cannot read the body of PR %s; nothing was merged.\n' "$pr_selector" >&2
      exit 2
    }
    side_record_check_body "$evidence_dir" "$tmp_dir/pr-body" 0 || {
      printf 'PR %s does not pass the side record check; nothing was merged.\n' "$pr_selector" >&2
      exit 2
    }
    changed=$(gh pr diff --name-only "$pr_selector" "${repo_args[@]}") || {
      printf 'Cannot read the changed paths of PR %s; nothing was merged.\n' "$pr_selector" >&2
      exit 2
    }
    printf '%s\n' "$changed" > "$tmp_dir/changed"
    pr_head=$(gh pr view "$pr_selector" "${repo_args[@]}" --json headRefName --jq .headRefName) || {
      printf 'Cannot read the head branch of PR %s; nothing was merged.\n' "$pr_selector" >&2
      exit 2
    }
    check_inflight_claims "$tmp_dir/changed" "$pr_head" "$facts_dir/inflight-files.txt" || {
      printf 'PR %s changes a path that live work claims; nothing was merged.\n' "$pr_selector" >&2
      exit 2
    }
    # `charliebot` is the round's own build (dev/improve/remote-runner.sh builds it), so it carries
    # the window sizes this check reads.
    check_args=(screenshots check --pr-body "$tmp_dir/pr-body")
    [[ -z "$evidence_dir" ]] || check_args+=(--dir "$evidence_dir")
    [[ -z "$review_record" ]] || check_args+=(--review-record "$review_record")
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
    # A round reads each skill file from its skills directory. The round's event log and the review record must hold no
    # skill read from any other copy; the check names each path it refuses.
    skill_logs=("$facts_dir/round-events.ndjson")
    [[ -z "$review_record" ]] || skill_logs+=("$review_record")
    skill_status=0
    "$repo_root/dev/improve/skill-reads.sh" "$skills_dir" "${skill_logs[@]}" || skill_status=$?
    if [[ "$skill_status" -ne 0 ]]; then
      printf 'PR %s: the skill check of the round refused its event logs (status %s, named above);' \
        "$pr_selector" "$skill_status" >&2
      printf ' nothing was merged.\n' >&2
      exit 2
    fi
    merge_args=(pr merge --squash "$pr_selector" "${repo_args[@]}")
    if [[ -n "$web_change" ]]; then
      pr_title=$(gh pr view "$pr_selector" "${repo_args[@]}" --json title --jq .title) || {
        printf 'Cannot read the title of PR %s; nothing was merged.\n' "$pr_selector" >&2
        exit 2
      }
      sed -n 's/^View:[[:space:]]*//p' "$tmp_dir/pr-body" > "$tmp_dir/views"
      if [[ ! -s "$tmp_dir/views" ]]; then
        printf 'PR %s changes a web page, so its body must carry a `View: <URL>` line naming the full URL, path and query, that opens the changed state; nothing was merged.\n' \
          "$pr_selector" >&2
        exit 2
      fi
      while IFS= read -r view; do
        if [[ "$view" != /* ]]; then
          printf 'PR %s changes a web page, so each `View: <URL>` line naming the full URL, path and query, that opens the changed state must start with `/`; nothing was merged.\n' \
            "$pr_selector" >&2
          exit 2
        fi
      done < "$tmp_dir/views"
      # The View lines move to the message's end, so the squash commit carries each exactly once, as a trailer
      # block; a duplicate line would capture the live page twice at the release switch.
      { sed '/^View:/d' "$tmp_dir/pr-body"; printf '\n'; sed 's/^/View: /' "$tmp_dir/views"; } > "$tmp_dir/merge-body"
      check_string title "$pr_title"
      check_text body "$tmp_dir/merge-body"
      merge_args+=(--subject "$pr_title" --body-file "$tmp_dir/merge-body")
    fi
    # gh's own merge line goes to stderr, so stdout carries the readback below and nothing else.
    gh "${merge_args[@]}" >&2
    # The readback is GitHub's state of the pull request after the merge. The merge counts only when GitHub shows it
    # merged at the head the verdict reviewed. A failure after the merge was sent exits 4: nothing is retried.
    readback=$(gh pr view "$pr_selector" "${repo_args[@]}" --json number,state,headRefOid,mergeCommit \
      --jq '[.number, .state, .headRefOid, (.mergeCommit.oid // "")] | @tsv') || {
      printf 'PR %s: the merge was sent, but its readback failed; the merge is not confirmed.\n' "$pr_selector" >&2
      exit 4
    }
    IFS=$'\t' read -r merged_number merged_state merged_head merge_commit <<< "$readback"
    if [[ "$merged_state" != MERGED || "$merged_head" != "$merge_head" || -z "$merge_commit" ]]; then
      printf 'PR %s: the merge was sent, but the readback shows state %s at head %s, merge commit %s; ' \
        "$pr_selector" "$merged_state" "$merged_head" "${merge_commit:-none}" >&2
      printf 'the merge is not confirmed at the reviewed head %s.\n' "$merge_head" >&2
      exit 4
    fi
    printf 'PR %s\nReviewed head: %s\nMerge commit: %s\nState: %s\n' \
      "$merged_number" "$merged_head" "$merge_commit" "$merged_state"
    ;;
  push)
    # The pushed text is checked in-process before git push runs, so a clone without the hooks path is covered
    # too: the source ref of each refspec (HEAD when the call names no refspec), the ref and branch names, and
    # every commit the push publishes — its message, identity, diff, decompressed `*.gz` blobs and designated
    # doc blobs. Credentials first, then the host patterns over all of it, then the generic rules over the
    # messages and docs. The pre-push hook still runs inside git push and applies the same stages.
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
    # The first positional is the remote; the rest are refspecs. A lone remote means HEAD, and a refspec's
    # source side is what its commits publish under — `feat`, `feat:main` or `+feat:main`. A `:dst` deletion
    # publishes no content.
    specs=()
    if [[ ${#positional[@]} -ge 2 ]]; then
      specs=("${positional[@]:1}")
    fi
    srcs=()
    names_file="$tmp_dir/push-names"
    : > "$names_file"
    if [[ ${#specs[@]} -eq 0 ]]; then
      srcs=(HEAD)
      git branch --show-current >> "$names_file" || {
        printf 'Cannot read the current branch name for the push check; nothing was pushed.\n' >&2
        exit 2
      }
    fi
    for spec in "${specs[@]}"; do
      spec=${spec#+}
      src=${spec%%:*}
      if [[ "$spec" == *:* ]]; then
        printf '%s\n%s\n' "$src" "${spec#*:}" >> "$names_file"
      else
        printf '%s\n' "$src" >> "$names_file"
      fi
      if [[ "$src" == HEAD ]]; then
        git branch --show-current >> "$names_file" || {
          printf 'Cannot read the current branch name for the push check; nothing was pushed.\n' >&2
          exit 2
        }
      fi
      [[ -n "$src" ]] && srcs+=("$src")
    done

    cred_list="$tmp_dir/cred.list"
    all_list="$tmp_dir/all.list"
    doc_list="$tmp_dir/doc.list"
    printf 'branch name\t%s\n' "$names_file" > "$cred_list"
    printf 'branch name\t%s\n' "$names_file" > "$all_list"
    : > "$doc_list"
    index=0
    for src in "${srcs[@]}"; do
      # The commits a push publishes: origin/main..src when main is an ancestor, or every commit no remote ref
      # reaches for a new branch. A failed listing stops the push.
      if git merge-base --is-ancestor origin/main "$src" 2>/dev/null; then
        commits=$(git rev-list "origin/main..$src") || {
          printf 'Cannot list origin/main..%s for the push check; nothing was pushed.\n' "$src" >&2
          exit 2
        }
      else
        commits=$(git rev-list "$src" --not --remotes) || {
          printf 'Cannot list the commits of %s for the push check; nothing was pushed.\n' "$src" >&2
          exit 2
        }
      fi
      while IFS= read -r sha; do
        [[ -n "$sha" ]] || continue
        collect_commit_texts "$sha" "$tmp_dir" "$index" "$cred_list" "$all_list" "$doc_list"
        index=$((index + 1))
      done <<< "$commits"
    done

    status=0
    "$scanner" --credential-check-list "$credential_values" "$cred_list" > "$tmp_dir/hit" || status=$?
    if [[ "$status" -eq 3 ]]; then
      cat -- "$tmp_dir/hit" >&2
      printf 'nothing was pushed; the round stops here: the kind named above is the reason.\n' >&2
      exit 3
    elif [[ "$status" -ne 0 ]]; then
      cat -- "$tmp_dir/hit" >&2
      exit "$status"
    fi
    "$scanner" --require-local-patterns --host-patterns-only --check-list "$all_list"
    "$scanner" --require-local-patterns --check-list "$doc_list"

    # The pre-push hook applies the same checks inside git push. Its refusal exits git's push with 1, so the
    # hook also writes the credential hit to this file, and the round's exit names the hit with code 3.
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
