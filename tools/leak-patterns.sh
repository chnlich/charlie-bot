#!/usr/bin/env bash

load_host_patterns() {
  extra_patterns_file=${EXTRA_PATTERNS_FILE:-}
  if [[ -z "$extra_patterns_file" ]]; then
    local profile_home
    # The profile home, by the rule the server's home module owns: CHARLIEBOT_HOME when set (blank counts as
    # unset), ~/.charliebot otherwise, and a leading ~ in the value is HOME. The hook must resolve it with no release
    # and no Python on PATH.
    profile_home=${CHARLIEBOT_HOME:-$HOME/.charliebot}
    profile_home=${profile_home/#\~/$HOME}
    if [[ -f "$profile_home/skills_leak_patterns.local.txt" ]]; then
      extra_patterns_file="$profile_home/skills_leak_patterns.local.txt"
    fi
  fi
  if [[ -n "$extra_patterns_file" ]]; then
    if [[ ! -r "$extra_patterns_file" ]]; then
      printf 'Extra patterns file is not readable: %s\n' "$extra_patterns_file" >&2
      return 2
    fi
    local status
    if grep -vE '^[[:space:]]*($|#)' "$extra_patterns_file" > "$tmp_patterns"; then
      :
    else
      status=$?
      if [[ "$status" -ne 1 ]]; then
        return "$status"
      fi
    fi
  fi
}
