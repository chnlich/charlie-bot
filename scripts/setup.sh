#!/usr/bin/env bash
set -euo pipefail

# Resolve paths relative to this script so setup works from any current directory.
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

DRY_RUN=0

usage() {
  echo "Usage: $0 [-n|--dry-run]"
}

# Parse setup flags. Dry-run keeps the flow read-only and skips skill writes.
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

# Sync shared and host-specific skills into backend skill directories.
echo "==> Syncing skills"
if (( DRY_RUN )); then
  "$SCRIPT_DIR/sync-skills.sh" -n
else
  "$SCRIPT_DIR/sync-skills.sh"
fi

# Provision ~/.charliebot/ and seed repo-default cron tasks. init_charliebot_home
# provisions the home layout (dirs, memory files, config.yaml) and is the same
# path the server runs at startup; seed_default_cron_tasks is the ONLY writer of
# cron.yaml and is never called from the server startup path — so running setup
# is the only way repo-default cron skeletons reach the host.
echo "==> Provisioning ~/.charliebot and seeding default cron tasks"
DRY_RUN_VAL=$DRY_RUN uv run python - <<'PY'
import asyncio
import os

from src.core.config import get_config, get_scheduled_tasks
from src.core.init import init_charliebot_home, seed_default_cron_tasks
from src.core.scheduler import effective_scheduled_task_backend
from src.core.yaml_utils import load_yaml

dry = os.environ.get("DRY_RUN_VAL") == "1"
cfg = get_config()

# Per-item created/exists for the home layout. init_charliebot_home() is the
# single source of truth for this layout; in dry-run we only report what already
# exists vs what would be created, and write nothing.
home_items = [
    ("dir", "~/.charliebot/", cfg.charliebot_home),
    ("dir", "~/.charliebot/sessions/", cfg.sessions_dir),
    ("dir", "~/.charliebot/config.d/", cfg.config_d_dir),
    ("file", "~/.charliebot/config.yaml", cfg.config_file),
    ("file", "~/.charliebot/MEMORY.md", cfg.memory_file),
    ("file", "~/.charliebot/MEMORY.host.md", cfg.memory_host_file),
    ("file", "~/.charliebot/MEMORY.tmp.md", cfg.memory_tmp_file),
    ("file", "~/.charliebot/slash_commands.yaml", cfg.charliebot_home / "slash_commands.yaml"),
]
existed_before = {str(p): p.exists() for _, _, p in home_items}
if not dry:
    asyncio.run(init_charliebot_home())
for label, path in [(lbl, p) for _, lbl, p in home_items]:
    now_exists = path.exists()
    if dry:
        status = "exists" if now_exists else "would-create"
    else:
        status = "exists" if existed_before[str(path)] else "created"
    print(f"  home {label}: {status}")

# Per-task created/exists for repo-default cron entries. In dry-run, compute
# what would be seeded from the repo defaults vs the host file without writing.
repo_root = cfg.charlie_bot_repo
defaults = (load_yaml(repo_root / "configs" / "cron.default.yaml", default={})
            .get("scheduled_tasks", []) or [])
cron_path = cfg.config_d_dir / "cron.yaml"
host_data = load_yaml(cron_path, default={"scheduled_tasks": []}) or {}
host_names = {t.get("name") for t in (host_data.get("scheduled_tasks", []) or [])
              if isinstance(t, dict)}
if dry:
    for entry in defaults:
        name = entry.get("name")
        status = "exists" if name in host_names else "would-create"
        print(f"  cron {name}: {status}")
else:
    for item in seed_default_cron_tasks(cfg):
        print(f"  cron {item['name']}: {item['status']}")

# Effective scheduled task list: name / cron / resolved timezone / resolved
# backend. If backend resolution raises (e.g. empty backend_options on a fresh
# host), print the reason instead of aborting setup.
print("  effective scheduled tasks:")
tasks = get_scheduled_tasks()
if not tasks:
    print("    (none)")
for t in tasks:
    try:
        backend = effective_scheduled_task_backend(t, cfg)
    except Exception as e:
        backend = f"unresolved: {e}"
    print(f"    - {t.name} | cron={t.cron} | tz={t.timezone} | backend={backend}")
PY
echo "  Reminder: fill in secret keys gemini_api_key and charliebot_access_key before first start."

# Smoke-check the Claude Code backend command for headless-unsafe tools.
echo "==> Checking Claude Code backend tools"
uv run python - <<'PY'
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
