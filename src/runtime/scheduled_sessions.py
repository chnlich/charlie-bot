"""Shared subtree membership and session behavior helpers."""

from __future__ import annotations

from collections.abc import Callable, Iterable

from src.infra.models import SessionMetadata
from src.runtime.hooks.sidebar_contributions import sidebar_contributions


def subtree_roots(
    metas: Iterable[SessionMetadata],
    is_root: Callable[[SessionMetadata], bool],
    *,
    include_root: bool,
) -> dict[str, str]:
  """Map every subtree row's id to the id of the root its chain reaches.

  The one parent-chain walk every subtree membership rule shares. A row belongs
  to a subtree when its ``task_parent_id`` chain (followed by id, whatever the
  ancestors' status) reaches a session answering *is_root*; *include_root*
  decides whether that root session itself is a member of its subtree. The two
  rules served today:

  - the sequence rule (``sequence_subtree_roots``): the root is owned by a
    registered sequence controller, and the root itself is not part of its subtree;
  - the view rule (``view_subtree_roots``): the root is a session a sidebar
    contribution names as the root of a view, and the root itself IS a member.

  The sidebar lists share this one walk, so each membership rule is
  implemented once.

  A chain member missing from *metas* (deleted or unreadable) ends that walk:
  the row classifies as unparented rather than guessing past the gap. When
  *include_root* is set, a start that is itself a root answers for itself
  without walking up — its own subtree outranks any ancestor's.
  """
  by_id = {meta.id: meta for meta in metas}
  roots: dict[str, str] = {}
  for start in by_id.values():
    chain: list[str] = []
    seen = {start.id}
    # An included root answers for itself without walking up: its own subtree
    # outranks any ancestor's.
    root: str | None = start.id if (include_root and is_root(start)) else None
    if root is None:
      parent_id = start.task_parent_id
      while parent_id is not None:
        if parent_id in roots:  # a memoized ancestor's verdict answers for this chain too
          root = roots[parent_id]
          break
        parent = by_id.get(parent_id)
        if parent is None or parent.id in seen:  # a cycle corrupts the relation; stop the walk
          break
        if is_root(parent):
          root = parent_id
          break
        seen.add(parent_id)
        chain.append(parent_id)
        parent_id = parent.task_parent_id
    if root is not None:
      members = [*chain, start.id]
      if include_root:
        members.append(root)
      for member_id in members:
        roots[member_id] = root
  return roots


def sequence_subtree_roots(metas: Iterable[SessionMetadata]) -> dict[str, str]:
  """Map every sequence-subtree row to the owned session above it."""
  from src.runtime.hooks.sequence_controllers import sequence_controllers

  controllers = sequence_controllers()
  return subtree_roots(
      metas,
      lambda meta: any(controller.owns_session(meta) for controller in controllers),
      include_root=False,
  )


def view_subtree_roots(metas: Iterable[SessionMetadata]) -> dict[str, dict[str, str]]:
  """Map each sidebar view's key to its subtree rows: row id to the id of the view's root above it.

  The view rule over :func:`subtree_roots`: a row is a view root when a sidebar
  contribution answers a view key for it (``SidebarContribution.view_member``),
  and a row belongs to that view's subtree when its parent chain reaches one of
  the view's roots. The root itself is a member of its subtree (the sidebar
  lists the view's roots with the rows below them). A view no row roots has no
  entry.
  """
  metas = list(metas)
  contributions = sidebar_contributions()
  root_ids: dict[str, set[str]] = {}
  for meta in metas:
    for contribution in contributions:
      key = contribution.view_member(meta)
      if key is not None:
        root_ids.setdefault(key, set()).add(meta.id)
  return {
      key: subtree_roots(metas, lambda meta, ids=ids: meta.id in ids, include_root=True)
      for key, ids in root_ids.items()
  }


class ScheduledSessionBusyError(RuntimeError):
  """Raised when a scheduled node's backend cannot switch in place because its own work is in flight."""
