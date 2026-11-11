#!/usr/bin/env bash
# List the files that other live work changes.
#
#   dev/improve/inflight-files.sh <own branch prefix>
#
# Print one repository-relative path per line: the files each open pull request
# changes, and the committed and uncommitted files of each worktree branch. A
# committed path is claimed only while the branch's version of it — its blob, or
# its deletion — is missing from origin/main's history since the merge base, so
# a branch whose commits all landed, and a branch whose version of a path
# origin/main has held, name no committed file. A branch whose name starts with
# the own prefix is skipped, because its work belongs to the caller.
#
# The branch state is read only after origin/main is fetched, because a stale
# origin/main would make merged work look live. The launches that read this list
# start in a burst beside the other launches, whose runners and workers fetch
# this checkout's refs/remotes/origin/main too: fetching into that shared ref, or
# into FETCH_HEAD, races their ref locks. This script therefore fetches into the
# ref its own prefix owns and compares every branch against that ref, and the
# clones' refs/remotes/origin/main stay with their owners. Otherwise the script
# only reads: it writes no branch, worktree or pull request.
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
# refspec writes the ref this prefix owns, whatever fetch configuration the clone
# carries, and --no-write-fetch-head leaves the shared FETCH_HEAD alone.
main_ref="refs/improve/inflight/${own_prefix//[^A-Za-z0-9._-]/_}"
origin_url=$(git -C "$main_root" remote get-url origin) || {
  printf 'cannot read the origin of %s\n' "$main_root" >&2
  exit 1
}
# The fetch names the URL instead of the remote, because a named remote's fetch
# also updates the remote's tracking ref, and that update is the shared write
# this fetch must not make.
if ! git -C "$main_root" fetch --quiet --no-tags --no-write-fetch-head "$origin_url" \
  +refs/heads/main:"$main_ref"; then
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

# Whether every commit of the branch is on origin/main, by patch or by subject.
#
# A rebase lands each commit unchanged, and git cherry finds it by patch. A
# rebase that resolved a conflict changes the patch but keeps the subject, so a
# commit whose subject origin/main has since the merge base also counts.
commits_landed() {
  local root=$1
  local main_ref=$2
  local merge_base=$3
  local branch=$4
  local sign sha subject
  local -A main_subjects=()
  while IFS= read -r subject; do
    main_subjects["$subject"]=1
  done < <(git -C "$root" log --format=%s "$merge_base..$main_ref")
  while read -r sign sha subject; do
    if [[ $sign == + && -z ${main_subjects[$subject]:-} ]]; then
      return 1
    fi
  done < <(git -C "$root" cherry -v "$main_ref" "$branch")
  return 0
}

# Print the committed paths of one worktree branch whose version origin/main's
# history does not hold, since the merge base.
#
# A squash merge lands the branch's content in one commit. A later change may
# move a path on, but the landed version stays in origin/main's history, so a
# path whose branch version never appears there carries the branch's own
# change. The branch's version of a path is its blob, or its deletion. Both
# sides read the fetched ref, so a stale fetch would make merged work look
# live; this is why the fetch above comes first.
list_unlanded_paths() {
  local root=$1
  local main_ref=$2
  local merge_base=$3
  local branch=$4
  local meta path version entry
  local -A main_versions=()
  local -a paths=()
  local -a branch_versions=()
  while IFS=$'\t' read -r meta path; do
    read -r _ _ _ version _ <<<"$meta"
    paths+=("$path")
    branch_versions+=("$path"$'\t'"$version")
  done < <(git -C "$root" diff --raw --no-renames --no-abbrev "$merge_base" "$branch")
  if [[ ${#paths[@]} -eq 0 ]]; then
    return 0
  fi
  while IFS=$'\t' read -r meta path; do
    read -r _ _ _ version _ <<<"$meta"
    main_versions["$path"$'\t'"$version"]=1
  done < <(git --literal-pathspecs -C "$root" log --format= --raw --no-renames --no-abbrev \
    "$merge_base..$main_ref" -- "${paths[@]}" | grep '^:')
  for entry in "${branch_versions[@]}"; do
    if [[ -z ${main_versions[$entry]:-} ]]; then
      printf '%s\n' "${entry%%$'\t'*}"
    fi
  done
}

# Print the committed and uncommitted paths of one worktree branch.
#
# The merge base with origin/main bounds the branch's own work. A branch whose
# commits all landed claims no committed path; otherwise each committed path is
# claimed only while its branch version is missing from origin/main's history.
# The uncommitted paths come from the worktree directory, which a stale entry
# may not have, and they are live whatever origin/main holds.
list_worktree_branch() {
  local worktree=$1
  local branch=$2
  local merge_base
  if [[ $branch == "$own_prefix"* ]]; then
    return
  fi
  if ! merge_base=$(git -C "$main_root" merge-base "$main_ref" "$branch"); then
    printf 'skip branch %s: no merge base with origin/main\n' "$branch" >&2
    return
  fi
  if ! commits_landed "$main_root" "$main_ref" "$merge_base" "$branch"; then
    list_unlanded_paths "$main_root" "$main_ref" "$merge_base" "$branch"
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
