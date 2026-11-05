#!/usr/bin/env bash
# Run one compiler-enabled command for a configured prescreen session tree.
set -euo pipefail

refuse() {
  printf 'refused: %s\n' "$1" >&2
  exit 1
}

if [[ -z ${CHARLIEBOT_HOME:-} ]]; then
  refuse 'CHARLIEBOT_HOME is unset; it is required for prescreen scope.'
fi
if [[ -z ${CHARLIEBOT_SESSION_ID:-} ]]; then
  refuse 'CHARLIEBOT_SESSION_ID is unset; it is required for prescreen scope.'
fi

scope_dir="$CHARLIEBOT_HOME/local-work-guard"
scope_file="$scope_dir/prescreen-scope.txt"
if [[ ! -e $scope_file ]]; then
  refuse "prescreen scope file is missing: $scope_file"
fi
if [[ ! -r $scope_file ]]; then
  refuse "prescreen scope file is unreadable: $scope_file"
fi
if [[ ! -s $scope_file ]]; then
  refuse "prescreen scope file has no session roots: $scope_file"
fi
if ! command -v jq >/dev/null 2>&1; then
  refuse 'jq is required to read prescreen session metadata.'
fi

session_id_pattern='^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
scope_roots=()
while IFS= read -r scope_root || [[ -n ${scope_root:-} ]]; do
  if [[ -z $scope_root || $scope_root == \#* ]]; then
    continue
  fi
  if [[ ! $scope_root =~ $session_id_pattern ]]; then
    refuse "invalid session root in prescreen scope file: $scope_file"
  fi
  scope_roots+=("$scope_root")
done < "$scope_file"
if (( ${#scope_roots[@]} == 0 )); then
  refuse "prescreen scope file has no session roots: $scope_file"
fi

caller_id=$CHARLIEBOT_SESSION_ID
in_scope=0
while [[ -n $caller_id ]]; do
  if [[ ! $caller_id =~ $session_id_pattern ]]; then
    refuse "invalid caller session ID: $caller_id"
  fi
  for scope_root in "${scope_roots[@]}"; do
    if [[ $caller_id == "$scope_root" ]]; then
      in_scope=1
      break 2
    fi
  done

  metadata_path="$CHARLIEBOT_HOME/sessions/$caller_id/metadata.json"
  if [[ ! -e $metadata_path ]]; then
    refuse "session metadata is missing: $metadata_path"
  fi
  if [[ ! -r $metadata_path ]]; then
    refuse "session metadata is unreadable: $metadata_path"
  fi
  if ! parent_id=$(jq -er '
    if has("task_parent_id") then (.task_parent_id // "")
    else error("metadata lacks task_parent_id")
    end
  ' "$metadata_path"); then
    refuse "cannot read task_parent_id from session metadata: $metadata_path"
  fi
  caller_id=$parent_id
done
if (( ! in_scope )); then
  refuse "caller session is outside the configured prescreen scope: $CHARLIEBOT_SESSION_ID"
fi

if (( $# == 0 )); then
  printf 'usage: %s <command> [arguments...]\n' "$0" >&2
  exit 2
fi

guard_cc=${CC:-}
guard_cxx=${CXX:-}
if [[ -z $guard_cc || -z $guard_cxx ]]; then
  refuse 'the local work guard compiler variables are missing.'
fi
guard_bin=${guard_cc%/*}
if [[ $guard_cc != "$guard_bin/cc" || $guard_cxx != "$guard_bin/c++" ]]; then
  refuse 'CC and CXX do not point to the local work guard compilers.'
fi
if [[ ! -x $guard_cc || ! -x $guard_cxx ]]; then
  refuse 'the local work guard compiler scripts are unavailable.'
fi
# The guard can already be present later in PATH when its child environment prepends it.
path_with_boundaries=":$PATH:"
if [[ $path_with_boundaries != *":$guard_bin:"* ]]; then
  refuse 'the local work guard compiler directory is absent from PATH.'
fi
while [[ $path_with_boundaries == *":$guard_bin:"* ]]; do
  path_with_boundaries=${path_with_boundaries/":$guard_bin:"/:}
done
path_without_guard=$path_with_boundaries
if [[ $path_without_guard == ':' ]]; then
  PATH=''
else
  PATH=${path_without_guard:1:${#path_without_guard}-2}
fi
if [[ -z $PATH ]]; then
  refuse 'PATH has no host tools after removing the compiler refusal directory.'
fi
CC=cc
CXX=c++
export PATH CC CXX

lock_file="$scope_dir/prescreen-entry.lock"
exec 9>"$lock_file"
if flock -n 9; then
  :
else
  printf 'Waiting for an earlier prescreen run to finish.\n' >&2
  flock 9
fi

exec "$@"
