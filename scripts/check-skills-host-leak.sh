#!/usr/bin/env bash
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

files=()
if [[ $# -eq 0 ]]; then
  while IFS= read -r -d '' file; do
    files+=("$file")
  done < <(find skills prompts -type f -name '*.md' -print0)
else
  for input in "$@"; do
    [[ "$input" == *.md ]] || continue

    if [[ "$input" = /* ]]; then
      case "$input" in
        "$repo_root"/skills/*.md|"$repo_root"/prompts/*.md)
          files+=("$input")
          ;;
        "$repo_root"/*)
          ;;
        *)
          files+=("$input")
          ;;
      esac
    else
      rel=${input#./}
      case "$rel" in
        skills/*.md|prompts/*.md)
          files+=("$rel")
          ;;
      esac
    fi
  done
fi

count_lines() {
  local text=${1:-}
  if [[ -z "$text" ]]; then
    printf '0\n'
  else
    printf '%s\n' "$text" | wc -l | tr -d '[:space:]'
    printf '\n'
  fi
}

grep_matches() {
  local status
  local output

  if output=$("$@" 2>&1); then
    printf '%s' "$output"
    return 0
  fi

  status=$?
  if [[ "$status" -eq 1 ]]; then
    return 0
  fi

  printf '%s\n' "$output" >&2
  return "$status"
}

built_in_matches=""
extra_matches=""

if [[ ${#files[@]} -gt 0 ]]; then
  built_in_regex='[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[0-9a-f]{4}-[0-9a-f]{12}|/data/(checkpoints|datasets|home)/|/home/[^/[:space:]]+/|\.ts\.net\b|[a-z][a-z0-9-]{2,}\.slack\.com'
  built_in_matches=$(grep_matches grep -nEHi "$built_in_regex" -- "${files[@]}")

  feishu_candidates=$(grep_matches grep -nEHi '[a-z0-9]{6,}\.feishu\.cn' -- "${files[@]}")
  if [[ -n "$feishu_candidates" ]]; then
    if filtered_feishu=$(printf '%s\n' "$feishu_candidates" | grep -vE 'open\.feishu\.cn|<tenant>\.feishu\.cn|xxx\.feishu\.cn'); then
      :
    else
      status=$?
      if [[ "$status" -ne 1 ]]; then
        printf 'Failed to filter built-in matches\n' >&2
        exit "$status"
      fi
    fi
    if [[ -n "$filtered_feishu" ]]; then
      if [[ -n "$built_in_matches" ]]; then
        built_in_matches+=$'\n'
      fi
      built_in_matches+="$filtered_feishu"
    fi
  fi

  extra_patterns_file=${EXTRA_PATTERNS_FILE:-}
  if [[ -z "$extra_patterns_file" && -f "$HOME/.charliebot/skills_leak_patterns.local.txt" ]]; then
    extra_patterns_file="$HOME/.charliebot/skills_leak_patterns.local.txt"
  fi

  if [[ -n "$extra_patterns_file" ]]; then
    if [[ ! -r "$extra_patterns_file" ]]; then
      printf 'Extra patterns file is not readable: %s\n' "$extra_patterns_file" >&2
      exit 2
    fi

    tmp_patterns=$(mktemp)
    trap 'rm -f "$tmp_patterns"' EXIT

    if grep -vE '^[[:space:]]*($|#)' "$extra_patterns_file" > "$tmp_patterns"; then
      :
    else
      status=$?
      if [[ "$status" -ne 1 ]]; then
        printf 'Failed to read extra patterns file: %s\n' "$extra_patterns_file" >&2
        exit "$status"
      fi
    fi

    if [[ -s "$tmp_patterns" ]]; then
      extra_matches=$(grep_matches grep -nEHi -f "$tmp_patterns" -- "${files[@]}")
    fi
  fi
fi

built_in_count=$(count_lines "$built_in_matches")
extra_count=$(count_lines "$extra_matches")
total_count=$((built_in_count + extra_count))

if [[ "$built_in_count" -gt 0 ]]; then
  printf '== Built-in pattern matches ==\n'
  printf '%s\n' "$built_in_matches"
fi

if [[ "$extra_count" -gt 0 ]]; then
  printf '== Extra pattern matches ==\n'
  printf '%s\n' "$extra_matches"
fi

printf 'Summary: %d matches (%d built-in, %d extra)\n' "$total_count" "$built_in_count" "$extra_count"

if [[ "$total_count" -gt 0 ]]; then
  exit 1
fi

exit 0
