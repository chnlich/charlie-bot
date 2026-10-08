"""Sidebar contributions: what a feature package adds to sidebar rows and to session housekeeping.

The session store reads and writes sessions for every feature, so it names none of them. A package
registers one ``SidebarContribution`` from its ``register()``; the store asks ``sidebar_contributions()``
for the list at four moments:

- the sidebar probe calls ``row_flags`` for each session it re-reads, and stats each ``watched_files``
  entry for the probe's change signature;
- a fork or an elone calls ``copy_on_fork`` once the child's directory exists;
- dropping a session's runtime state calls ``drop_runtime_state``;
- the sidebar listings call ``view_member`` for each row to find the roots of a feature's view.

A registered target is a "module:attr" string imported on first use, so registering costs no feature
import. This module imports no feature module.
"""

from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING

from src.runtime.hooks.backend_lifecycle import import_attr

if TYPE_CHECKING:
  from src.infra.models import SessionMetadata


class SidebarContribution:
  """Base class. Every method has a default that adds nothing."""

  # Paths relative to the session directory that row_flags reads. The probe re-runs row_flags only
  # when one of these files, or another probe input, changed.
  watched_files: tuple[str, ...] = ()

  def row_flags(self, session_dir: Path, session_id: str) -> dict[str, bool]:
    """The sidebar flags this feature sets on one session's row, by flag name. Must not raise for a corrupt file."""
    return {}

  def copy_on_fork(self, parent_dir: Path, child_dir: Path) -> None:
    """Copy this feature's files from the parent's session directory into the child's."""

  def drop_runtime_state(self, session_id: str) -> None:
    """Drop this feature's in-memory state for one session."""

  def view_member(self, meta: SessionMetadata) -> str | None:
    """The key of the sidebar view whose root *meta* is, or None. A view lists its roots and every row below them."""
    return None


_registered: dict[str, str | SidebarContribution] = {}  # name -> module path or contribution
_resolved: tuple[SidebarContribution, ...] | None = None


def register_sidebar_contribution(name: str, contribution: str | SidebarContribution) -> None:
  """*contribution* is "module:attr", a SidebarContribution instance, imported on first use.

  A second registration of one name raises ValueError.
  """
  global _resolved
  if name in _registered:
    raise ValueError(f"sidebar contribution {name!r} is already registered by {_registered[name]}")
  _registered[name] = contribution
  _resolved = None


def sidebar_contributions() -> tuple[SidebarContribution, ...]:
  """The registered contributions, imported, in registration order."""
  global _resolved
  if _resolved is None:
    _resolved = tuple(import_attr(target) if isinstance(target, str) else target for target in _registered.values())
  return _resolved
