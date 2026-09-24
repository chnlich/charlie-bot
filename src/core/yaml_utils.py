"""Shared YAML load/save helpers for consistent encoding and serialization."""

from pathlib import Path
from typing import Any

import yaml

# libyaml's C safe parser/emitter: the same safe subset SafeLoader/SafeDumper
# constructs (parity pinned by tests/test_yaml_utils.py), at ~1/9 the parse
# wall of the pure-Python pair. This venv's pyyaml builds with libyaml; a
# build without it fails this import loud rather than parsing slowly.
_SAFE_LOADER = yaml.CSafeLoader
_SAFE_DUMPER = yaml.CSafeDumper


def load_yaml_text(raw: str, *, default: Any) -> Any:
  """Parse one YAML document from *raw*. Returns *default* for an empty document."""
  data = yaml.load(raw, Loader=_SAFE_LOADER)
  return data if data is not None else default


def load_yaml(path: Path, *, default: Any) -> Any:
  """Read a YAML file with consistent UTF-8 encoding. Returns *default* if the file is missing or empty."""
  if not path.exists():
    return default
  return load_yaml_text(path.read_text(encoding="utf-8"), default=default)


def save_yaml(path: Path, data: Any) -> None:
  """Write data to a YAML file with consistent UTF-8 encoding and formatting."""
  path.write_text(
      yaml.dump(data, Dumper=_SAFE_DUMPER, allow_unicode=True, default_flow_style=False, sort_keys=False),
      encoding="utf-8")
