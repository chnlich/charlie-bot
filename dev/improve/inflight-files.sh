#!/usr/bin/env bash
# List the files that other live work claims.
#
#   dev/improve/inflight-files.sh <own branch prefix>
#
# Print one repository-relative path per line: open pull request files, and the
# committed and uncommitted paths of each worktree branch with a live claimant.
# An open pull request for the branch, a process in its worktree, or an open
# owning task keeps those paths claimed. A worktree no task names also keeps its
# claims. A task owns the worktree when its run metadata names that path; the
# task store's event history, not a run's end, decides whether that task is
# open. With a closed task, no process, and no open pull request, the worktree
# claims no path, whether or not its branch landed. A condition that cannot be
# read stops the script with an error naming the condition.
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

# Print every path changed by one branch since its merge base.
list_committed_paths() {
  local root=$1
  local merge_base=$2
  local branch=$3
  git -C "$root" diff --name-only "$merge_base" "$branch"
}

# Print the tracked changes from HEAD and every untracked path of one checkout.
list_dirty_paths() {
  local worktree=$1
  git -C "$worktree" diff --name-only HEAD
  git -C "$worktree" ls-files --others --exclude-standard
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

# Resolve each candidate worktree to the task sessions whose run metadata names it, then read each task's current
# state from its event history. Run records supply ownership only; their timestamps do not decide whether work is live.
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

  unreadable=$(find "$sessions" -mindepth 5 -maxdepth 5 -type f -name metadata.json ! -readable -print -quit) || {
    printf 'cannot read the task-owner condition: cannot list %s\n' "$sessions" >&2
    exit 1
  }
  if [[ -n $unreadable ]]; then
    printf 'cannot read the task-owner condition: %s is unreadable\n' "$unreadable" >&2
    exit 1
  fi
  record_list=$(find "$sessions" -mindepth 5 -maxdepth 5 -type f -name metadata.json -print) || {
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
      printf 'cannot read the task-owner condition: run metadata is unreadable or invalid\n' >&2
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

# Return success when an open pull request, an open task, a process, or no task owner keeps this worktree live.
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
    open | no_task) return 0 ;;
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

list_open_pull_requests() {
  local pull_requests number head files
  pull_requests=$(gh pr list --state open --limit 200 --json number,headRefName \
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
    files=$(gh api --paginate "repos/{owner}/{repo}/pulls/$number/files" --jq '.[].filename') || {
      printf 'cannot read the open pull request condition for PR %s\n' "$number" >&2
      exit 1
    }
    if [[ -n $files ]]; then
      printf '%s\n' "$files"
    fi
  done <<<"$pull_requests"
}

# Print the committed and uncommitted paths of each worktree whose claimant keeps it live.
#
# Run metadata maps a worktree path to its task session. The session's current event history owns its state; when no
# task names a worktree, its claims remain. An open pull request, an open task, or a process keeps every branch path
# and every dirty path claimed, including paths whose branch version has landed on main.
list_worktree_branches() {
  local -a worktrees=() branches=()
  local worktree= branch= line index merge_base

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
      list_committed_paths "$main_root" "$merge_base" "$branch"
      if [[ -d $worktree ]]; then
        list_dirty_paths "$worktree"
      fi
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
