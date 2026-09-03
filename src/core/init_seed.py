"""Seed and first-run initialization of the ~/.charliebot/ directory structure."""

import copy
import subprocess
from pathlib import Path

import yaml

from src.core.config import (
    CharlieBotConfig,
    ScheduledTaskConfig,
    _resolve_local_timezone,
    _resolve_prompt_file,
    get_config,
)
from src.core.yaml_utils import load_yaml, save_yaml


def _default_config_yaml() -> dict:
  """Build the default config dict with placeholder values."""
  return {
      "workspace_dirs": ["~/workspace"],
      "worktree_dir": "~/worktrees",
  }


DEFAULT_MEMORY_TOPICS = (
    "profile resident\n"
    "communication resident\n"
    "workflow resident\n"
    "rulings resident\n"
    "host resident\n"
    "charliebot\n")

DEFAULT_MEMORY_GITIGNORE = "staging/\n"

DEFAULT_SLASH_COMMANDS = """\
commands:
  # Initially empty — /help is built-in, not defined here.
  #
  # Example shell command:
  # git:
  #   scope: shell
  #   description: "Run git command"
  #   args: "<git args>"
  #   command: "git {args}"
  #   cwd: "/path/to/repo"
  #   timeout: 10
  #
  # Example prompt command:
  # summarize:
  #   scope: prompt
  #   description: "Summarize conversation"
  #   prompt: "Summarize our conversation in bullet points."
  #
  # Example prompt command with claude_code_flags (runs in plan-only mode):
  # plan:
  #   scope: prompt
  #   description: 'Plan without implementing'
  #   args: '<what to plan>'
  #   prompt: '{args}'
  #   claude_code_flags: ['--permission-mode', 'plan']
"""


async def init_charliebot_home() -> None:
  """Ensure ~/.charliebot/ directory structure exists and seed default files."""
  cfg = get_config()

  # Create all required directories
  dirs = [
      cfg.charliebot_home,
      cfg.sessions_dir,
      cfg.config_d_dir,
  ]
  for d in dirs:
    d.mkdir(parents=True, exist_ok=True)

  # Seed the memory store scaffold (git repo + topics vocabulary + .gitignore)
  _seed_memory_scaffold(cfg)
  _seed_if_missing(cfg.charliebot_home / 'slash_commands.yaml', DEFAULT_SLASH_COMMANDS)

  # Seed config.yaml from the committed template if missing
  if not cfg.config_file.exists():
    template = cfg.charlie_bot_repo / "configs" / "config.example.yaml"
    if template.exists():
      cfg.config_file.write_text(template.read_text(encoding="utf-8"), encoding="utf-8")
    else:
      with open(cfg.config_file, "w") as f:
        yaml.dump(_default_config_yaml(), f, default_flow_style=False, sort_keys=False)


def _seed_if_missing(path: Path, content: str) -> None:
  """Write content to path only if the file does not already exist."""
  if not path.exists():
    path.write_text(content, encoding="utf-8")


def _seed_memory_scaffold(cfg: CharlieBotConfig) -> None:
  """Seed the labeled-entry memory store at cfg.memory_dir (idempotent).

  Creates the directory, runs ``git init`` when it is not already a repo, seeds
  the topics vocabulary and .gitignore (never overwriting existing files), and
  creates the ``entries/`` and ``staging/`` directories. The canon (entries/
  and topics) is populated only by user-approved curation diffs, never here.
  """
  memory_dir = cfg.memory_dir
  memory_dir.mkdir(parents=True, exist_ok=True)
  if not (memory_dir / ".git").exists():
    subprocess.run(["git", "init"], cwd=str(memory_dir), check=True, capture_output=True)
  _seed_if_missing(memory_dir / "topics", DEFAULT_MEMORY_TOPICS)
  _seed_if_missing(memory_dir / ".gitignore", DEFAULT_MEMORY_GITIGNORE)
  (memory_dir / "entries").mkdir(exist_ok=True)
  (memory_dir / "staging").mkdir(exist_ok=True)


def seed_default_cron_tasks(cfg: CharlieBotConfig) -> list[dict]:
  """Seed repo-owned default cron tasks into per-job host files by name.

  Reads ``configs/cron.default.yaml`` from the repo and the host
  ``~/.charliebot/config.d/cron.d/`` directory. For each default entry: if no
  host file ``cron.d/<name>.yaml`` exists, create it with the entry body minus
  ``name``, keeping the entry's ``prompt_file`` pointer intact (the pointed
  file owns the prompt body and the host file carries only the path to it);
  if one exists, change nothing about it. Never rewrites or drops host-only
  files.

  Creates ``config.d/cron.d/`` when absent. Writes through
  :func:`src.core.yaml_utils.save_yaml` (the same writer ``src/api/cron.py``
  uses). Validates every default entry before writing: each must construct a
  :class:`ScheduledTaskConfig` after ``prompt_file``/``local`` resolution (an
  entry with ``steps`` resolves each step's ``prompt_file`` exactly like the
  task-level pointer) and every ``prompt_file`` must resolve to an existing
  file. Fails loudly without writing if validation fails, and fails loudly
  (writing nothing) when a legacy ``config.d/cron.yaml`` exists — migration to
  the per-job layout is a human action, never automatic.

  Returns a per-entry report: ``[{"name": str, "status": "created"|"exists"}, ...]``.

  This is a library function invoked only by ``./scripts/setup.sh``. It is NOT
  called by :func:`init_charliebot_home`, so the server startup path never
  writes cron config — that is an invariant the tests assert directly.
  """
  repo_root = cfg.charlie_bot_repo
  defaults_data = load_yaml(repo_root / "configs" / "cron.default.yaml", default={})
  default_entries = list(defaults_data.get("scheduled_tasks", []))

  # Validate every default entry on a resolved copy before touching the host
  # directory, so a bad repo default fails loudly without any partial write.
  for entry in default_entries:
    if not isinstance(entry, dict):
      raise ValueError(f"invalid default cron entry (not a mapping): {entry!r}")
    resolved = copy.deepcopy(entry)
    resolved.pop("name", None)
    _resolve_prompt_file(resolved, repo_root)  # raises ValueError
    for step in resolved.get("steps") or []:
      if isinstance(step, dict):
        _resolve_prompt_file(step, repo_root)  # raises ValueError
    _resolve_local_timezone(resolved)
    ScheduledTaskConfig(name=entry.get("name"), **resolved)  # raises on validation error

  # A leftover legacy cron.yaml is a loud tripwire, never a silent fallback:
  # refuse to seed (and write nothing) until a human migrates and removes it.
  legacy_path = cfg.config_d_dir / "cron.yaml"
  if legacy_path.exists():
    raise ValueError(
        f"legacy {legacy_path} present; not seeding. Split its entries into "
        f"config.d/cron.d/<name>.yaml and remove cron.yaml (migration is manual)")

  cron_d_dir = cfg.config_d_dir / "cron.d"
  cron_d_dir.mkdir(parents=True, exist_ok=True)

  report: list[dict] = []
  for entry in default_entries:
    name = entry.get("name")
    path = cron_d_dir / f"{name}.yaml"
    if path.exists():
      report.append({"name": name, "status": "exists"})
      continue
    body = {k: v for k, v in copy.deepcopy(entry).items() if k != "name"}
    # Persist the pointer unchanged: the pointed file owns the prompt body,
    # and this host file carries only its path.
    save_yaml(path, body)
    report.append({"name": name, "status": "created"})
  return report
