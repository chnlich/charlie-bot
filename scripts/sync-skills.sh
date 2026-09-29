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
  "$HOME/.charliebot/.claude/skills"  # Claude Code ancestor root: collected from every master session cwd, whatever the account
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

# --- Claude Code discovery check ---
# The ancestor root only helps if Claude Code actually collects it. The probe
# starts claude with an empty CLAUDE_CONFIG_DIR from the master session cwd:
# not logged in, it loads skills from the cwd ancestors first, writes the
# debug log, and fails authentication before any API call. K is the project
# count on the last "Loaded N unique skills" log line; N is the symlink count
# in the ancestor root. K must equal N.
if (( DRY_RUN )); then
  exit 0
fi

if ! command -v claude >/dev/null 2>&1; then
  echo "Claude discovery: skipped (claude not on PATH)"
  exit 0
fi

discovery_config=$(mktemp -d)
discovery_log=$(mktemp)
trap 'rm -rf "$discovery_config" "$discovery_log"' EXIT

# The probe cwd is the master session root; setup.sh runs this sync before
# init_charliebot_home provisions the home layout, so create it when absent.
mkdir -p "$HOME/.charliebot/sessions"
cd "$HOME/.charliebot/sessions"
CLAUDE_CONFIG_DIR="$discovery_config" timeout 60 claude -p --no-session-persistence --debug-file "$discovery_log" x >/dev/null 2>&1 || true

# grep misses must not abort under set -euo pipefail before the check reports.
loaded_line=$(grep -E 'Loaded [0-9]+ unique skills' "$discovery_log" | tail -n 1 || true)
loaded=$(sed -nE 's/.*project: ([0-9]+).*/\1/p' <<<"$loaded_line")
expected=$(find "$HOME/.charliebot/.claude/skills" -mindepth 1 -maxdepth 1 -type l | wc -l)

reproduce_cmd='cd $HOME/.charliebot/sessions && CLAUDE_CONFIG_DIR=$(mktemp -d) timeout 60 claude -p --no-session-persistence --debug-file <tmp-log> x'

if [[ -z "$loaded" ]]; then
  echo "Claude discovery: FAILED - no 'Loaded N unique skills (... project: K ...)' line in the debug log; reproduce with: $reproduce_cmd" >&2
  exit 1
fi

echo "Claude discovery: $loaded/$expected"

if (( loaded != expected )); then
  echo "Claude discovery: FAILED - claude loaded $loaded of the $expected ancestor-root skills; reproduce with: $reproduce_cmd" >&2
  exit 1
fi
