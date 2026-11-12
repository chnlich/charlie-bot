#!/usr/bin/env bash
# List the files that other live work changes.
#
#   dev/improve/inflight-files.sh <own branch prefix>
#
# Print one repository-relative path per line: the files each open pull request
# changes, and the committed and uncommitted files of each worktree branch. A
# committed path is claimed only while the branch's version of it — its blob, or
# its deletion — is missing from origin/main's history since the merge base, so
# a branch whose commits all landed names no committed file, and a path whose
# branch version origin/main has held is not named. A path whose removal
# origin/main has landed since the merge base is not named either: main's own
# deletion is the landed work at that path, so neither a branch's version of the
# old path nor an uncommitted or untracked file there names it, while a path
# main has deleted and later brought back stays open to work. A branch whose
# name starts with the own prefix is skipped, because its work belongs to
# the caller.
#
# A worktree whose branch's commits all landed keeps its uncommitted and
# untracked paths named only while its task run is live. All three of the
# branch having landed, no process running in the worktree, and the task run
# being closed keep those paths off the list. Each condition is read from its
# owner: the landing from the fetched ref, a process from the process table
# (/proc), and the run's end from the run records of the profile home
# ($CHARLIEBOT_HOME), whose newest record naming the worktree decides because
# a retry records the same worktree path. A condition that cannot be read
# stops the script with an error naming it, and a worktree no run record names
# keeps its paths named, because the rule drops a claim only when all three
# hold.
#
# Sourced, the file only defines its rule functions: the improve runner reads
# the same rule from the copy the launch directory carries. Executed, it prints
# the list.
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

# The blob id that a raw diff line reports for the side a change does not touch:
# an added path's source, and a deleted path's destination.
zero_blob=0000000000000000000000000000000000000000

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
# change. The branch's version of a path is its blob, or its deletion.
#
# A path whose removal origin/main has landed since the merge base is not
# named: main's own deletion is the landed work at that path, so the branch's
# version of the old path carries nothing live. A path main has deleted and
# later brought back is still open to work, so the tree's own answer decides.
# Both sides read the fetched ref, so a stale fetch would make merged work look
# live; this is why the fetch comes first.
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
  local -A landed_removals=()
  while IFS=$'\t' read -r meta path; do
    read -r _ _ _ version _ <<<"$meta"
    main_versions["$path"$'\t'"$version"]=1
    if [[ $version == "$zero_blob" ]]; then
      landed_removals["$path"]=1
    fi
  done < <(git --literal-pathspecs -C "$root" log --format= --raw --no-renames --no-abbrev \
    "$merge_base..$main_ref" -- "${paths[@]}" | grep '^:')
  # A path origin/main's tree still holds is not removed, whatever its history
  # shows: a deletion that a later re-add undid leaves the path live.
  while IFS= read -r path; do
    unset "landed_removals[$path]"
  done < <(git --literal-pathspecs -C "$root" ls-tree --name-only "$main_ref" -- "${paths[@]}")
  for entry in "${branch_versions[@]}"; do
    path=${entry%%$'\t'*}
    if [[ -z ${main_versions[$entry]:-} && -z ${landed_removals[$path]:-} ]]; then
      printf '%s\n' "$path"
    fi
  done
}

# Print the paths of one checkout's uncommitted and untracked state: the tracked change of the
# tree and index, and every untracked path. A checkout whose HEAD has no commits yet still
# lists its untracked paths; its tracked read prints nothing.
list_dirty_paths() {
  local worktree=$1
  git -C "$worktree" diff --name-only HEAD
  git -C "$worktree" ls-files --others --exclude-standard
}

