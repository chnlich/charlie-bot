"""Scheduled-session read paths, the cron-yaml single-key write, and the
scheduler-bookkeeping migration the auto-bind uses.

No scheduled task creates a dedicated cron session: every scheduled task binds
a task-tree manager node (the scheduler's auto-bind); sessions that still carry
a ``scheduled_task`` stamp are the legacy ones, archived at migration.
What remains here serves them: the cron-subtree membership rule the sidebar
lists share, the one field list a migration copies, and the one write rule that
changes a single cron-yaml key.
"""

from __future__ import annotations

from collections.abc import Iterable

from src.core.config import cron_path
from src.core.models import SessionMetadata
from src.core.yaml_utils import load_yaml, save_yaml


def cron_subtree_roots(metas: Iterable[SessionMetadata]) -> dict[str, str]:
  """Map every cron-subtree row's id to the id of the cron session above it.

  A row belongs to one scheduled task's cron subtree when its ``task_parent_id``
  chain (followed by id, whatever the ancestors' status) reaches a session whose
  ``scheduled_task`` is set; the cron session itself is not part of its subtree.
  Projected legacy worker-thread rows carry ``task_parent_id`` = their parent
  session, so the same walk classifies them. The sidebar lists share this one
  walk, so the membership rule is implemented once.

  A chain member missing from *metas* (deleted or unreadable) ends that walk:
  the row classifies as unparented rather than guessing past the gap.
  """
  by_id = {meta.id: meta for meta in metas}
  roots: dict[str, str] = {}
  for start in by_id.values():
    chain: list[str] = []
    seen = {start.id}
    root: str | None = None
    parent_id = start.task_parent_id
    while parent_id is not None:
      if parent_id in roots:  # a memoized ancestor's verdict answers for this chain too
        root = roots[parent_id]
        break
      parent = by_id.get(parent_id)
      if parent is None or parent.id in seen:  # a cycle corrupts the relation; stop the walk
        break
      if parent.scheduled_task is not None:
        root = parent_id
        break
      seen.add(parent_id)
      chain.append(parent_id)
      parent_id = parent.task_parent_id
    if root is not None:
      for member_id in [*chain, start.id]:
        roots[member_id] = root
  return roots


class ScheduledSessionBusyError(RuntimeError):
  """Raised when a scheduled node's backend cannot switch in place because its
  own work is in flight."""


def migrate_scheduler_bookkeeping(old_session: SessionMetadata, new_session: SessionMetadata) -> None:
  """Carry scheduler bookkeeping fields from *old_session* onto *new_session*.

  The single home of the migration field list: the auto-bind's migration step
  (src/core/scheduler.py) calls it, so what migrates onto a task's node is
  defined exactly once.
  """
  new_session.last_scheduled_run = old_session.last_scheduled_run
  new_session.last_scheduled_cron = old_session.last_scheduled_cron
  new_session.last_run_status = old_session.last_run_status


def write_cron_key(task_name: str, key: str, value: str | bool) -> None:
  """Write one key of *task_name*'s cron yaml, preserving every other key.

  Single home of the single-key write rule: full-file rewrite via save_yaml —
  the same persistence form as the cron editor's whole-record update. Path
  resolution comes from the canonical helper (src.core.config.cron_path); a
  missing, empty, or non-mapping task file fails loud instead of silently
  recreating one.
  """
  path = cron_path(task_name)
  data = load_yaml(path, default=None)
  if not isinstance(data, dict):
    raise FileNotFoundError(f"scheduled task '{task_name}' has no readable cron yaml at {path}")
  data[key] = value
  save_yaml(path, data)
