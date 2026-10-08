"""The memory store's root: one derivation for the CLI, the run instructions and the diff view."""

from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
  from src.infra.config import CharlieBotConfig


def store_root(home: Path) -> Path:
  """The store root under a profile home: ``<home>/memory`` (``~/.charliebot/memory/``)."""
  return home / "memory"


def memory_dir(cfg: CharlieBotConfig) -> Path:
  """The store root of cfg's profile home; the diff view also serves the repository under it."""
  return store_root(cfg.charliebot_home)
