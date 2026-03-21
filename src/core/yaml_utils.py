"""Shared YAML load/save helpers for consistent encoding and serialization."""

from pathlib import Path
from typing import Any

import yaml


def load_yaml(path: Path, *, default: Any = None) -> Any:
  """Read a YAML file with consistent UTF-8 encoding. Returns *default* if the file is missing or empty."""
  if not path.exists():
    return default
  data = yaml.safe_load(path.read_text(encoding='utf-8'))
  return data if data is not None else default


def save_yaml(path: Path, data: Any, *, sort_keys: bool = False) -> None:
  """Write data to a YAML file with consistent UTF-8 encoding and formatting."""
  path.write_text(
      yaml.safe_dump(data, allow_unicode=True, default_flow_style=False, sort_keys=sort_keys), encoding='utf-8')
