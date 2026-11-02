#!/usr/bin/env bash
# The remote runner's shared functions: configuration, preflight, ssh, and sync.
#
# Sourced by run.sh and setup.sh, which run from the directory to work in. Host-specific values
# come from the environment; see README.md for each one.

REMOTE_RUN_SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
REMOTE_RUN_TAILSCALE_STATUS=86
REMOTE_RUN_OUTPUT=''
REMOTE_RUN_LOCAL_PYTHON_VERSION=''
REMOTE_RUN_WORKTREE_ROOT=''
REMOTE_RUN_IGNORED_FILTERS=()

# The remote state base, $HOME-relative on the build host. The default is a plain name; the host
# points it at its installed runner state by exporting the variable.
REMOTE_RUN_REMOTE_BASE=${REMOTE_RUN_REMOTE_BASE:-scripts/remote-run}
# The connectivity check's time limit in seconds. Tailscale's check mode otherwise waits 30
# minutes in silence; the limit turns that wait into a reported blocker.
REMOTE_RUN_CONNECT_TIMEOUT=${REMOTE_RUN_CONNECT_TIMEOUT:-60}
REMOTE_RUN_RSYNC_RSH="$REMOTE_RUN_SCRIPT_DIR/ssh-guard.pl --server-alive 3"

remote_run_fail() {
  printf 'remote-run: %s\n' "$1" >&2
  exit 2
}

# Every ssh call names the host through this variable, so no tracked file carries the host's
# address or alias.
remote_run_configure_host() {
  if [[ -z "${REMOTE_RUN_HOST:-}" ]]; then
    remote_run_fail 'REMOTE_RUN_HOST is unset; export it in your shell profile with the ssh alias of the build host'
  fi
}

remote_run_validate_base() {
  if [[ ! "$REMOTE_RUN_REMOTE_BASE" =~ ^[A-Za-z0-9._/-]+$ || "$REMOTE_RUN_REMOTE_BASE" == */ ]]; then
    remote_run_fail "REMOTE_RUN_REMOTE_BASE must be a \$HOME-relative path of safe characters: $REMOTE_RUN_REMOTE_BASE"
  fi
  if [[ "$REMOTE_RUN_REMOTE_BASE" == *..* ]]; then
    remote_run_fail "REMOTE_RUN_REMOTE_BASE must not climb out of \$HOME: $REMOTE_RUN_REMOTE_BASE"
  fi
}

remote_run_base_path() {
  printf '%s' "\$HOME/$REMOTE_RUN_REMOTE_BASE"
}

remote_run_local_preflight() {
  local tool
  for tool in ssh rsync git base64 perl timeout mktemp; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      printf 'preflight: required local tool %s is missing\n' "$tool" >&2
      return 2
    fi
  done
  if [[ ! -x "$REMOTE_RUN_SCRIPT_DIR/ssh-guard.pl" ]]; then
    printf 'preflight: %s/ssh-guard.pl is not executable\n' "$REMOTE_RUN_SCRIPT_DIR" >&2
    return 2
  fi
}

