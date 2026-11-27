#!/usr/bin/env bash
set -euo pipefail

scanner=$(git config --local --get charliebot.queueTestScanner)
patterns=$(git config --local --get charliebot.queueTestPatterns)
home=$(git config --local --get charliebot.queueTestHome)
claude=$(git config --local --get charliebot.queueTestClaude)
codex=$(git config --local --get charliebot.queueTestCodex)

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT
texts="$tmp_dir/texts"
: > "$texts"
index=0

add_text() {
  local field=$1 content=$2 path="$tmp_dir/text-$index"
  printf '%s' "$content" > "$path"
  printf '%s\t%s\n' "$field" "$path" >> "$texts"
  index=$((index + 1))
}

while IFS=' ' read -r old_sha new_sha refname; do
  add_text "received ref $index" "$refname"
  if [[ "$new_sha" =~ ^0+$ ]]; then
    continue
  fi
  if [[ "$old_sha" =~ ^0+$ ]]; then
    commits=$(git rev-list "$new_sha" --not --all)
  else
    commits=$(git rev-list "$new_sha" --not "$old_sha")
  fi
  while IFS= read -r commit; do
    [[ -n "$commit" ]] || continue
    local_path="$tmp_dir/commit-$index"
    git show -s --format=%B "$commit" > "$local_path"
    printf 'commit message %s\t%s\n' "$index" "$local_path" >> "$texts"
    index=$((index + 1))
  done <<< "$commits"
done

env -i \
  PATH="$PATH" \
  HOME="$home" \
  CHARLIEBOT_HOME="$home" \
  CLAUDE_CONFIG_DIR="$claude" \
  CODEX_HOME="$codex" \
  EXTRA_PATTERNS_FILE="$patterns" \
  bash "$scanner" --check-list "$texts"
