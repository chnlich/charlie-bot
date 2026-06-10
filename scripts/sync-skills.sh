#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

# --- Canonical skill sources ---
REPO_SKILLS="$REPO_ROOT/skills"
HOST_SKILLS="$HOME/.charliebot/skills"

# --- Sync targets ---
TARGETS=(
  "$HOME/.claude/skills"
  "$HOME/.agents/skills"
  "$HOME/.gemini/antigravity-cli/skills"  # Antigravity CLI (agy) global skills root
)

# --- Flags ---
DRY_RUN=0
VERBOSE=0

while getopts "nv" opt; do
  case "$opt" in
    n) DRY_RUN=1 ;;
    v) VERBOSE=1 ;;
    *) echo "Usage: $0 [-n] [-v]" >&2; exit 1 ;;
  esac
done

# --- Helpers ---
run() {
  if (( DRY_RUN )); then
    echo "  [dry-run] $*"
  else
    "$@"
  fi
}

log()     { echo "  $*"; }
verbose() { (( VERBOSE )) && echo "  $*" || true; }
warn()    { echo "  WARNING: $*" >&2; }

# --- Collect skills ---
declare -A SKILLS

shopt -s nullglob

# Pass 1: repo skills (lower priority)
for skill_md in "$REPO_SKILLS"/*/SKILL.md; do
  dir="${skill_md%/SKILL.md}"
  dir="${dir%/}"
  name="${dir##*/}"
  SKILLS["$name"]="$dir"
done

# Pass 2: host skills (higher priority, overwrites pass 1)
for skill_md in "$HOST_SKILLS"/*/SKILL.md; do
  dir="${skill_md%/SKILL.md}"
  dir="${dir%/}"
  name="${dir##*/}"
  SKILLS["$name"]="$dir"
done

shopt -u nullglob

echo "Collected ${#SKILLS[@]} skill(s): ${!SKILLS[*]}"
echo

# --- Counters ---
created=0
updated=0
removed=0
skipped=0

# --- Sync to each target ---
for target in "${TARGETS[@]}"; do
  echo "=== Syncing to $target ==="
  run mkdir -p "$target"

  # Step A: Remove stale symlinks
  if [[ -d "$target" ]]; then
    for entry in "$target"/*; do
      [[ -e "$entry" || -L "$entry" ]] || continue
      name="${entry##*/}"
      # Skip dotfiles
      [[ "$name" == .* ]] && continue
      # Skip non-symlinks
      if [[ ! -L "$entry" ]]; then
        verbose "skip non-symlink: $name"
        continue
      fi
      # Remove if dangling or skill name not in collected set
      if [[ ! -e "$entry" ]] || [[ -z "${SKILLS[$name]+x}" ]]; then
        log "remove stale: $name -> $(readlink "$entry")"
        run rm "$entry"
        (( removed++ )) || true
      fi
    done
  fi

  # Step B: Create/update symlinks
  for name in "${!SKILLS[@]}"; do
    src="${SKILLS[$name]}"
    link="$target/$name"

    if [[ -L "$link" ]]; then
      current=$(readlink "$link")
      if [[ "$current" == "$src" ]]; then
        verbose "up-to-date: $name"
        continue
      fi
      log "update: $name -> $src (was $current)"
      run rm "$link"
      run ln -s "$src" "$link"
      (( updated++ )) || true
    elif [[ -e "$link" ]]; then
      warn "skipping $name: $link exists and is not a symlink"
      (( skipped++ )) || true
    else
      log "create: $name -> $src"
      run ln -s "$src" "$link"
      (( created++ )) || true
    fi
  done

  echo
done

# --- Summary ---
echo "Done: $created created, $updated updated, $removed removed, $skipped skipped"
