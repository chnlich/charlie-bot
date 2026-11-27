#!/usr/bin/env bash
# Sync the current tree to the build host, run one command there, and copy the results back.
#
#   run.sh <command> [args...]
#   run.sh --dry-run
#
# Called from the directory to run in. Inside a git worktree the sync covers the whole worktree;
# in any other directory it covers the current directory. The command's exit code comes back
# unchanged once the connectivity check has passed, 255 included. Exit 2 is this script's own
# argument or configuration check, 86 the check's Tailscale login link, and 87 the check's other
# failures.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd -P)
source "$SCRIPT_DIR/common.sh"

usage() {
  printf 'usage: %s <command> [args...] | --dry-run\n' "$0" >&2
}

dry_run=0
if (( $# == 1 )) && [[ "$1" == --dry-run ]]; then
  dry_run=1
  set --
elif (( $# == 0 )); then
  usage
  exit 2
fi

tree_retention_days=${REMOTE_RUN_TREE_RETENTION_DAYS-3}
if [[ ! "$tree_retention_days" =~ ^[1-9][0-9]*$ ]]; then
  printf 'remote-run: REMOTE_RUN_TREE_RETENTION_DAYS must be a positive whole number of days: %q\n' \
    "$tree_retention_days" >&2
  exit 2
fi

remote_run_read_config
remote_run_validate_base
remote_run_local_preflight
remote_run_read_caller_host
if (( dry_run == 0 )); then
  remote_run_resolve_root
  sync_root=$REMOTE_RUN_WORKTREE_ROOT
  remote_run_tree_name
  tree_name=$REMOTE_RUN_TREE_NAME
else
  sync_root=''
  tree_name=''
fi

# A tree without pyproject.toml has no Python project: its payload carries an empty Python version.
python_version=''
if (( dry_run == 0 )) && remote_run_has_python_project "$sync_root"; then
  remote_run_read_local_python_version "$sync_root"
  python_version=$REMOTE_RUN_LOCAL_PYTHON_VERSION
fi

# The connectivity check comes first: every later step needs the route, and the check's stop text
# is what an agent follows when the route is blocked.
remote_run_connectivity_check
remote_run_refresh_remote_exec
remote_run_toolchain_preflight
if [[ -n "$python_version" ]]; then
  remote_run_python_preflight "$python_version"
fi

handle_signal() {
  local status=$1 wait_status
  trap - TERM INT HUP
  if [[ -n "${REMOTE_RUN_SSH_PID:-}" ]] && kill -0 "$REMOTE_RUN_SSH_PID" 2>/dev/null; then
    kill -TERM "$REMOTE_RUN_SSH_PID"
    if wait "$REMOTE_RUN_SSH_PID"; then
      :
    else
      wait_status=$?
      printf 'run.sh: ssh guard stopped after signal (status %s)\n' "$wait_status" >&2
    fi
    REMOTE_RUN_SSH_PID=''
  fi
  exit "$status"
}
REMOTE_RUN_SSH_PID=''
trap 'handle_signal 143' TERM
trap 'handle_signal 130' INT
trap 'handle_signal 129' HUP

base=$(remote_run_base_path)

if remote_run_capture "find \"$base/trees\" -mindepth 1 -maxdepth 1 -type d -printf \"%f\\n\""; then
  remote_tree_names=$REMOTE_RUN_OUTPUT
else
  status=$?
  printf 'preflight: cannot list the build host trees (status %s): %s\n' "$status" "$REMOTE_RUN_OUTPUT" >&2
  exit "$status"
fi
if remote_run_capture "find \"$base/tree-owners\" -mindepth 1 -maxdepth 1 -printf \"%f\\n\""; then
  remote_record_names=$REMOTE_RUN_OUTPUT
else
  status=$?
  printf 'preflight: cannot list the build host tree owner records (status %s): %s\n' "$status" "$REMOTE_RUN_OUTPUT" >&2
  exit "$status"
fi
remote_names=$(printf '%s\n%s\n' "$remote_tree_names" "$remote_record_names" | sort -u)

# One read of every tree's last-use record. A live-root tree whose record is younger than the retention age is kept
# without a sweep call, because remote-exec.pl would answer kept-young for it. Every other tree, a tree without a
# record, and a tree whose recorded root is gone still goes to remote-exec.pl, which decides it under the owner lock.
remote_last_use_command="if [[ -d \"$base/tree-last-use\" ]]; then \
  for f in \"$base/tree-last-use\"/*; do \
  [[ -f \"\$f\" ]] && printf '%s %s\\n' \"\${f##*/}\" \"\$(cat -- \"\$f\")\"; done; fi"
if remote_run_capture "$remote_last_use_command"; then
  remote_last_use_lines=$REMOTE_RUN_OUTPUT
else
  status=$?
  printf 'preflight: cannot read the build host tree last-use records (status %s): %s\n' \
    "$status" "$REMOTE_RUN_OUTPUT" >&2
  exit "$status"
fi
declare -A last_use_epochs=()
while read -r last_use_name last_use_epoch; do
  [[ -n "$last_use_name" ]] || continue
  last_use_epochs[$last_use_name]=$last_use_epoch
done <<< "$remote_last_use_lines"
sweep_now=$(date +%s)

# Sweep every build host tree and owner record by name. The caller checks each recorded local root, then the build
# host decides under its owner lock whether the tree is removed. A tree whose current record names a gone root is
# removed at once; every other tree, including a legacy path-only record and a tree with no record, is removed once its
# last use is older than the retention age. Current records for other hosts stay untouched.
dry_run_selected_count=0
dry_run_selected_bytes=0

# Prints one sweep outcome from remote-exec.pl, and adds a dry-run selection to the totals. Every outcome passes here,
# so an outcome that remote-exec.pl adds fails the run rather than passing silently.
report_sweep_outcome() {
  local name=$1 root_state=$2 outcome=$3 selected_bytes process reason local_root_state
  case "$outcome" in
    removed-gone)
      printf 'sweep: removed build host tree %s because its recorded local directory is gone\n' "$name" >&2
      ;;
    removed-age:*)
      printf 'sweep: removed build host tree %s because it was last used more than %s days ago\n' \
        "$name" "$tree_retention_days" >&2
      ;;
    would-remove-gone:*|would-remove-age:*)
      selected_bytes=${outcome##*:}
      dry_run_selected_count=$((dry_run_selected_count + 1))
      dry_run_selected_bytes=$((dry_run_selected_bytes + selected_bytes))
      if [[ "$outcome" == would-remove-gone:* ]]; then
        reason='its recorded local directory is gone'
      else
        reason="it was last used more than $tree_retention_days days ago"
      fi
      printf 'dry-run: selected build host tree %s because %s (%s allocated bytes)\n' \
        "$name" "$reason" "$selected_bytes"
      ;;
    would-drop)
      printf 'dry-run: would drop the owner record of build host tree %s because the tree is missing\n' "$name"
      ;;
    dropped)
      if [[ "$root_state" == live ]]; then
        local_root_state='still exists'
      else
        local_root_state='is gone'
      fi
      printf 'sweep: dropped the owner record of build host tree %s because the tree is missing; ' "$name" >&2
      printf 'its recorded local directory %s\n' "$local_root_state" >&2
      ;;
    absent|kept|kept-young|kept-foreign)
      ;;
    kept-dirty)
      printf 'sweep: kept build host tree %s because it has uncommitted changes\n' "$name" >&2
      ;;
    kept-locked)
      printf 'sweep: kept build host tree %s because its activity lock is held\n' "$name" >&2
      ;;
    kept-in-use:*)
      process=${outcome#kept-in-use:}
      printf 'sweep: kept build host tree %s because process %s uses it\n' "$name" "$process" >&2
      ;;
    *)
      printf 'sweep: build host tree %s returned an unknown outcome: %q\n' "$name" "$outcome" >&2
      exit 2
      ;;
  esac
}

