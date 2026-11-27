#!/usr/bin/env bash
# The one claim test: whether a round's diff may change a path that a live claim in the in-flight list holds.
#
#   dev/improve/claim-check.sh <head branch> <in-flight list>
#
# The diff is the head's change against origin/main, the round's base. A claim refuses the diff only when its
# claimant's change conflicts with the round's change on that path: the three-way merge of the round's head and the
# claimant branch's tip on origin (git merge-tree --write-tree) conflicts on the path. A clean merge passes, and stderr
# names it as `claim passed:`. Two claims refuse without a merge test:
#   - a path in CLAIM_PATH_LEVEL_FILES, a generated lock file, whose merge does not show what the claimant changed;
#   - a claim the list marks `uncommitted`: its work is in no commit, so no merge can test it.
# A claim also refuses, with its cause named, when the merge cannot be computed: the claimant branch is not on origin,
# or git fails. An unknown result never passes.
#
# Sourced, the file defines check_inflight_claims, which pr-create and pr-merge run in github-post.sh, so every claim
# check shares this one test. Executed, it runs the same check on one head branch and exits 0 when it passes, 2 when
# it refuses.

# The lock files whose claim refuses on the path alone. One list names them: a generated lock file's merge does not
# show what the claimant changed, so a round that changes one is refused while another live claim holds it.
CLAIM_PATH_LEVEL_FILES=(
  dev/alloc-probe/Cargo.lock
  dev/integration-selection/package-lock.json
  rust/Cargo.lock
  web/package-lock.json
)

# claim_path_level_file <path>: return 0 when the path is one of CLAIM_PATH_LEVEL_FILES.
claim_path_level_file() {
  local file
  for file in "${CLAIM_PATH_LEVEL_FILES[@]}"; do
    if [[ $1 == "$file" ]]; then
      return 0
    fi
  done
  return 1
}

# one_line <text>: print the text on one line, so a git error names its cause within one refusal line.
one_line() {
  printf '%s\n' "${1//$'\n'/ }"
}

# claim_tip <branch>: fetch the claimant branch from origin into the ref this check owns, and print its commit. A branch
# that origin cannot provide prints its cause and returns 1. The fetch names the URL's remote, not FETCH_HEAD, and
# writes only refs/improve/claims/, so no other reader's ref moves.
claim_tip() {
  local branch=$1 error tip
  if ! error=$(git fetch --quiet --no-tags --no-write-fetch-head origin \
    "+refs/heads/$branch:refs/improve/claims/$branch" 2>&1); then
    printf 'cannot read the claimant ref %s from origin: %s\n' "$branch" "$(one_line "$error")"
    return 1
  fi
  if ! tip=$(git rev-parse --verify --quiet "refs/improve/claims/$branch^{commit}"); then
    printf 'cannot read the commit of the claimant ref %s after the fetch\n' "$branch"
    return 1
  fi
  printf '%s\n' "$tip"
}

# claim_conflicts <round commit> <claimant commit>: print the paths on which the three-way merge of the two commits
# conflicts, one per line, and return 0. A clean merge prints nothing and returns 0. A merge that git cannot compute
# prints its cause and returns 1. git merge-tree exits 1 both for a conflict and for an error, so exit 1 reads as a
# conflict only when it prints a commit id followed by the conflicted paths.
claim_conflicts() {
  local ours=$1 theirs=$2 listing status
  local -a lines=()
  if listing=$(git merge-tree --write-tree --name-only --no-messages "$ours" "$theirs" 2>&1); then
    return 0
  else
    status=$?
  fi
  mapfile -t lines <<<"$listing"
  if (( status == 1 )) && (( ${#lines[@]} > 1 )) && [[ ${lines[0]} =~ ^[0-9a-f]{40}([0-9a-f]{24})?$ ]]; then
    printf '%s\n' "${lines[@]:1}"
    return 0
  fi
  printf 'git merge-tree exited %s: %s\n' "$status" "$(one_line "$listing")"
  return 1
}

# check_inflight_claims <changed paths file> <head branch> <head rev> <in-flight list>: return 1 after naming on stderr
# each claim that refuses the diff, and return 0 after naming each claim that passes. The head rev is the round's head
# as this clone reads it. The head branch never refuses itself, and a claim the list marks absent-on-origin-main does
# not refuse. An unreadable list or head refuses with its cause named, and so does an unknown merge.
check_inflight_claims() {
  local changed=$1 head=$2 head_rev=$3 list=$4
  local -A changed_paths=() merged=() merge_error=() conflicts=()
  local path claimed branch marker head_commit tip out found=0
  if [[ ! -r $list ]]; then
    printf 'cannot read the in-flight list %s\n' "$list" >&2
    return 1
  fi
  if ! head_commit=$(git rev-parse --verify --quiet "$head_rev^{commit}"); then
    printf 'cannot read the round head %s in this clone\n' "$head_rev" >&2
    return 1
  fi
  while IFS= read -r path; do
    if [[ -n $path ]]; then
      changed_paths["$path"]=1
    fi
  done < "$changed"
  while IFS=$'\t' read -r claimed branch marker; do
    if [[ -z $claimed || -z ${changed_paths["$claimed"]:-} ]]; then
      continue
    fi
    if [[ $branch == "$head" || $marker == absent-on-origin-main ]]; then
      continue
    fi
    if claim_path_level_file "$claimed"; then
      printf 'claimed path %s, claimed by %s: a generated lock file is claimed on its path alone\n' \
        "$claimed" "$branch" >&2
      found=1
      continue
    fi
    if [[ $marker == uncommitted ]]; then
      printf 'claimed path %s, claimed by %s: its uncommitted work cannot be merge-tested\n' "$claimed" "$branch" >&2
      found=1
      continue
    fi
    if [[ -z ${merged["$branch"]:-} ]]; then
      merged["$branch"]=1
      if tip=$(claim_tip "$branch"); then
        if out=$(claim_conflicts "$head_commit" "$tip"); then
          conflicts["$branch"]=$out
        else
          merge_error["$branch"]=$out
        fi
      else
        merge_error["$branch"]=$tip
      fi
    fi
    if [[ -n ${merge_error["$branch"]:-} ]]; then
      printf 'claimed path %s, claimed by %s: cannot compute the merge with its change: %s\n' \
        "$claimed" "$branch" "${merge_error["$branch"]}" >&2
      found=1
    elif grep -qxF -- "$claimed" <<<"${conflicts["$branch"]:-}"; then
      printf 'claimed path %s, claimed by %s: the merge conflicts with its change\n' "$claimed" "$branch" >&2
      found=1
    else
      printf 'claim passed: path %s, claimed by %s: the merge with the round is clean\n' "$claimed" "$branch" >&2
    fi
  done < "$list"
  [[ $found -eq 0 ]]
}

# Executed, the file runs the check on one head branch against origin/main.
if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  set -euo pipefail
  if [[ $# -ne 2 ]]; then
    printf 'usage: %s <head branch> <in-flight list>\n' "${BASH_SOURCE[0]}" >&2
    exit 2
  fi
  tmp_dir=$(mktemp -d)
  trap 'rm -rf -- "$tmp_dir"' EXIT
  if ! git diff --name-only "origin/main...$1" > "$tmp_dir/changed"; then
    printf 'cannot read the changed paths of %s against origin/main\n' "$1" >&2
    exit 2
  fi
  check_inflight_claims "$tmp_dir/changed" "$1" "$1" "$2" || exit 2
fi
