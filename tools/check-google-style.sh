#!/usr/bin/env bash
set -euo pipefail

# Repo root from the script's own location, not the cwd: the probe must run
# from anywhere (the code-health cron and developers alike).
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && git rev-parse --show-toplevel)
cd "$repo_root"

# The probe's scope. The list is literal on purpose -- a new top-level path
# joins it as a review-visible act.
default_paths=(src tests tools scripts server.py)

paths=()
if [[ $# -eq 0 ]]; then
  for path in "${default_paths[@]}"; do
    if [[ -e "$path" ]]; then
      paths+=("$path")
    fi
  done
else
  paths=("$@")
fi

# `uv run` puts no cwd on sys.path, so pylint's resolver cannot see src.*,
# tools.* or conftest from the cwd alone and would fire W9001 on every
# project import. Root goes first; a caller's PYTHONPATH follows.
export PYTHONPATH="$repo_root${PYTHONPATH:+:$PYTHONPATH}"

# pylint's exit code is a bitmask of finding classes: 2 error, 4 warning,
# 8 refactor, 16 convention. The probe enables only those classes, so a run
# with findings exits inside the mask 30; 1 (fatal) and 32 (usage error)
# mean the run itself broke and must fail loudly instead of counting as
# zero. yapf instead exits 1 when diffs exist -- its normal finding case --
# so its mask is 1. The status must be read in the else branch: after a
# failed if condition with no else, $? reads 0 and the real code is lost.
run_capture() {
  local mask=$1
  shift
  local output
  local status
  if output=$("$@" 2>&1); then
    printf '%s' "$output"
    return 0
  else
    status=$?
  fi
  if (( (status & mask) == status )); then
    printf '%s' "$output"
    return 0
  fi
  printf '%s\n' "$output" >&2
  return "$status"
}

yapf_output=$(run_capture 1 uv run yapf --diff --recursive "${paths[@]}")
pylint_output=$(run_capture 30 uv run pylint --score=n "${paths[@]}")

if [[ -n "$yapf_output" ]]; then
  printf '%s\n' "$yapf_output"
fi
if [[ -n "$pylint_output" ]]; then
  printf '%s\n' "$pylint_output"
fi

# One file per unified-diff header; yapf marks the reformatted side.
reformat_count=$(printf '%s\n' "$yapf_output" | grep -cE '^\+\+\+ .*\(reformatted\)$') || true
# Every finding prints one line of the form path:line:col: C9001: ... -- the
# code letter varies (C/W/E/R) and each line counts, W9001 included.
finding_count=$(printf '%s\n' "$pylint_output" | grep -cE '^[^:]+:[0-9]+:[0-9]+: [A-Z][0-9]{4}: ') || true

printf 'google-style: %d files to reformat, %d import findings\n' "$reformat_count" "$finding_count"

if [[ "$reformat_count" -eq 0 && "$finding_count" -eq 0 ]]; then
  exit 0
fi
exit 1