while IFS= read -r remote_name; do
  [[ -n "$remote_name" ]] || continue
  if [[ ! "$remote_name" =~ ^[A-Za-z0-9._-]+$ ]]; then
    printf 'sweep: build host name is unsafe: %s\n' "$remote_name" >&2
    exit 2
  fi
  if grep -Fxq -- "$remote_name" <<< "$remote_tree_names"; then
    listed=1
  else
    listed=0
  fi
  # An entry of any kind counts as a record, so a symlink or directory reaches the read and fails there.
  remote_record_command=$(remote_run_owner_record_query "$base/tree-owners/$remote_name")
  if remote_run_capture "$remote_record_command"; then
    owner_record=$REMOTE_RUN_OUTPUT
  else
    status=$?
    printf 'sweep: cannot read the local-root record for build host tree %s (status %s): %s\n' \
      "$remote_name" "$status" "$REMOTE_RUN_OUTPUT" >&2
    exit "$status"
  fi
  if [[ "$owner_record" == '__NO_LOCAL_ROOT_RECORD__' ]]; then
    # A name that only the record listing showed had its record removed since the listing: nothing is left.
    if (( listed == 0 )); then
      continue
    fi
    # A tree without a record has no recorded local root. The sweep applies its age rule alone, as for a live root.
    has_record=0
    recorded_root=''
    root_state=live
  else
    if ! remote_run_parse_owner_record "$owner_record"; then
      printf 'sweep: build host tree %s has an invalid owner record: %q\n' \
        "$remote_name" "$owner_record" >&2
      exit 2
    fi
    has_record=1
    recorded_root=$REMOTE_RUN_OWNER_ROOT
    if [[ "$REMOTE_RUN_OWNER_FORMAT" == current && "$REMOTE_RUN_OWNER_HOST" != "$REMOTE_RUN_CALLER_HOST" ]]; then
      printf 'sweep: kept build host tree %s owned by caller host %s; this caller host is %s\n' \
        "$remote_name" "$REMOTE_RUN_OWNER_HOST" "$REMOTE_RUN_CALLER_HOST" >&2
      continue
    fi
    # remote-exec.pl applies the gone rule only to a current record, so a legacy record's root state changes nothing.
    if [[ -d "$recorded_root" ]]; then
      root_state=live
    else
      root_state=gone
    fi
  fi
  if [[ "$root_state" == live && -n "${last_use_epochs[$remote_name]-}" ]] \
      && (( sweep_now - last_use_epochs[$remote_name] < tree_retention_days * 86400 )); then
    outcome=kept-young
  else
    sweep_command=$(remote_run_sweep_tree_command \
      "$REMOTE_RUN_CALLER_HOST" "$base/trees/$remote_name" "$root_state" "$tree_retention_days" "$dry_run")
    if remote_run_capture "$sweep_command"; then
      outcome=$REMOTE_RUN_OUTPUT
    else
      status=$?
      printf 'sweep: failed to sweep build host tree %s (status %s): %s\n' \
        "$remote_name" "$status" "$REMOTE_RUN_OUTPUT" >&2
      exit "$status"
    fi
  fi
  report_sweep_outcome "$remote_name" "$root_state" "$outcome"
  if (( dry_run == 0 && has_record == 1 )) && [[ "$root_state" == live && "$remote_name" == "$tree_name" \
      && "$recorded_root" != "$sync_root" ]] && \
      [[ "$outcome" != removed-age:* && "$outcome" != removed-gone && "$outcome" != dropped \
      && "$outcome" != absent ]]; then
    printf 'sync: build host tree %s belongs to local directory %s, not %s\n' \
      "$remote_name" "$recorded_root" "$sync_root" >&2
    exit 2
  fi
