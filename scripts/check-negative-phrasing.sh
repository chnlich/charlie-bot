#!/usr/bin/env bash
# Check for negative phrasing in memory and skill files.
# Scans bullet lines (- ...) for negative words that should be rephrased positively.
# Per the llm-context-guideline skill positive-phrasing rule: state what the thing is,
# where it belongs, or what to do.
#
# Usage: scripts/check-negative-phrasing.sh
# Exit 0 = clean, 1 = candidates found.
#
# Negative word list is configurable via the NEG_WORDS array below.

set -uo pipefail

NEG_WORDS=(
  '\bnot\b'
  "\bisn't\b"
  "\baren't\b"
  "\bwasn't\b"
  "\bweren't\b"
  "\bdon't\b"
  "\bdoesn't\b"
  "\bdidn't\b"
  '\bno \b'
  '\bnever\b'
  '\bwithout\b'
  '\bavoid\b'
)

NEG_PATTERN=$(printf '|%s' "${NEG_WORDS[@]}")
NEG_PATTERN=${NEG_PATTERN#|}

count=0

check_file() {
  local file="$1"
  if [[ ! -f "$file" ]]; then
    echo "warn: $file not found, skipping" >&2
    return
  fi
  local lineno=0
  while IFS= read -r line; do
    ((lineno++)) || true
    # Skip frontmatter
    if [[ "$line" =~ ^--- ]] || [[ "$line" =~ ^(name|description|version): ]]; then
      continue
    fi
    # Only check bullet lines
    if [[ "$line" =~ ^-[[:space:]] ]]; then
      if echo "$line" | grep -qEi "$NEG_PATTERN"; then
        echo "$file:$lineno: $line"
        count=$((count + 1))
      fi
    fi
  done < "$file"
}

# Repo files
repo_root=$(git rev-parse --show-toplevel 2>/dev/null || echo "")

shopt -s nullglob

repo_files=()
if [[ -n "$repo_root" ]]; then
  for f in \
    "$repo_root"/prompts/*.md \
    "$repo_root"/skills/*/SKILL.md \
    "$repo_root"/skills/*/*/*.md
  do
    repo_files+=("$f")
  done
fi

shopt -u nullglob

for f in "${repo_files[@]}"; do
  check_file "$f"
done

# Local memory store entries
shopt -s globstar nullglob
for f in "$HOME/.charliebot/memory/entries"/**/*.md; do
  check_file "$f"
done
shopt -u globstar nullglob

echo "---"
echo "$count candidates found"
[[ $count -eq 0 ]]
