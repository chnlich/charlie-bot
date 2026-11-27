#!/usr/bin/env bash
# List the files that other live work claims.
#
#   dev/improve/inflight-files.sh <own branch prefix>
#
# Print one `path<TAB>branch` line per claimed path: open pull request files, and the
# committed and uncommitted paths of each worktree branch with a live claimant. A claimed
# path that origin/main has no object at carries a third field, `absent-on-origin-main`.
# Work that origin does not carry, on a path origin/main has, carries the third field `local-only`: uncommitted work,
# and every committed path of a branch whose origin copy is not at the commit this checkout holds. No merge on origin
# can test that work, so the claim check refuses it (dev/improve/claim-check.sh).
# The branch is the one that carries the path: uncommitted work prints with the branch of
# its dirty worktree, a path that an unmerged commit changes prints with that commit's
# branch, and an open pull request's files print with its head branch. A path whose
# only branch commits are patch-equivalent on main prints nothing, so a squash-landed
# path, or a rebased one whose patch main carries, is not listed as live work. An open pull request
# for the branch, a process in its worktree, or an open owning task keeps those
# paths claimed. A worktree no task names claims its paths only while its branch
# has a commit ahead of origin/main, or one of its changed, uncommitted or
# untracked paths changed within NO_RECORD_CLAIM_DAYS. Otherwise it claims no
# path, and stderr names it with its newest change time. A task owns the worktree
# when a record of the task store names that path, a run record under
# `data/runs/` or a task record under `threads/`; the task store's event history,
# not a run's end, decides whether that task is open. With a closed task, no
# process, and no open pull request, the worktree claims no path, whether or not
# its branch landed. A condition that cannot be read stops the script with an
# error naming the condition.
#
# A branch whose name starts with the own prefix is skipped, because its work
# belongs to the caller.
#
# Sourced, the file defines the same claimant and path functions the improve
# runner uses for its clone scan. Executed, it prints the list.
#
# The branch state is read only after origin/main is fetched, because a stale
# origin/main would make merge bases stale. The launches that read this list
# start in a burst beside the other launches, whose runners and workers fetch
# this checkout's refs/remotes/origin/main too: fetching into that shared ref,
# or into FETCH_HEAD, races their ref locks. This script therefore fetches into
# the ref its own prefix owns and compares every branch against that ref, and
# the clones' refs/remotes/origin/main stay with their owners. Otherwise the
# script only reads: it writes no branch, worktree or pull request.
set -euo pipefail

# The window, in days, in which a worktree that no task record names keeps its claim through a changed path.
readonly NO_RECORD_CLAIM_DAYS=7

# The patch ids of main's commits, shared by every branch this run classifies. Reading them once
# per run keeps one pass over main's history instead of one per branch.
declare -A main_patch_ids=()

# Read the patch ids of main's commits since base into the shared map. `git patch-id` prints
# `<id> <commit>` per patch; the ids alone decide equivalence.
read_main_patch_ids() {
  local root=$1
  local base=$2
  local ref=$3
  local ids id commit
  if ! ids=$(git -C "$root" log --no-merges -p --format=%H "$base..$ref" | git patch-id --stable); then
    printf 'cannot read the patch ids of %s in %s\n' "$ref" "$root" >&2
    exit 1
  fi
  main_patch_ids=()
  while read -r id commit; do
    [[ -n $id ]] || continue
    main_patch_ids["$id"]=1
  done <<<"$ids"
}

# The third field of a claim whose path origin/main has no object at. A claim without it names a path origin/main has.
readonly ABSENT_ON_MAIN_FIELD=absent-on-origin-main

# The third field of a claim whose work origin does not carry: uncommitted work, or a commit that the branch's origin
# copy lacks. No merge on origin holds that work, so the claim check cannot merge-test it (dev/improve/claim-check.sh
# reads this field).
readonly LOCAL_ONLY_FIELD=local-only