done <<< "$remote_names"

if (( dry_run )); then
  gib_tenths=$(((dry_run_selected_bytes * 10 + 536870912) / 1073741824))
  printf 'dry-run: selected %s trees, %s allocated bytes (%s.%s GiB)\n' \
    "$dry_run_selected_count" "$dry_run_selected_bytes" "$((gib_tenths / 10))" "$((gib_tenths % 10))"
  exit 0
fi

exclude_args=(
  --exclude=/.git
  --exclude=/.venv/
  --exclude=__pycache__/
  --exclude=/rust/target/
  --filter=:-\ .gitignore
)
# rsync expands a leading ~ on the remote path itself; a $HOME would reach it unexpanded.
remote_tree="$REMOTE_RUN_HOST:~/$REMOTE_RUN_REMOTE_BASE/trees/$tree_name/"

marker_query=$(remote_run_owner_record_query "$base/tree-owners/$tree_name")
if remote_run_capture "$marker_query"; then
  existing_record=$REMOTE_RUN_OUTPUT
else
  status=$?
  printf 'sync: cannot inspect build host tree ownership (status %s): %s\n' "$status" "$REMOTE_RUN_OUTPUT" >&2
  exit "$status"
fi
encoded_root=$(printf '%s' "$sync_root" | base64 -w0)
if [[ "$existing_record" != '__NO_LOCAL_ROOT_RECORD__' ]]; then
  if ! remote_run_parse_owner_record "$existing_record"; then
    printf 'sync: build host tree %s has an invalid owner record: %q\n' "$tree_name" "$existing_record" >&2
    exit 2
  fi
  if [[ "$REMOTE_RUN_OWNER_FORMAT" == legacy ]]; then
    if [[ "$REMOTE_RUN_OWNER_ROOT" != "$sync_root" ]]; then
      printf 'sync: build host tree %s belongs to legacy local directory %s, not %s\n' \
        "$tree_name" "$REMOTE_RUN_OWNER_ROOT" "$sync_root" >&2
      exit 2
    fi
  fi
  if [[ "$REMOTE_RUN_OWNER_FORMAT" == current && \
      ( "$REMOTE_RUN_OWNER_HOST" != "$REMOTE_RUN_CALLER_HOST" || "$REMOTE_RUN_OWNER_ROOT" != "$sync_root" ) ]]; then
    printf 'sync: build host tree %s belongs to caller host %s at %s; this run is caller host %s at %s\n' \
      "$tree_name" "$REMOTE_RUN_OWNER_HOST" "$REMOTE_RUN_OWNER_ROOT" "$REMOTE_RUN_CALLER_HOST" "$sync_root" >&2
    exit 2
  fi
  remote_check=$(remote_run_check_tree_command \
    "$REMOTE_RUN_CALLER_HOST" "$base/trees/$tree_name" "$encoded_root")
  if remote_run_capture "$remote_check"; then
    case "$REMOTE_RUN_OUTPUT" in
      ready|absent)
        ;;
      kept-locked)
        printf 'sync: kept build host tree %s because its activity lock is held\n' "$tree_name" >&2
        exit 2
        ;;
      kept-in-use:*)
        printf 'sync: kept build host tree %s because process %s uses it\n' \
          "$tree_name" "${REMOTE_RUN_OUTPUT#kept-in-use:}" >&2
        exit 2
        ;;
      *)
        printf 'sync: build host tree %s returned an unknown pre-sync check outcome: %q\n' \
          "$tree_name" "$REMOTE_RUN_OUTPUT" >&2
        exit 2
        ;;
    esac
  else
    status=$?
    printf 'sync: failed to check build host tree %s before replacing it (status %s): %s\n' \
      "$tree_name" "$status" "$REMOTE_RUN_OUTPUT" >&2
    exit "$status"
  fi
