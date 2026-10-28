#!/usr/bin/env bash
# List the files that other open work changes.
#
#   tools/improve/inflight-files.sh <own branch prefix>
#
# Print one repository-relative path per line: the files each open pull request
# changes, and the files each worktree branch of this repository changes since
# its merge base with origin/main, uncommitted changes included. A branch whose
# name starts with the own prefix is skipped, because its work belongs to the
# caller. The script only reads: it writes no branch, worktree or pull request.
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

# Print the committed and uncommitted paths of one worktree branch.
#
# The merge base with origin/main bounds the branch's own work. The uncommitted
# paths come from the worktree directory, which a stale entry may not have.
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
  git -C "$main_root" diff --name-only "$merge_base" "$branch"
  if [[ -d $worktree ]]; then
    git -C "$worktree" diff --name-only HEAD
    git -C "$worktree" ls-files --others --exclude-standard
  fi
}

{
  list_open_pull_requests
  list_worktree_branches
} | LC_ALL=C sort -u
