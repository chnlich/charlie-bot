#!/usr/bin/env bash
# The Python guard: fail on tracked files that bring Python back into the repository. CI runs it in the
# frontend-test job, before the installs; run it by hand from anywhere inside the checkout.
#
# Four categories fail:
# - a tracked .py file;
# - a python shebang, the first line of a tracked file;
# - a line in a shell script, a git hook or a CI configuration that names python;
# - a line in prompts, skills or docs that names python. Prose names the language with a capital P, so a lowercase
#   `python` marks a command.
set -euo pipefail

# The script's own text names python in its rules; it runs none, so the line scans skip its path.
SELF=dev/check-no-python.sh

cd "$(git rev-parse --show-toplevel)"

status=0

# One entry per path whose lines stay, with the reason. Keep the list as short as the tree allows.
EXEMPT_PATHS=(
  'README.md' # quotes the emergency rollback command of the last Python-server commit; that script and its mode exist only there
  'prompts/task_base.md' # the recorded prompt snapshot asserts on this text byte for byte (tasks/tests/fixtures)
  'skills/cli-tool-traps/SKILL.md' # quotes a recorded runaway process's argv as the pgrep anchoring example
)

exempt() {
  local file=$1 known
  for known in "${EXEMPT_PATHS[@]}"; do
    [[ "$file" == "$known" ]] && return 0
  done
  return 1
}

py_files=$(git ls-files '*.py')
if [[ -n "$py_files" ]]; then
  printf 'Python files:\n%s\n' "$py_files" >&2
  status=1
fi

# git grep narrows the candidates to text files that name python anywhere; the first-line test keeps the shebang
# category to actual shebangs.
rc=0
candidates=$(git grep -Il -E '^#!.*python' -- .) || rc=$?
if (( rc > 1 )); then exit "$rc"; fi
while IFS= read -r file; do
  [[ -n "$file" ]] || continue
  first_line=$(head -n 1 -- "$file")
  if [[ "$first_line" == '#!'*python* ]]; then
    printf 'Python shebang in %s: %s\n' "$file" "$first_line" >&2
    status=1
  fi
done <<< "$candidates"

# One git grep per category; each output line carries its path, so the exemption and self-path filters read the path
# off the line.
scan() {
  local label=$1; shift
  local rc=0 output line file
  output=$(git grep -nI -E '\bpython3?\b' -- "$@") || rc=$?
  if (( rc > 1 )); then exit "$rc"; fi
  local offending=()
  while IFS= read -r line; do
    [[ -n "$line" ]] || continue
    file=${line%%:*}
    content=${line#*:*:}
    [[ "$file" == "$SELF" ]] && continue
    # The CI step that runs the guard names its path; that line runs the guard, not python.
    [[ "$content" =~ ^[[:space:]]*"run:"[[:space:]]*"$SELF"[[:space:]]*$ ]] && continue
    exempt "$file" && continue
    offending+=("$line")
  done <<< "$output"
  if ((${#offending[@]} > 0)); then
    printf '%s:\n' "$label" >&2
    printf '  %s\n' "${offending[@]}" >&2
    status=1
  fi
}

scan 'Lines naming python in shell scripts, git hooks and CI configuration:' -- '*.sh' .github dev/git-hooks
scan 'Lines naming python in prompts, skills and docs:' -- prompts skills docs '*.md'

exit "$status"
