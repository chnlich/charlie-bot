"""Small writes to per-task cron configuration files."""

from src.infra.config import cron_path
from src.infra.yaml_utils import load_yaml, save_yaml


def write_cron_key(task_name: str, key: str, value: str | bool) -> None:
  """Write one key of *task_name*'s cron yaml, preserving every other key.

  Single home of the single-key write rule: full-file rewrite via save_yaml —
  the same persistence form as the cron editor's whole-record update. Path
  resolution comes from the canonical helper (src.infra.config.cron_path); a
  missing, empty, or non-mapping task file fails loud instead of silently
  recreating one.
  """
  path = cron_path(task_name)
  data = load_yaml(path, default=None)
  if not isinstance(data, dict):
    raise FileNotFoundError(f"scheduled task '{task_name}' has no readable cron yaml at {path}")
  data[key] = value
  save_yaml(path, data)