# Print the paths of one checkout's uncommitted and untracked state that live
# work changes: each path whose removal origin/main has landed since the
# checkout's own merge base with main_ref stays off, because main's own deletion
# is the landed work at that path. A checkout whose HEAD has no merge base with
# main_ref is skipped with a note, as its branch is.
list_worktree_directory_paths() {
  local root=$1
  local main_ref=$2
  local worktree=$3
  local meta path version merge_base
  if ! merge_base=$(git -C "$worktree" merge-base "$main_ref" HEAD); then
    printf 'skip worktree %s: no merge base with origin/main\n' "$worktree" >&2
    return 0
  fi
  local -a paths=()
  local -A landed_removals=()
  while IFS= read -r path; do
    paths+=("$path")
  done < <(list_dirty_paths "$worktree")
  if [[ ${#paths[@]} -eq 0 ]]; then
    return 0
  fi
  while IFS=$'\t' read -r meta path; do
    read -r _ _ _ version _ <<<"$meta"
    if [[ $version == "$zero_blob" ]]; then
      landed_removals["$path"]=1
    fi
  done < <(git --literal-pathspecs -C "$root" log --format= --raw --no-renames --no-abbrev \
    "$merge_base..$main_ref" -- "${paths[@]}" | grep '^:')
  # A path origin/main's tree still holds is not removed, whatever its history
  # shows: a deletion that a later re-add undid leaves the path live.
  while IFS= read -r path; do
    unset "landed_removals[$path]"
  done < <(git --literal-pathspecs -C "$root" ls-tree --name-only "$main_ref" -- "${paths[@]}")
  for path in "${paths[@]}"; do
    if [[ -z ${landed_removals[$path]:-} ]]; then
      printf '%s\n' "$path"
    fi
  done
}

# Print whether a process runs in one worktree: `yes` when a process's working directory is the
# worktree or a path inside it, `no` when none does, `unreadable` when the process table cannot
# be read. The process table owns the process condition. A process of another user hides its
# working directory from this read, so such a process is not seen: the rounds and task runs that
# hold these worktrees run as the same user, and an unreadable entry is skipped on that account.
worktree_process_state() {
  local worktree=$1
  local proc target
  if [[ ! -d /proc ]]; then
    printf 'unreadable\n'
    return 0
  fi
  for proc in /proc/[0-9]*/cwd; do
    [[ -e $proc ]] || continue
    target=$(readlink "$proc") || continue
    if [[ $target == "$worktree" || $target == "$worktree"/* ]]; then
      printf 'yes\n'
      return 0
    fi
  done
  printf 'no\n'
}

# One field of a run record's metadata file. The store writes the record as JSON, pretty or on
# one line, so one line read settles the field; a record without the field prints nothing.
run_record_field() {
  local record=$1
  local field=$2
  sed -n "s/.*\"$field\": *\"\\([^\"]*\\)\".*/\\1/p" "$record"
}

# Read the run records that name one of the candidate worktrees, and record per worktree whether
# its task run is closed: `run_closed_state[worktree]` is 1 when it is. The run record owns the
# run's lifecycle. The store writes one metadata file per run under the profile home's sessions,
# and every run that worked in a worktree records its path as its worktree_path; a retry records
# the same path, so several records can name one worktree, and the newest record decides: the
# run is closed when that record carries an ended_at. A worktree no record names gets no entry,
# and its claim stands, because the rule drops a claim only when all three conditions hold.
#
# One pass answers every candidate: the grep names the records whose text names a candidate, and
# the per-record reads settle the three fields. A missing profile home or sessions directory
# leaves the condition unreadable, and the script stops with an error naming it.
declare -A run_closed_state=()
declare -A run_newest_started=()

read_run_records() {
  local candidate record worktree started ended unreadable
  local -a patterns=() records=()
  if (( $# == 0 )); then
    return 0
  fi
  if [[ -z ${CHARLIEBOT_HOME:-} ]]; then
    printf 'cannot read the run-record condition: CHARLIEBOT_HOME is unset\n' >&2
    exit 1
  fi
  if [[ ! -d $CHARLIEBOT_HOME/sessions ]]; then
    printf 'cannot read the run-record condition: %s/sessions is missing\n' "$CHARLIEBOT_HOME" >&2
    exit 1
  fi
  # grep skips a record it cannot open without a status the pipeline reports, and the newest record may
  # be that one, so an unreadable record or an unlistable directory stops the read here.
  unreadable=$(find "$CHARLIEBOT_HOME/sessions" -mindepth 5 -maxdepth 5 -type f -name metadata.json ! -readable \
    -print -quit) || {
    printf 'cannot read the run-record condition: %s/sessions cannot be listed\n' "$CHARLIEBOT_HOME" >&2
    exit 1
  }
  if [[ -n $unreadable ]]; then
    printf 'cannot read the run-record condition: %s is unreadable\n' "$unreadable" >&2
    exit 1
  fi
  for candidate in "$@"; do
    patterns+=(-e "$candidate")
  done
  while IFS= read -r record; do
    records+=("$record")
  done < <(find "$CHARLIEBOT_HOME/sessions" -mindepth 5 -maxdepth 5 -type f -name metadata.json -print0 |
    xargs -0 -r grep -lF "${patterns[@]}")
  for record in "${records[@]}"; do
    worktree=$(run_record_field "$record" worktree_path) || {
      printf 'cannot read the run-record condition: %s is unreadable\n' "$record" >&2
      exit 1
    }
    [[ -n $worktree ]] || continue
    started=$(run_record_field "$record" started_at) || {
      printf 'cannot read the run-record condition: %s is unreadable\n' "$record" >&2
      exit 1
    }
    ended=$(run_record_field "$record" ended_at) || {
      printf 'cannot read the run-record condition: %s is unreadable\n' "$record" >&2
      exit 1
    }
    for candidate in "$@"; do
      [[ $candidate == "$worktree" ]] || continue
      if [[ -z ${run_newest_started[$candidate]:-} || $started > ${run_newest_started[$candidate]} ]]; then
        run_newest_started[$candidate]=$started
        if [[ -n $ended ]]; then
          run_closed_state[$candidate]=1
        else
          unset "run_closed_state[$candidate]"
        fi
      fi
    done
  done
}

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

# Print the committed and uncommitted paths of every worktree branch.
#
# The committed side keeps its rule per branch: a branch whose commits all
# landed names no committed path, and an unlanded branch names each path whose
# branch version is missing from origin/main's history, with a path whose
# removal origin/main has landed left off. The worktree directory's
# uncommitted and untracked paths follow the same landed-removal rule, and a
# worktree whose branch landed, whose directory no process runs in, and whose
# task run is closed names none of them; a stale entry may not have the
# directory at all.
#
# The run records are read once for every worktree that passes the other two
# conditions and holds an uncommitted change, so the store answers one pass
# per list instead of one pass per worktree.
list_worktree_branches() {
  local -a worktrees=() branches=() merge_bases=() landed=() candidates=()
  local worktree= branch= line index merge_base worktree_path
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
          worktrees+=("$worktree")
          branches+=("$branch")
        fi
        ;;
    esac
  done < <(git -C "$main_root" worktree list --porcelain)

  for index in "${!worktrees[@]}"; do
    worktree=${worktrees[index]}
    branch=${branches[index]}
    if [[ $branch == "$own_prefix"* ]]; then
      landed[index]=own
      continue
    fi
    if ! merge_base=$(git -C "$main_root" merge-base "$main_ref" "$branch"); then
      printf 'skip branch %s: no merge base with origin/main\n' "$branch" >&2
      landed[index]=skip
      continue
    fi
    merge_bases[index]=$merge_base
    if commits_landed "$main_root" "$main_ref" "$merge_base" "$branch"; then
      landed[index]=yes
    else
      landed[index]=no
    fi
  done

  # The process condition comes before the run-record read, because a worktree
  # with a process in it keeps its claim without reading the store.
  for index in "${!worktrees[@]}"; do
    if [[ ${landed[index]} != yes ]]; then
      continue
    fi
    worktree=${worktrees[index]}
    [[ -d $worktree ]] || continue
    local -a dirty=()
    while IFS= read -r worktree_path; do
      dirty+=("$worktree_path")
    done < <(list_dirty_paths "$worktree")
    if (( ${#dirty[@]} == 0 )); then
      continue
    fi
    case $(worktree_process_state "$worktree") in
      no) candidates+=("$worktree") ;;
      yes) : ;;
      *)
        printf 'cannot read the process condition for worktree %s: /proc is missing\n' "$worktree" >&2
        exit 1
        ;;
    esac
  done
  read_run_records "${candidates[@]}"

  for index in "${!worktrees[@]}"; do
    worktree=${worktrees[index]}
    branch=${branches[index]}
    case ${landed[index]} in
      own | skip)
        continue
        ;;
      no)
        list_unlanded_paths "$main_root" "$main_ref" "${merge_bases[index]}" "$branch"
        ;;
    esac
    if [[ -d $worktree && ${run_closed_state[$worktree]:-0} != 1 ]]; then
      list_worktree_directory_paths "$main_root" "$main_ref" "$worktree"
    fi
  done
}

# Executed, the script prints the list; sourced, the block above already defined
# the rule functions and nothing here runs.
if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
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
  # refspec writes the ref this prefix owns, whatever fetch configuration the
  # clone carries, and --no-write-fetch-head leaves the shared FETCH_HEAD alone.
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

  {
    list_open_pull_requests
    list_worktree_branches
  } | LC_ALL=C sort -u
fi
