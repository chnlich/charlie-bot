#!/usr/bin/env bash
# List the files that other live work changes.
#
#   tools/improve/inflight-files.sh <own branch prefix>
#
# Print one repository-relative path per line: the files each open pull request
# changes, and the committed and uncommitted files of each worktree branch that
# is in flight. A branch is in flight when origin/main does not already contain
# its work; a branch whose pull request was squash-merged and closed has landed,
# so it names no committed file. A branch whose name starts with the own prefix
# is skipped, because its work belongs to the caller.
#
# The branch state is read only after origin/main is fetched, because a stale
# origin/main would make merged work look live. The script only reads: it writes
# no branch, worktree or pull request.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  printf 'usage: %s <own branch prefix>\n' "${BASH_SOURCE[0]}" >&2
  exit 2
fi
own_prefix=$1

# Resolve the main checkout from the script's own location, so the script runs
# from any worktree. The main checkout owns the shared worktree list.
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(git -C "$script_dir" rev-parse --show-toplevel)
common_dir=$(git -C "$repo_root" rev-parse --path-format=absolute --git-common-dir)
main_root=$(dirname "$common_dir")

# Fetch origin/main before any branch is compared against it. The explicit
# refspec updates the tracking ref the comparisons read, whatever fetch
# configuration the clone carries.
if ! git -C "$main_root" fetch --quiet --no-tags origin +refs/heads/main:refs/remotes/origin/main; then
  printf 'cannot fetch origin/main in %s\n' "$main_root" >&2
  exit 1
fi

list_open_pull_requests() {
  local number
  local head
  gh pr list --state open --limit 200 --json number,headRefName \
    --jq '.[] | [.number, .headRefName] | @tsv' |
    while IFS=$'\t' read -r number head; do
      if [[ $head == "$own_prefix"* ]]; then
        continue
      fi
      gh pr diff "$number" --name-only
    done
}

list_worktree_branches() {
  local worktree=
  local branch=
  local line
  while IFS= read -r line; do
    case $line in
      worktree\ *)
        worktree=${line#worktree }
        branch=
        ;;
      branch\ *)
        branch=${line#branch refs/heads/}
        ;;
      '')
        if [[ -n $branch ]]; then
          list_worktree_branch "$worktree" "$branch"
        fi
        ;;
    esac
  done < <(git -C "$main_root" worktree list --porcelain)
}

# Whether a branch carries a change that origin/main does not already contain.
#
# A branch is in flight when it changes a path whose content origin/main still
# matches the merge base: no landing put anything there, so the path holds only
# the branch's change. A landing rewrites every path that the landed branch
# changed, so once the branch has landed origin/main differs from the merge base
# at each of them, whether the landing kept the branch's commits or squashed
# them into one. A stale origin/main fails this test, which is why the fetch
# above comes first.
branch_is_in_flight() {
  local root=$1
  local merge_base=$2
  local branch=$3
  local path
  local -A moved=()
  while IFS= read -r path; do
    moved["$path"]=1
  done < <(git -C "$root" diff --name-only --no-renames "$merge_base" origin/main)
  while IFS= read -r path; do
    if [[ -z ${moved[$path]:-} ]]; then
      return 0
    fi
  done < <(git -C "$root" diff --name-only --no-renames "$merge_base" "$branch")
  return 1
}

# Print the committed and uncommitted paths of one worktree branch.
#
# The merge base with origin/main bounds the branch's own work, and the branch
# contributes that work only when it is in flight. The uncommitted paths come
# from the worktree directory, which a stale entry may not have, and they are
# live whatever origin/main holds.
list_worktree_branch() {
  local worktree=$1
  local branch=$2
  local merge_base
  if [[ $branch == "$own_prefix"* ]]; then
    return
  fi
  if ! merge_base=$(git -C "$main_root" merge-base origin/main "$branch"); then
    printf 'skip branch %s: no merge base with origin/main\n' "$branch" >&2
    return
  fi
  if branch_is_in_flight "$main_root" "$merge_base" "$branch"; then
    git -C "$main_root" diff --name-only --no-renames "$merge_base" "$branch"
  fi
  if [[ -d $worktree ]]; then
    git -C "$worktree" diff --name-only HEAD
    git -C "$worktree" ls-files --others --exclude-standard
  fi
}

{
  list_open_pull_requests
  list_worktree_branches
} | LC_ALL=C sort -u
