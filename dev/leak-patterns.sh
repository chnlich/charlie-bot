#!/usr/bin/env bash

load_host_patterns() {  # [--require]
  local required=''
  if [[ "${1:-}" == '--require' ]]; then
    required=1
  fi
  extra_patterns_file=${EXTRA_PATTERNS_FILE:-}
  local profile_home
  # The profile home, by the rule the server's home module owns: CHARLIEBOT_HOME when set (blank counts as
  # unset), ~/.charliebot otherwise, and a leading ~ in the value is HOME. The hook runs from git's own
  # environment and must resolve it with plain shell.
  profile_home=${CHARLIEBOT_HOME:-$HOME/.charliebot}
  profile_home=${profile_home/#\~/$HOME}
  if [[ -z "$extra_patterns_file" ]]; then
    if [[ -n "$required" || -f "$profile_home/skills_leak_patterns.local.txt" ]]; then
      extra_patterns_file="$profile_home/skills_leak_patterns.local.txt"
    fi
  fi
  if [[ -n "$extra_patterns_file" ]]; then
    if [[ ! -f "$extra_patterns_file" ]]; then
      printf 'The local leak patterns file does not exist: %s\n' "$extra_patterns_file" >&2
      return 2
    fi
    if [[ ! -r "$extra_patterns_file" ]]; then
      printf 'The local leak patterns file is not readable: %s\n' "$extra_patterns_file" >&2
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
  # --require fails closed: a caller that mandates the local patterns gets no scan at all when the file
  # holds none outside comments and blanks, instead of a scan that silently skips them.
  if [[ -n "$required" && ! -s "$tmp_patterns" ]]; then
    printf 'The required local leak patterns file holds no pattern outside comments and blanks: %s\n' \
      "${extra_patterns_file:-$profile_home/skills_leak_patterns.local.txt}" >&2
    return 2
  fi
}