fi

# The claim writes the owner record and creates the tree together, under the build host's owner lock. A claim that
# fails leaves neither, and one that stops after its record leaves a record the next sweep drops.
remote_claim=$(remote_run_claim_tree_command "$REMOTE_RUN_CALLER_HOST" "$base/trees/$tree_name" "$encoded_root")
if remote_run_capture "$remote_claim"; then
  :
else
  status=$?
  printf 'sync: cannot claim build host tree %s (status %s): %s\n' "$tree_name" "$status" "$REMOTE_RUN_OUTPUT" >&2
  exit "$status"
fi

remote_run_collect_ignored_filters "$sync_root" "$tree_name"
ignored_filter_args=("${REMOTE_RUN_IGNORED_FILTERS[@]}")

# Do not queue an upload behind a sweep lock: it could otherwise recreate the tree after that sweep removes it.
remote_rsync_path=$(printf 'flock -n -x -- "$HOME/%s/tree-locks/%s.lock" rsync' \
  "$REMOTE_RUN_REMOTE_BASE" "$tree_name")
if remote_run_rsync_capture --archive --delete "${exclude_args[@]}" "${ignored_filter_args[@]}" \
  --rsync-path="$remote_rsync_path" \
  --rsh "$REMOTE_RUN_RSYNC_RSH" "$sync_root/" "$remote_tree"; then
  :