# The sync root is the whole git worktree that holds the current directory, or the current
# directory itself when it is not inside one.
remote_run_resolve_root() {
  local root current_dir
  if root=$(git rev-parse --show-toplevel 2>/dev/null); then
    REMOTE_RUN_SYNC_MODE=worktree
  else
    REMOTE_RUN_SYNC_MODE=directory
    root=$PWD
  fi
  REMOTE_RUN_WORKTREE_ROOT=$(cd -- "$root" && pwd -P)
  current_dir=$(pwd -P)
  if [[ "$current_dir" != "$REMOTE_RUN_WORKTREE_ROOT" && "$current_dir" != "$REMOTE_RUN_WORKTREE_ROOT/"* ]]; then
    printf 'preflight: current directory is outside the resolved sync root: %s\n' "$current_dir" >&2
    return 2
  fi
  if [[ "$REMOTE_RUN_WORKTREE_ROOT" == *$'\n'* ]]; then
    printf 'preflight: sync root path contains a newline and cannot be recorded safely\n' >&2
    return 2
  fi
  if [[ "$current_dir" == "$REMOTE_RUN_WORKTREE_ROOT" ]]; then
    REMOTE_RUN_RELATIVE_DIR='.'
  else
    REMOTE_RUN_RELATIVE_DIR=${current_dir#"$REMOTE_RUN_WORKTREE_ROOT/"}
  fi
}

# The remote tree takes the sync root's name, so the name must be safe for a remote path.
remote_run_tree_name() {
  local tree_name
  tree_name=$(basename -- "$REMOTE_RUN_WORKTREE_ROOT")
  if [[ ! "$tree_name" =~ ^[A-Za-z0-9._-]+$ ]]; then
    printf 'preflight: directory name is not safe for a remote path: %s\n' "$tree_name" >&2
    return 2
  fi
  REMOTE_RUN_TREE_NAME=$tree_name
}

remote_run_has_python_project() {
  [[ -f "$1/pyproject.toml" ]]
}

# The remote Python must be the same CPython patch release the local project's .venv pins.
remote_run_read_local_python_version() {
  local root=$1 python version_line implementation status
  python="$root/.venv/bin/python"
  if [[ ! -x "$python" ]]; then
    printf 'preflight: the project Python is missing: %s; create the project .venv first\n' "$python" >&2
    return 2
  fi
  if version_line=$("$python" --version 2>&1); then
    :
  else
    status=$?
    printf 'preflight: %s --version failed (status %s): %s\n' "$python" "$status" "$version_line" >&2
    return "$status"
  fi
  if [[ ! "$version_line" =~ ^Python[[:space:]](3\.[0-9]+\.[0-9]+)$ ]]; then
    printf 'preflight: expected CPython 3.x.y from %s --version, got: %s\n' "$python" "$version_line" >&2
    return 2
  fi
  version=${BASH_REMATCH[1]}
  if implementation=$("$python" -c 'import platform; print(platform.python_implementation())'); then
    :
  else
    status=$?
    printf 'preflight: cannot identify the interpreter at %s (status %s)\n' "$python" "$status" >&2
    return "$status"
  fi
  if [[ "$implementation" != CPython ]]; then
    printf 'preflight: expected CPython in %s; found %s %s\n' "$python" "$implementation" "$version" >&2
    return 2
  fi
  REMOTE_RUN_LOCAL_PYTHON_VERSION=$version
}

# Read the build host's address for people, which the stop text links. The status goes through the
# || capture: after `if output=$(...)` with an assignment in the then-branch, bash's $? is reset in
# the else-branch.
remote_run_public_url() {
  local output status
  status=0
  output=$(charliebot config get server.public_url 2>&1) || status=$?
  if (( status == 0 )); then
    REMOTE_RUN_PUBLIC_URL=${output%$'\n'}
    REMOTE_RUN_PUBLIC_URL=${REMOTE_RUN_PUBLIC_URL%/}
    return 0
  fi
  printf '%s\n' "$output" >&2
  return "$status"
}

# The stop text tells the agent to end the run and report the blocker, with the /host-auth page.
remote_run_stop_text() {
  local reason=$1
  printf 'remote-run: %s\n' "$reason" >&2
  printf 'remote-run: End this run and report the blocker to the user. Do not retry the command.\n' >&2
  if remote_run_public_url; then
    printf 'remote-run: The /host-auth page: %s/host-auth\n' "$REMOTE_RUN_PUBLIC_URL" >&2
  fi
}

# The one connectivity check every run starts with. A Tailscale login link in the check's output
# exits 86; every other failure, including the time limit, exits 87.
remote_run_connectivity_check() {
  local stderr_file status
  stderr_file=$(mktemp "${TMPDIR:-/tmp}/remote-run-connect.XXXXXX")
  if timeout -k 5 "$REMOTE_RUN_CONNECT_TIMEOUT" \
    "$REMOTE_RUN_SCRIPT_DIR/ssh-guard.pl" --server-alive 3 "$REMOTE_RUN_HOST" true \
    2>"$stderr_file" >/dev/null; then
    status=0
  else
    status=$?
  fi
  if (( status == 0 )); then
    rm -f -- "$stderr_file"
    return 0
  fi
  cat -- "$stderr_file" >&2
  rm -f -- "$stderr_file"
  if (( status == REMOTE_RUN_TAILSCALE_STATUS )); then
    remote_run_stop_text "Tailscale needs an SSH approval for $REMOTE_RUN_HOST."
    exit "$status"
  fi
  remote_run_stop_text "The connectivity check to $REMOTE_RUN_HOST failed (exit $status)."
  exit 87
}

remote_run_capture() {
  local remote_command=$1 output status
  status=0
  output=$("$REMOTE_RUN_SCRIPT_DIR/ssh-guard.pl" --server-alive 3 "$REMOTE_RUN_HOST" "$remote_command" </dev/null) || status=$?
  REMOTE_RUN_OUTPUT=$output
  if (( status == REMOTE_RUN_TAILSCALE_STATUS )); then
    printf '%s\n' "$output" >&2
  fi
  return "$status"
}

remote_run_stream() {
  local remote_command=$1
  "$REMOTE_RUN_SCRIPT_DIR/ssh-guard.pl" --server-alive 3 "$REMOTE_RUN_HOST" "$remote_command"
}

# The remote toolchain check runs perl as well: remote-exec.pl is perl.
remote_run_toolchain_preflight() {
  local status base
  base=$(remote_run_base_path)
  if remote_run_capture "set -e; base=\"$base\"; export CARGO_HOME=\"\$base/cargo\" RUSTUP_HOME=\"\$base/rustup\" UV_PYTHON_INSTALL_DIR=\"\$base/python\" UV_CACHE_DIR=\"\$base/cache/uv\" PATH=\"\$base/node/bin:\$base/bin:\$base/cargo/bin:\$HOME/.local/bin:\$PATH\"; for tool in node npm cargo rustc rsync git flock nice perl; do command -v \"\$tool\" >/dev/null || { echo \"missing build host tool: \$tool\" >&2; exit 2; }; done; test -x \"\$base/node/bin/node\"; test -x \"\$base/bin/uv\"; test -x \"\$base/cargo/bin/rustc\"; test -d \"\$base/rustup/toolchains\"; test -d \"\$base/python\"; test -d \"\$base/slots\"; test -f \"\$base/remote-exec.pl\""; then
    :
  else
    status=$?
    if (( status == REMOTE_RUN_TAILSCALE_STATUS )); then
      return "$status"
    fi
    printf 'preflight: the build host needs node, npm, cargo, uv, rsync, git, flock, nice, perl, rustc, and remote-exec.pl; run setup.sh\n' >&2
    if [[ -n "$REMOTE_RUN_OUTPUT" ]]; then
      printf '%s\n' "$REMOTE_RUN_OUTPUT" >&2
    fi
    return 2
  fi
}

remote_run_python_preflight() {
  local version=$1 minor remote_command remote_line remote_version status base
  base=$(remote_run_base_path)
  minor=${version%.*}
  remote_command="base=\"$base\"; export CARGO_HOME=\"\$base/cargo\" RUSTUP_HOME=\"\$base/rustup\" PATH=\"\$base/bin:\$base/cargo/bin:\$HOME/.local/bin:\$PATH\" UV_PYTHON_INSTALL_DIR=\"\$base/python\" UV_CACHE_DIR=\"\$base/cache/uv\"; python_path=\$(\"\$base/bin/uv\" python find '$minor') && \"\$python_path\" --version"
  if remote_run_capture "$remote_command"; then
    remote_line=$REMOTE_RUN_OUTPUT
  else
    status=$?
    if (( status == REMOTE_RUN_TAILSCALE_STATUS )); then
      return "$status"
    fi
    printf 'preflight: Python version mismatch: the local project is Python %s; the build host uv has no Python %s.x interpreter\n' \
      "$version" "$minor" >&2
    if [[ -n "$REMOTE_RUN_OUTPUT" ]]; then
      printf '%s\n' "$REMOTE_RUN_OUTPUT" >&2
    fi
    return 2
  fi
  if [[ ! "$remote_line" =~ ^Python[[:space:]](3\.[0-9]+\.[0-9]+)$ ]]; then
    printf 'preflight: Python version mismatch: the local project is Python %s; the build host uv returned %q\n' \
      "$version" "$remote_line" >&2
    return 2
  fi
  remote_version=${BASH_REMATCH[1]}
  if [[ "$remote_version" != "$version" ]]; then
    printf 'preflight: Python version mismatch: the local project is Python %s; the build host uv is Python %s\n' \
      "$version" "$remote_version" >&2
    return 2
  fi
}

# Ask the build host for the synced tree's paths, and turn the ones the local tree ignores into
# rsync filter rules, so the sync's delete cannot remove a remote path the upload does not carry.
remote_run_collect_ignored_filters() {
  local root=$1 tree_name=$2 remote_paths_command remote_paths_b64 filter_args_b64 status base
  base=$(remote_run_base_path)
  remote_paths_command="set -euo pipefail; tree=\"\$HOME/$REMOTE_RUN_REMOTE_BASE/trees/$tree_name\"; if [[ -d \"\$tree\" ]]; then find \"\$tree\" -mindepth 1 \\( -path \"\$tree/.git\" -o -path \"\$tree/.venv\" -o -path \"\$tree/rust/target\" -o -name __pycache__ \\) -prune -o -printf '%P\\0'; fi | base64 -w0"
  if remote_run_capture "$remote_paths_command"; then
    remote_paths_b64=$REMOTE_RUN_OUTPUT
  else
    status=$?
    if (( status == REMOTE_RUN_TAILSCALE_STATUS )); then
      return "$status"
    fi
    printf 'sync: cannot list build host paths for ignore protection (status %s): %s\n' \
      "$status" "$REMOTE_RUN_OUTPUT" >&2
    return "$status"
  fi

  if filter_args_b64=$(printf '%s' "$remote_paths_b64" | base64 -d | \
    perl "$REMOTE_RUN_SCRIPT_DIR/git-ignore-filters.pl" "$root" | base64 -w0); then
    :
  else
    status=$?
    printf 'sync: cannot build ignored-file protection rules (status %s)\n' "$status" >&2
    return "$status"
  fi
  mapfile -d '' -t REMOTE_RUN_IGNORED_FILTERS < <(printf '%s' "$filter_args_b64" | base64 -d)
}

remote_run_rsync_capture() {
  local output status
  status=0
  output=$(rsync "$@") || status=$?
  if (( status == REMOTE_RUN_TAILSCALE_STATUS )) || [[ "$output" == *'Tailscale SSH approval is required at '* ]]; then
    printf '%s\n' "$output" >&2
    return "$REMOTE_RUN_TAILSCALE_STATUS"
  fi
  if (( status != 0 )) && [[ -n "$output" ]]; then
    printf '%s\n' "$output" >&2
  fi
  return "$status"
}

# The remote-exec.pl that runs the command is this checkout's copy: sync it to the remote base on
# every run, so a branch's runner changes take effect without a setup pass.
remote_run_refresh_remote_exec() {
  local status
  if remote_run_rsync_capture --archive --checksum --rsh "$REMOTE_RUN_RSYNC_RSH" \
    "$REMOTE_RUN_SCRIPT_DIR/remote-exec.pl" \
    "$REMOTE_RUN_HOST:\$HOME/$REMOTE_RUN_REMOTE_BASE/remote-exec.pl"; then
    :
  else
    status=$?
    if (( status == REMOTE_RUN_TAILSCALE_STATUS )); then
      return "$status"
    fi
    printf 'setup: the remote-exec.pl sync failed (status %s)\n' "$status" >&2
    return "$status"
  fi
}

remote_run_remote_payload() {
  local remote_root=$1 relative_dir=$2 python_version=$3
  shift 3
  printf '%s\0' "$remote_root" "$relative_dir" "$python_version" "$@" | base64 -w0
}

remote_run_remote_command() {
  local payload=$1
  printf 'exec "%s/remote-exec.pl" "%s"' "$(remote_run_base_path)" "$payload"
}