# Print one claim as `path<TAB>branch`, with the absent field when origin/main has no object at the path, or the
# local-only field when the claim is work origin does not carry (local_only=yes) on a path origin/main has. The lookup
# runs in the repository at root, any checkout of it, because main_ref lives in the refs that all checkouts share. An
# unreadable repository stops the script. A missing path is no error: git answers it with the object name and `missing`.
print_claim() {
  local root=$1
  local path=$2
  local branch=$3
  local local_only=${4:-no}
  local answer
  if ! answer=$(git -C "$root" cat-file --batch-check='%(objecttype)' <<<"$main_ref:$path"); then
    printf 'cannot read %s:%s in %s\n' "$main_ref" "$path" "$root" >&2
    exit 1
  fi
  if [[ $answer == "$main_ref:$path missing" ]]; then
    printf '%s\t%s\t%s\n' "$path" "$branch" "$ABSENT_ON_MAIN_FIELD"
  elif [[ $local_only == yes ]]; then
    printf '%s\t%s\t%s\n' "$path" "$branch" "$LOCAL_ONLY_FIELD"
  else
    printf '%s\t%s\n' "$path" "$branch"
  fi
}

# branch_local_only <root> <branch>: set branch_is_local_only to yes when origin does not carry the branch at the commit
# that this checkout holds for it, and to no when it does. A commit that origin lacks has no merge on origin to test. An
# unreadable branch or origin stops the script. The result is a variable, not printed output, so the exit stops the
# script rather than a subshell.
branch_is_local_only=no
branch_local_only() {
  local root=$1
  local branch=$2
  local tip listing sha ref remote_tip=
  if ! tip=$(git -C "$root" rev-parse --verify --quiet "refs/heads/$branch^{commit}"); then
    printf 'cannot read the commit of branch %s in %s\n' "$branch" "$root" >&2
    exit 1
  fi
  if ! listing=$(git -C "$root" ls-remote --heads origin "refs/heads/$branch"); then
    printf 'cannot read branch %s on origin from %s\n' "$branch" "$root" >&2
    exit 1
  fi
  while IFS=$'\t' read -r sha ref; do
    if [[ $ref == "refs/heads/$branch" ]]; then
      remote_tip=$sha
    fi
  done <<<"$listing"
  if [[ $remote_tip == "$tip" ]]; then
    branch_is_local_only=no
  else
    branch_is_local_only=yes
  fi
}

# Print every path changed by one branch since its merge base, as `path<TAB>branch`. A commit whose
# patch id the shared main map holds is patch-equivalent on main; a commit whose id the map does
# not hold is an unmerged commit, and every path it touches prints with the branch. A path that only
# patch-equivalent commits touch prints nothing. A changed path that no listed commit touches, such as
# a merge's own resolution, prints with the branch, because no commit on main explains it.
# read_main_patch_ids must have run for a base at or before this branch's merge base.
list_committed_paths() {
  local root=$1
  local merge_base=$2
  local branch=$3
  local line sha path id patch_ids log changed_paths
  local -A carried=() landed=() commit_patch_id=()

  branch_local_only "$root" "$branch"
  if ! changed_paths=$(git -C "$root" diff --name-only "$merge_base" "$branch"); then
    printf 'cannot read changed paths of %s in %s\n' "$branch" "$root" >&2
    exit 1
  fi

  if ! patch_ids=$(git -C "$root" log --no-merges -p --format=%H "$merge_base..$branch" | git patch-id --stable); then
    printf 'cannot read the patch ids of %s in %s\n' "$branch" "$root" >&2
    exit 1
  fi
  while read -r id sha; do
    [[ -n $id && -n $sha ]] || continue
    commit_patch_id["$sha"]="$id"
  done <<<"$patch_ids"

  log=$(git -C "$root" log --format=$'\001%H' --name-only "$merge_base..$branch")
  sha=
  while IFS= read -r line; do
    if [[ $line == $'\001'* ]]; then
      sha=${line:1}
    elif [[ -n $line && -n $sha ]]; then
      if [[ -z ${main_patch_ids[${commit_patch_id["$sha"]:-}]:-} ]]; then
        carried["$line"]=1
      else
        landed["$line"]=1
      fi
    fi
  done <<<"$log"

  while IFS= read -r path; do
    [[ -n $path ]] || continue
    if [[ -n ${carried["$path"]:-} || -z ${landed["$path"]:-} ]]; then
      print_claim "$root" "$path" "$branch" "$branch_is_local_only"
    fi
  done <<<"$changed_paths"
}