else
  status=$?
  if (( status == REMOTE_RUN_TAILSCALE_STATUS )); then
    exit "$status"
  fi
  printf 'sync: the local upload failed (status %s)\n' "$status" >&2
  exit "$status"
fi

# The upload excludes .git, so the remote tree has no history, and release tests that read git
# history fail with 128. Keep a synthetic commit and an origin/main ref in every synced tree, and
# refresh them after each upload so HEAD matches the uploaded files. A commit that finds nothing
# to commit exits 1, which is fine.
remote_commit_command="cd \"$base/trees/$tree_name\" || exit 2; if [[ ! -e .git ]]; then git init -q -b main || exit 2; fi; git add -A || exit 2; git -c user.name=remote-run -c user.email=remote-run@localhost commit -q -m 'remote-run sync' || [[ \$? -eq 1 ]]; git update-ref refs/remotes/origin/main HEAD"
if remote_run_capture "$remote_commit_command"; then
  :
else
  status=$?
  if (( status == REMOTE_RUN_TAILSCALE_STATUS )); then
    exit "$status"
  fi
  printf 'sync: the remote tree git history could not be prepared (status %s): %s\n' \
    "$status" "$REMOTE_RUN_OUTPUT" >&2
  exit "$status"
fi

# The build host owns the build outputs: keep rust/target there and drop the local copy after a
# successful upload, so a cargo command never leaves a second target tree on this host.
command_name=$1
if [[ "$command_name" == cargo ]]; then
  rm -rf -- "$sync_root/rust/target" || {
    printf 'cleanup: failed to delete the local rust/target after successful sync\n' >&2
    exit 2
  }
fi

# The payload's tree root carries a leading ~, which remote-exec.pl expands with its own $HOME.
remote_root="~/$REMOTE_RUN_REMOTE_BASE/trees/$tree_name"
relative_dir=$REMOTE_RUN_RELATIVE_DIR
shift
payload=$(remote_run_remote_payload "$remote_root" "$relative_dir" "$python_version" "$command_name" "$@")
remote_command=$(remote_run_remote_command "$payload" "$base/trees/$tree_name")
# ssh-guard runs as this shell's direct child. Backgrounding the stream would make a subshell the
# parent instead, and that subshell outlives a SIGKILLed run.sh, which keeps the remote command
# attached.
"$REMOTE_RUN_SCRIPT_DIR/ssh-guard.pl" --server-alive 3 "$REMOTE_RUN_HOST" "$remote_command" &
REMOTE_RUN_SSH_PID=$!
command_status=0
wait "$REMOTE_RUN_SSH_PID" || command_status=$?
REMOTE_RUN_SSH_PID=''

if (( command_status == REMOTE_RUN_TAILSCALE_STATUS )); then
  exit "$command_status"
fi

remote_run_collect_ignored_filters "$sync_root" "$tree_name"
ignored_filter_args=("${REMOTE_RUN_IGNORED_FILTERS[@]}")
if remote_run_rsync_capture --archive --update "${exclude_args[@]}" "${ignored_filter_args[@]}" \
  --rsync-path="$remote_rsync_path" \
  --rsh "$REMOTE_RUN_RSYNC_RSH" "$remote_tree" "$sync_root/"; then
  :
else
  status=$?
  if (( status == REMOTE_RUN_TAILSCALE_STATUS )); then
    exit "$status"
  fi
  printf 'sync: the build host changes could not be copied back (status %s)\n' "$status" >&2
  exit "$status"
fi

exit "$command_status"
