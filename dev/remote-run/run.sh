#!/usr/bin/env bash
# Sync the current tree to the build host, run one command there, and copy the results back.
#
#   run.sh <command> [args...]
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
  printf 'usage: %s <command> [args...]\n' "$0" >&2
}

if (( $# == 0 )); then
  usage
  exit 2
fi

remote_run_read_config
remote_run_validate_base
remote_run_local_preflight
remote_run_read_caller_host
remote_run_resolve_root
sync_root=$REMOTE_RUN_WORKTREE_ROOT
remote_run_tree_name
tree_name=$REMOTE_RUN_TREE_NAME

# A tree without pyproject.toml has no Python project: its payload carries an empty Python version.
python_version=''
if remote_run_has_python_project "$sync_root"; then
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

# Remove only trees owned by this caller host whose local root is gone. A path-only legacy record
# cannot establish its caller host, so it remains untouched. Refuse a name whose recorded root is
# a different live directory: two local trees must never share one remote tree.
while IFS= read -r remote_name; do
  [[ -n "$remote_name" ]] || continue
  if [[ ! "$remote_name" =~ ^[A-Za-z0-9._-]+$ ]]; then
    printf 'sweep: build host tree has an unsafe directory name: %s\n' "$remote_name" >&2
    exit 2
  fi
  remote_record_command="if [[ -f \"$base/tree-owners/$remote_name\" ]]; then cat -- \"$base/tree-owners/$remote_name\"; else printf '__NO_LOCAL_ROOT_RECORD__'; fi"
  if remote_run_capture "$remote_record_command"; then
    owner_record=$REMOTE_RUN_OUTPUT
  else
    status=$?
    printf 'sweep: cannot read the local-root record for build host tree %s (status %s): %s\n' \
      "$remote_name" "$status" "$REMOTE_RUN_OUTPUT" >&2
    exit "$status"
  fi
  if [[ "$owner_record" == '__NO_LOCAL_ROOT_RECORD__' ]]; then
    if [[ "$remote_name" == "$tree_name" ]]; then
      printf 'sync: build host tree %s has no local-root record; refusing to overwrite it\n' "$remote_name" >&2
      exit 2
    fi
    printf 'sweep: kept build host tree %s because it has no local-root record\n' "$remote_name" >&2
    continue
  fi
  if ! remote_run_parse_owner_record "$owner_record"; then
    printf 'sweep: build host tree %s has an invalid owner record: %q\n' \
      "$remote_name" "$owner_record" >&2
    exit 2
  fi
  if [[ "$REMOTE_RUN_OWNER_FORMAT" == legacy ]]; then
    if [[ "$remote_name" == "$tree_name" ]]; then
      printf 'sync: build host tree %s has legacy owner record %q without a caller host; refusing to overwrite it\n' \
        "$remote_name" "$owner_record" >&2
      exit 2
    fi
    printf 'sweep: kept build host tree %s because its legacy owner record has no caller host\n' \
      "$remote_name" >&2
    continue
  fi
  recorded_root=$REMOTE_RUN_OWNER_ROOT
  if [[ "$REMOTE_RUN_OWNER_HOST" != "$REMOTE_RUN_CALLER_HOST" ]]; then
    printf 'sweep: kept build host tree %s owned by caller host %s; this caller host is %s\n' \
      "$remote_name" "$REMOTE_RUN_OWNER_HOST" "$REMOTE_RUN_CALLER_HOST" >&2
    continue
  fi
  if [[ -d "$recorded_root" ]]; then
    if [[ "$remote_name" == "$tree_name" && "$recorded_root" != "$sync_root" ]]; then
      printf 'sync: build host tree %s belongs to local directory %s, not %s\n' \
        "$remote_name" "$recorded_root" "$sync_root" >&2
      exit 2
    fi
    continue
  fi
  remote_remove=$(remote_run_remove_tree_command "$REMOTE_RUN_CALLER_HOST" "$base/trees/$remote_name")
  if remote_run_capture "$remote_remove"; then
    printf 'sweep: removed build host tree %s because its recorded local directory is gone\n' "$remote_name" >&2
  else
    status=$?
    printf 'sweep: failed to remove build host tree %s (status %s): %s\n' \
      "$remote_name" "$status" "$REMOTE_RUN_OUTPUT" >&2
    exit "$status"
  fi
done <<< "$remote_tree_names"

exclude_args=(
  --exclude=/.git
  --exclude=/.venv/
  --exclude=__pycache__/
  --exclude=/rust/target/
  --filter=:-\ .gitignore
)
# rsync expands a leading ~ on the remote path itself; a $HOME would reach it unexpanded.
remote_tree="$REMOTE_RUN_HOST:~/$REMOTE_RUN_REMOTE_BASE/trees/$tree_name/"

marker_query="if [[ -f \"$base/tree-owners/$tree_name\" ]]; then cat -- \"$base/tree-owners/$tree_name\"; else printf '__NO_LOCAL_ROOT_RECORD__'; fi"
if remote_run_capture "$marker_query"; then
  existing_record=$REMOTE_RUN_OUTPUT
else
  status=$?
  printf 'sync: cannot inspect build host tree ownership (status %s): %s\n' "$status" "$REMOTE_RUN_OUTPUT" >&2
  exit "$status"
fi
if [[ "$existing_record" != '__NO_LOCAL_ROOT_RECORD__' ]]; then
  if ! remote_run_parse_owner_record "$existing_record"; then
    printf 'sync: build host tree %s has an invalid owner record: %q\n' "$tree_name" "$existing_record" >&2
    exit 2
  fi
  if [[ "$REMOTE_RUN_OWNER_FORMAT" == legacy ]]; then
    printf 'sync: build host tree %s has legacy owner record %q without a caller host; refusing to overwrite it\n' \
      "$tree_name" "$existing_record" >&2
    exit 2
  fi
  if [[ "$REMOTE_RUN_OWNER_HOST" != "$REMOTE_RUN_CALLER_HOST" || "$REMOTE_RUN_OWNER_ROOT" != "$sync_root" ]]; then
    printf 'sync: build host tree %s belongs to caller host %s at %s; this run is caller host %s at %s\n' \
      "$tree_name" "$REMOTE_RUN_OWNER_HOST" "$REMOTE_RUN_OWNER_ROOT" "$REMOTE_RUN_CALLER_HOST" "$sync_root" >&2
    exit 2
  fi
  remote_stop=$(remote_run_stop_tree_command "$REMOTE_RUN_CALLER_HOST" "$base/trees/$tree_name")
  if remote_run_capture "$remote_stop"; then
    :
  else
    status=$?
    printf 'sync: failed to stop processes in build host tree %s (status %s): %s\n' \
      "$tree_name" "$status" "$REMOTE_RUN_OUTPUT" >&2
    exit "$status"
  fi
fi

encoded_owner=$(printf 'host=%s\nroot=%s\n' "$REMOTE_RUN_CALLER_HOST" "$sync_root" | base64 -w0)
remote_marker_write="printf '%s' '$encoded_owner' | base64 -d > \"$base/tree-owners/$tree_name\""
if remote_run_capture "$remote_marker_write"; then
  :
else
  status=$?
  printf 'sync: cannot record the local tree owner on the build host (status %s): %s\n' \
    "$status" "$REMOTE_RUN_OUTPUT" >&2
  exit "$status"
fi

remote_run_collect_ignored_filters "$sync_root" "$tree_name"
ignored_filter_args=("${REMOTE_RUN_IGNORED_FILTERS[@]}")

if remote_run_rsync_capture --archive --delete "${exclude_args[@]}" "${ignored_filter_args[@]}" \
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
remote_command=$(remote_run_remote_command "$payload")
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