# Print the tracked changes from HEAD and every untracked path of one checkout, one path per line. A checkout
# whose paths cannot be read stops the script, because a claim it cannot state would be guessed.
dirty_paths() {
  local worktree=$1
  local listing
  if ! listing=$(git -C "$worktree" diff --name-only HEAD \
    && git -C "$worktree" ls-files --others --exclude-standard); then
    printf 'cannot read the changed paths of worktree %s\n' "$worktree" >&2
    exit 1
  fi
  printf '%s\n' "$listing"
}

# Print each dirty path with the branch that carries the checkout's dirty work. Uncommitted work is on no
# branch's main comparison, so it prints with that branch, never as landed work.
list_dirty_paths() {
  local worktree=$1
  local branch=$2
  local path listing
  listing=$(dirty_paths "$worktree") || exit 1
  while IFS= read -r path; do
    [[ -n $path ]] || continue
    print_claim "$worktree" "$path" "$branch" yes
  done <<<"$listing"
}

# Print the newest change time, in epoch seconds, of one checkout's changed, uncommitted and untracked paths, or
# `none` when it has none. A deleted path has no file to time, so its deletion takes the time of the nearest
# directory that still exists, because removing an entry changes that directory.
newest_change_time() {
  local worktree=$1
  local listing path target mtime newest=none
  listing=$(dirty_paths "$worktree") || exit 1
  while IFS= read -r path; do
    [[ -n $path ]] || continue
    target=$worktree/$path
    while [[ ! -e $target && ! -L $target ]]; do
      target=${target%/*}
    done
    if ! mtime=$(stat -c %Y -- "$target"); then
      printf 'cannot read the change time of %s\n' "$target" >&2
      exit 1
    fi
    if [[ $newest == none ]] || (( mtime > newest )); then
      newest=$mtime
    fi
  done <<<"$listing"
  printf '%s\n' "$newest"
}

# Return success when a worktree that no task record names keeps its claim: its branch has a commit ahead of
# origin/main, or one of its changed, uncommitted or untracked paths changed within NO_RECORD_CLAIM_DAYS. Otherwise
# return failure and name the worktree on stderr with its newest change time. The notice changes no exit status.
no_record_worktree_is_live() {
  local worktree=$1
  local branch=$2
  local ahead newest=none now when=none

  if ! ahead=$(git -C "$main_root" rev-list --count "$main_ref..$branch"); then
    printf 'cannot count the commits of %s ahead of origin/main in %s\n' "$branch" "$main_root" >&2
    exit 1
  fi
  if (( ahead > 0 )); then
    return 0
  fi

  if [[ -d $worktree ]]; then
    newest=$(newest_change_time "$worktree") || exit 1
  fi
  if [[ $newest != none ]]; then
    now=$(date +%s)
    if (( newest >= now - NO_RECORD_CLAIM_DAYS * 86400 )); then
      return 0
    fi
    when=$(date -u -d "@$newest" +%Y-%m-%dT%H:%M:%SZ)
  fi
  printf 'inflight-files: dropped worktree %s (branch %s): no task record names it, no commit is ahead of origin/main, newest change: %s\n' \
    "$worktree" "$branch" "$when" >&2
  return 1
}

# Print whether a process runs in one worktree: yes when its working directory is the worktree or a path inside it,
# no when none does, unreadable when /proc is missing. A process of another user may hide its working directory.
worktree_process_state() {
  local worktree=$1
  local target targets
  if [[ ! -d /proc ]]; then
    printf 'unreadable\n'
    return 0
  fi
  targets=$(readlink /proc/[0-9]*/cwd 2>/dev/null || true)
  while IFS= read -r target; do
    if [[ $target == "$worktree" || $target == "$worktree"/* ]]; then
      printf 'yes\n'
      return 0
    fi
  done <<<"$targets"
  printf 'no\n'
}

# Read the task store's newest lifecycle event: task_closed closes it, and task_reopened or clone_start opens it.
# No lifecycle event means open, as in Store::task_state_fast.
task_store_state() {
  local session_home=$1
  local task_id=$2
  local session_dir="$session_home/sessions/$task_id"
  local state

  if ! state=$(perl -MJSON::PP -e '
    my ($session_dir, $task_id) = @ARGV;
    my $metadata_path = "$session_dir/metadata.json";
    open my $metadata_file, "<", $metadata_path or die "$metadata_path: $!\n";
    my $metadata_text = do { local $/; <$metadata_file> };
    my $metadata = eval { decode_json($metadata_text) };
    die "$metadata_path is invalid JSON: $@" if $@;
    die "$metadata_path is not a JSON object\n" unless ref($metadata) eq "HASH";
    die "$metadata_path names a different task\n" unless ($metadata->{id} // "") eq $task_id;

    my @logs;
    my $archive_dir = "$session_dir/data/archives";
    if (-e $archive_dir || -l $archive_dir) {
      opendir my $archives, $archive_dir or die "$archive_dir: $!\n";
      my @names = sort grep { /\Achat_events\..*\.jsonl\z/ } readdir $archives;
      closedir $archives or die "$archive_dir: $!\n";
      push @logs, map { "$archive_dir/$_" } @names;
    }
    my $live_log = "$session_dir/data/chat_events.jsonl";
    if (-e $live_log || -l $live_log) {
      die "$live_log is not a file\n" unless -f $live_log;
      push @logs, $live_log;
    }

    my $state = "open";
    for my $path (@logs) {
      open my $log, "<", $path or die "$path: $!\n";
      while (my $line = <$log>) {
        my $event = eval { decode_json($line) };
        die "$path has invalid event JSON: $@" if $@;
        die "$path has an event that is not a JSON object\n" unless ref($event) eq "HASH";
        my $type = $event->{type} // "";
        if ($type eq "task_closed") {
          $state = "closed";
        } elsif ($type eq "task_reopened" || $type eq "clone_start") {
          $state = "open";
        }
      }
      close $log or die "$path: $!\n";
    }
    print "$state\n";
  ' -- "$session_dir" "$task_id" 2>&1); then
    printf 'cannot read the task-state condition for task %s: %s\n' "$task_id" "$state" >&2
    exit 1
  fi
  printf '%s' "$state"
}

declare -A task_state_by_worktree=()
declare -A open_pr_branches=()

# Register a checkout whose owner the task store cannot name, together with the state its claims follow in the
# store's two states. The improve runner's clone scan resolves the state outside the store, from the scanned
# direction's own round: open while that round runs, closed when it does not, and the claimant rule's process
# check then decides.
mark_worktree_state() {
  task_state_by_worktree["$1"]=$2
}

# Resolve each candidate worktree to the task sessions whose records name it, then read each task's current state
# from its event history. The records supply ownership only; their timestamps do not decide whether work is live.
read_worktree_task_states() {
  local session_home=$1
  shift
  local sessions candidate record_list record_listing unreadable owner_listing worktree task_id task_state
  local -A candidate_paths=()
  local -A owner_seen=()
  local -A task_ids_by_worktree=()
  local -a candidates=()

  if (( $# == 0 )); then
    return 0
  fi
  if [[ -z $session_home ]]; then
    printf 'cannot read the task-owner condition: CHARLIEBOT_HOME is unset\n' >&2
    exit 1
  fi
  sessions="$session_home/sessions"
  if [[ ! -d $sessions ]]; then
    printf 'cannot read the task-owner condition: %s is missing\n' "$sessions" >&2
    exit 1
  fi
  for candidate in "$@"; do
    candidates+=("$candidate")
    candidate_paths["$candidate"]=1
  done

  # The store writes the two records that own a worktree under `data/runs/` and `threads/`; both carry the path as
  # worktree_path. Other session files name no worktree and stay outside the read, so a file beside the records
  # that cannot be read does not stop the list.
  local record_paths=('*/data/runs/*/metadata.json' '*/threads/*/metadata.json')
  unreadable=$(find "$sessions" -mindepth 4 -maxdepth 5 -type f \
    \( -path "${record_paths[0]}" -o -path "${record_paths[1]}" \) ! -readable -print -quit) || {
    printf 'cannot read the task-owner condition: cannot list %s\n' "$sessions" >&2
    exit 1
  }
  if [[ -n $unreadable ]]; then
    printf 'cannot read the task-owner condition: %s is unreadable\n' "$unreadable" >&2
    exit 1
  fi
  record_list=$(find "$sessions" -mindepth 4 -maxdepth 5 -type f \
    \( -path "${record_paths[0]}" -o -path "${record_paths[1]}" \) -print) || {
    printf 'cannot read the task-owner condition: cannot list %s\n' "$sessions" >&2
    exit 1
  }
  if [[ -n $record_list ]]; then
    if ! owner_listing=$(printf '%s\n' "$record_list" | perl -MJSON::PP -e '
      my ($sessions, @candidates) = @ARGV;
      my %wanted = map { $_ => 1 } @candidates;
      while (<STDIN>) {
        chomp;
        my $path = $_;
        open my $file, "<", $path or die "$path: $!";
        local $/;
        my $record = decode_json(<$file>);
        die "$path is not a JSON object\n" unless ref($record) eq "HASH";
        my $worktree = $record->{worktree_path};
        next unless defined $worktree;
        die "$path field worktree_path is not a string\n" if ref($worktree);
        next unless $wanted{$worktree};
        my $relative = substr($path, length($sessions) + 1);
        my ($task_id) = split m{/}, $relative;
        die "$path is outside $sessions\n" unless defined $task_id && length $task_id;
        print "$worktree\t$task_id\n";
      }
    ' -- "$sessions" "${candidates[@]}"); then
      printf 'cannot read the task-owner condition: a run or task record is unreadable or invalid\n' >&2
      exit 1
    fi
    while IFS=$'\t' read -r worktree task_id; do
      [[ -n $worktree && -n $task_id ]] || continue
      [[ -n "${candidate_paths[$worktree]:-}" ]] || continue
      local owner_key="$worktree"$'\t'"$task_id"
      [[ -z "${owner_seen["$owner_key"]:-}" ]] || continue
      owner_seen["$owner_key"]=1
      task_ids_by_worktree["$worktree"]="${task_ids_by_worktree["$worktree"]:-}$task_id"$'\n'
    done <<<"$owner_listing"
  fi

  for candidate in "${candidates[@]}"; do
    if [[ -z "${task_ids_by_worktree["$candidate"]:-}" ]]; then
      task_state_by_worktree["$candidate"]=no_task
      continue
    fi
    task_state=closed
    while IFS= read -r task_id; do
      [[ -n $task_id ]] || continue
      local owner_state
      owner_state=$(task_store_state "$session_home" "$task_id") || exit 1
      if [[ $owner_state == open ]]; then
        task_state=open
      elif [[ $owner_state != closed ]]; then
        printf 'cannot read the task-state condition for task %s: unknown state %s\n' "$task_id" "$owner_state" >&2
        exit 1
      fi
    done <<<"${task_ids_by_worktree["$candidate"]}"
    task_state_by_worktree["$candidate"]=$task_state
  done
}

# Return success when an open pull request, an open task, a process, or recent work in a worktree that no task names
# keeps this worktree live.
worktree_has_live_claimant() {
  local worktree=$1
  local branch=$2
  local task_state process_state

  if [[ -z "${task_state_by_worktree["$worktree"]:-}" ]]; then
    printf 'cannot read the task-owner condition for worktree %s: state was not resolved\n' "$worktree" >&2
    exit 1
  fi
  task_state=${task_state_by_worktree["$worktree"]}
  if [[ -n "${open_pr_branches[$branch]:-}" ]]; then
    return 0
  fi
  case $task_state in
    open) return 0 ;;
    no_task)
      if no_record_worktree_is_live "$worktree" "$branch"; then
        return 0
      fi
      return 1
      ;;
    closed)
      ;;
    *)
      printf 'cannot read the task-state condition for worktree %s: unknown state %s\n' \
        "$worktree" "$task_state" >&2
      exit 1
      ;;
  esac

  process_state=$(worktree_process_state "$worktree")
  case $process_state in
    yes) return 0 ;;
    no) return 1 ;;
    unreadable)
      printf 'cannot read the process condition for worktree %s: /proc is missing\n' "$worktree" >&2
      exit 1
      ;;
    *)
      printf 'cannot read the process condition for worktree %s: unknown state %s\n' \
        "$worktree" "$process_state" >&2
      exit 1
      ;;
  esac
}

# Parse a Git remote URL into the hostname, owner and repository that the GitHub CLI accepts.
parse_github_origin() {
  local origin=$1 authority path
  case $origin in
    https://*/* | http://*/* | ssh://*/* | git://*/*)
      authority=${origin#*://}
      [[ $authority == */* ]] || return 1
      gh_hostname=${authority%%/*}
      gh_hostname=${gh_hostname##*@}
      path=${authority#*/}
      ;;
    *@*:*)
      gh_hostname=${origin%%:*}
      gh_hostname=${gh_hostname##*@}
      path=${origin#*:}
      ;;
    *)
      return 1
      ;;
  esac

  path=${path%.git}
  [[ $gh_hostname =~ ^[A-Za-z0-9.-]+$ ]] || return 1
  [[ $path =~ ^([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)$ ]] || return 1
  gh_repository_path="${BASH_REMATCH[1]}/${BASH_REMATCH[2]}"
  gh_repository="$gh_hostname/$gh_repository_path"
}

list_open_pull_requests() {
  local pull_requests number head files
  pull_requests=$(gh pr list --repo "$gh_repository" --state open --limit 200 --json number,headRefName \
    --jq '.[] | [.number, .headRefName] | @tsv') || {
    printf 'cannot read the open pull request condition\n' >&2
    exit 1
  }
  while IFS=$'\t' read -r number head; do
    [[ -n $number && -n $head ]] || continue
    if [[ $head == "$own_prefix"* ]]; then
      continue
    fi
    open_pr_branches["$head"]=1
    files=$(gh api --hostname "$gh_hostname" --paginate "repos/$gh_repository_path/pulls/$number/files" \
      --jq '.[].filename') || {
      printf 'cannot read the open pull request condition for PR %s\n' "$number" >&2
      exit 1
    }
    if [[ -n $files ]]; then
      while IFS= read -r file; do
        [[ -n $file ]] || continue
        print_claim "$main_root" "$file" "$head"
      done <<<"$files"
    fi
  done <<<"$pull_requests"
}

# Print the committed and uncommitted paths of each worktree whose claimant keeps it live, in the list's form: each
# path with the branch that carries it, and no path whose branch commits all landed on main.
#
# Run metadata maps a worktree path to its task session. The session's current event history owns its state; when no
# task names a worktree, its claims remain. An open pull request, an open task, or a process keeps the worktree's
# unmerged and dirty paths claimed; a path whose branch commits all landed on main prints under no claimant.
#
# The first pass resolves each live branch's merge base, and their common ancestor bounds the one
# shared read of main's patch ids; the second pass classifies each branch's paths against it.
list_worktree_branches() {
  local -a worktrees=() branches=()
  local -a live_worktrees=() live_branches=() live_bases=()
  local worktree= branch= line index merge_base oldest_base

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
        if [[ -n $branch && $branch != "$own_prefix"* ]]; then
          worktrees+=("$worktree")
          branches+=("$branch")
        fi
        ;;
    esac
  done < <(git -C "$main_root" worktree list --porcelain)

  read_worktree_task_states "${CHARLIEBOT_HOME:-}" "${worktrees[@]}"
  for index in "${!worktrees[@]}"; do
    worktree=${worktrees[index]}
    branch=${branches[index]}
    if ! merge_base=$(git -C "$main_root" merge-base "$main_ref" "$branch"); then
      printf 'skip branch %s: no merge base with origin/main\n' "$branch" >&2
      continue
    fi
    if worktree_has_live_claimant "$worktree" "$branch"; then
      live_worktrees+=("$worktree")
      live_branches+=("$branch")
      live_bases+=("$merge_base")
    fi
  done

  if (( ${#live_branches[@]} > 0 )); then
    if ! oldest_base=$(git -C "$main_root" merge-base --octopus "${live_bases[@]}"); then
      printf 'cannot read a common ancestor of the live branches\n' >&2
      exit 1
    fi
    read_main_patch_ids "$main_root" "$oldest_base" "$main_ref"
  fi

  for index in "${!live_branches[@]}"; do
    worktree=${live_worktrees[index]}
    branch=${live_branches[index]}
    list_committed_paths "$main_root" "${live_bases[index]}" "$branch"
    if [[ -d $worktree ]]; then
      list_dirty_paths "$worktree" "$branch"
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

  if [[ -n ${CHARLIEBOT_HOME:-} && $CHARLIEBOT_HOME != /* ]]; then
    printf 'cannot read the task-owner condition: CHARLIEBOT_HOME must be an absolute path\n' >&2
    exit 1
  fi

  # Fetch origin/main before any branch is compared against it. The explicit
  # refspec writes the ref this prefix owns, whatever fetch configuration the
  # clone carries, and --no-write-fetch-head leaves the shared FETCH_HEAD alone.
  main_ref="refs/improve/inflight/${own_prefix//[^A-Za-z0-9._-]/_}"
  # The URL is read as configured, because `git remote get-url` expands a `url.*.insteadOf`
  # mirror to the path it names, and that path carries no GitHub repository. The fetch below
  # follows the mirror all the same.
  origin_url=$(git -C "$main_root" config --get remote.origin.url) || {
    printf 'cannot read the origin of %s\n' "$main_root" >&2
    exit 1
  }
  if ! parse_github_origin "$origin_url"; then
    printf 'cannot parse the origin of %s as a GitHub repository\n' "$main_root" >&2
    exit 1
  fi
  # The fetch names the URL instead of the remote, because a named remote's fetch
  # also updates the remote's tracking ref, and that update is the shared write
  # this fetch must not make.
  if ! git -C "$main_root" fetch --quiet --no-tags --no-write-fetch-head "$origin_url" \
    +refs/heads/main:"$main_ref"; then
    printf 'cannot fetch origin/main in %s\n' "$main_root" >&2
    exit 1
  fi
  # Name the main tree the claims compare against. The line goes to stderr, so the list on stdout keeps its shape.
  fetched_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  main_commit=$(git -C "$main_root" rev-parse --verify --quiet "$main_ref^{commit}") || {
    printf 'cannot read %s after the fetch\n' "$main_ref" >&2
    exit 1
  }
  printf 'origin/main fetched %s into %s at %s\n' "$fetched_at" "$main_ref" "$main_commit" >&2

  {
    list_open_pull_requests
    list_worktree_branches
  } | LC_ALL=C sort -u
fi
