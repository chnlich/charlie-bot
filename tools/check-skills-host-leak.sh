#!/usr/bin/env bash
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"
source tools/leak-patterns.sh

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT
tmp_patterns="$tmp_dir/patterns"
: > "$tmp_patterns"
load_host_patterns

manifest_scan=()
shopt -s nullglob
for manifest in src/features/*/leak_scan_files.txt; do
  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ -z "$line" ]] || manifest_scan+=("$line")
  done < "$manifest"
done
shopt -u nullglob

host_files=()
builtin_files=()
if [[ $# -eq 0 ]]; then
  while IFS= read -r -d '' file; do host_files+=("$file"); done < <(git ls-files -z)
  while IFS= read -r -d '' file; do builtin_files+=("$file"); done < <(find skills prompts -type f -name '*.md' -print0)
  for file in "${manifest_scan[@]}"; do
    [[ ! -f "$file" ]] || builtin_files+=("$file")
  done
else
  host_files=("$@")
  for input in "$@"; do
    rel=${input#./}
    rel=${rel#"$repo_root"/}
    for scan_file in "${manifest_scan[@]}"; do
      if [[ "$rel" == "$scan_file" ]]; then
        builtin_files+=("$input")
        continue 2
      fi
    done
    [[ "$input" == *.md ]] || continue
    if [[ "$input" = /* ]]; then
      case "$input" in
        "$repo_root"/skills/*.md|"$repo_root"/prompts/*.md) builtin_files+=("$input") ;;
        "$repo_root"/*) ;;
        *) builtin_files+=("$input") ;;
      esac
    else
      case "$rel" in skills/*.md|prompts/*.md) builtin_files+=("$input") ;; esac
    fi
  done
fi

read_file() {
  if [[ ${LEAK_SCAN_STAGED:-0} == 1 ]]; then
    git show ":${1#./}"
  else
    cat -- "$1"
  fi
}

matches=0
built_in_regex='[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[0-9a-f]{4}-[0-9a-f]{12}|/data/(checkpoints|datasets|home)/|/home/[^/[:space:]]+/|\.ts\.net\b|[a-z][a-z0-9-]{2,}\.slack\.com'
for file in "${builtin_files[@]}"; do
  read_file "$file" > "$tmp_dir/content"
  if grep -aiEn "$built_in_regex" "$tmp_dir/content" > "$tmp_dir/matches"; then
    printf 'Built-in pattern matches in %s:\n' "$file"
    cat "$tmp_dir/matches"
    matches=$((matches + 1))
  else
    status=$?
    [[ "$status" -eq 1 ]] || exit "$status"
  fi
  if grep -aiEn '[a-z0-9]{6,}\.feishu\.cn' "$tmp_dir/content" > "$tmp_dir/candidates"; then
    if grep -vE 'open\.feishu\.cn|<tenant>\.feishu\.cn|xxx\.feishu\.cn' "$tmp_dir/candidates" > "$tmp_dir/matches"; then
      printf 'Built-in pattern matches in %s:\n' "$file"
      cat "$tmp_dir/matches"
      matches=$((matches + 1))
    else
      status=$?
      [[ "$status" -eq 1 ]] || exit "$status"
    fi
  else
    status=$?
    [[ "$status" -eq 1 ]] || exit "$status"
  fi
done

if [[ -s "$tmp_patterns" ]]; then
  for file in "${host_files[@]}"; do
    read_file "$file" > "$tmp_dir/content"
    if [[ "$file" == *.gz ]]; then
      gzip -cd "$tmp_dir/content" > "$tmp_dir/uncompressed"
      scan="$tmp_dir/uncompressed"
    else
      scan="$tmp_dir/content"
    fi
    if grep -aiEq -f "$tmp_patterns" "$scan"; then
      printf 'Host pattern matches in %s (patterns: %s)\n' "$file" "$extra_patterns_file"
      matches=$((matches + 1))
    else
      status=$?
      [[ "$status" -eq 1 ]] || exit "$status"
    fi
  done
fi
printf 'Summary: %d files with matches\n' "$matches"
[[ "$matches" -eq 0 ]]
