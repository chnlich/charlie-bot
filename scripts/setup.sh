#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

DRY_RUN=0

usage() {
  echo "Usage: $0 [-n|--dry-run]"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -n|--dry-run)
      DRY_RUN=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 1
      ;;
  esac
done

cd "$REPO_ROOT"

echo "==> Syncing skills"
if (( DRY_RUN )); then
  "$SCRIPT_DIR/sync-skills.sh" -n
else
  "$SCRIPT_DIR/sync-skills.sh"
fi

echo "==> Checking Claude Code backend tools"
python - <<'PY'
from src.agents.backends.claude_code import BASE_COMMAND

required = ["Monitor", "ScheduleWakeup", "CronCreate", "CronDelete", "CronList"]

try:
  disallowed_index = BASE_COMMAND.index("--disallowed-tools")
except ValueError as exc:
  raise SystemExit("missing --disallowed-tools in BASE_COMMAND") from exc

try:
  disallowed_tools = set(BASE_COMMAND[disallowed_index + 1].split(","))
except IndexError as exc:
  raise SystemExit("--disallowed-tools has no value in BASE_COMMAND") from exc

missing = set(required) - disallowed_tools
if missing:
  raise SystemExit(f"missing disallowed tools: {','.join(sorted(missing))}")

print("OK: backend disallows " + ",".join(required))
PY

echo "Setup complete."
