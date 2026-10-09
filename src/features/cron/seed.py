"""Seed repo-owned default cron tasks into the host's per-job cron files."""

import copy

from src.features.cron import loader
from src.features.cron.config import ScheduledTaskConfig
from src.features.cron.cron_sequence import effective_scheduled_task_backend
from src.features.cron.loader import get_scheduled_tasks
from src.infra import config, yaml_utils


def seed_default_cron_tasks(cfg: config.CharlieBotConfig, *, dry_run: bool = False) -> list[dict]:
  """Seed repo-owned default cron tasks into per-job host files by name.

  Reads ``configs/cron.default.yaml`` from the repo and the host
  ``~/.charliebot/config.d/cron.d/`` directory. For each default entry: if no
  host file ``cron.d/<name>.yaml`` exists, create it with the entry body minus
  ``name``, keeping the entry's ``prompt_file`` pointer intact (the pointed
  file owns the prompt body and the host file carries only the path to it);
  if one exists, change nothing about it. Never rewrites or drops host-only
  files.

  With ``dry_run`` the same validation runs, nothing is written or created,
  and a would-be seed reports ``would-create`` — the preview
  ``./scripts/setup.sh -n`` shows must fail exactly where the real run would.

  Creates ``config.d/cron.d/`` when absent. Writes through
  :func:`src.infra.yaml_utils.save_yaml` (the same writer ``src/features/cron/api.py``
  uses). Validates every default entry before writing: each must construct a
  :class:`ScheduledTaskConfig` after ``prompt_file``/``local`` resolution (an
  entry with ``steps`` resolves each step's ``prompt_file`` exactly like the
  task-level pointer) and every ``prompt_file`` must resolve to an existing
  file. Fails loudly without writing if validation fails.

  Returns a per-entry report: ``[{"name": str, "status": "created"|"exists"}]``
  (``dry_run`` reports ``would-create`` instead of ``created``).

  This is a library function invoked only by :func:`setup_step`, which ``./scripts/setup.sh`` runs. The server
  startup path (:func:`src.runtime.init_seed.init_charliebot_home`) never calls
  it, so the server never writes cron config — that is an invariant the tests
  assert directly.
  """
  repo_root = cfg.charlie_bot_repo
  defaults_data = yaml_utils.load_yaml(repo_root / "configs" / "cron.default.yaml", default={})
  default_entries = list(defaults_data.get("scheduled_tasks", []))

  # Validate every default entry on a resolved copy before touching the host
  # directory, so a bad repo default fails loudly without any partial write.
  for entry in default_entries:
    if not isinstance(entry, dict):
      raise ValueError(f"invalid default cron entry (not a mapping): {entry!r}")
    resolved = copy.deepcopy(entry)
    resolved.pop("name", None)
    loader._resolve_prompt_file(resolved, repo_root)  # raises ValueError
    for step in resolved.get("steps") or []:
      if isinstance(step, dict):
        loader._resolve_prompt_file(step, repo_root)  # raises ValueError
    loader._resolve_local_timezone(resolved)
    ScheduledTaskConfig(name=entry.get("name"), **resolved)  # raises on validation error

  cron_d_dir = cfg.config_d_dir / "cron.d"
  if not dry_run:
    cron_d_dir.mkdir(parents=True, exist_ok=True)

  report: list[dict] = []
  for entry in default_entries:
    name = entry.get("name")
    path = cron_d_dir / f"{name}.yaml"
    if path.exists():
      report.append({"name": name, "status": "exists"})
      continue
    if dry_run:
      report.append({"name": name, "status": "would-create"})
      continue
    body = {k: v for k, v in copy.deepcopy(entry).items() if k != "name"}
    # Persist the pointer unchanged: the pointed file owns the prompt body,
    # and this host file carries only its path.
    yaml_utils.save_yaml(path, body)
    report.append({"name": name, "status": "created"})
  return report


def setup_step(cfg: config.CharlieBotConfig, *, dry_run: bool) -> None:
  """Seed the default cron tasks, then list the effective scheduled tasks. Registered with ``register_setup_step``."""
  print("==> Seeding default cron tasks")
  # Per-task created/exists for repo-default cron entries, keyed on whether the
  # per-job host file config.d/cron.d/<name>.yaml exists. The dry-run runs the
  # same validation and legacy tripwire as the real run and writes nothing, so
  # the preview fails exactly where the real run would.
  for item in seed_default_cron_tasks(cfg, dry_run=dry_run):
    print(f"  cron {item['name']}: {item['status']}")

  # Effective scheduled task list: name / cron / resolved timezone / resolved
  # backend. If backend resolution raises (e.g. empty backends.options on a fresh
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
