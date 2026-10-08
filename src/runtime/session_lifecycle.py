"""Session lifecycle: rename, backend switch, read marks, archive, permanent delete, history recycling, stars, groups.

``SessionLifecycle`` holds the writes that change a session's own metadata or files after it exists. Each write
runs under the session's metadata lock (through ``SessionStore.update_field`` or directly) and tells the sidebar
through the events block. The task-tree owner registers ``tree_index_invalidator``; the writes that move a
tree-projection input call it. The process builds one block (``lifecycle()``); tests build their own and install it
with ``set_lifecycle()``.
"""

import asyncio
import shutil
from collections.abc import Callable
from datetime import datetime

from src.infra import event_types as ET
from src.infra.config import CharlieBotConfig, get_config
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import SessionMetadata, SessionStatus, utc_now
from src.infra.process import cleanup_session_cgroup
from src.runtime import session_events, session_store, sidebar_state

log = LazyStructlogLogger()


class SessionLifecycle:
  """Session renames, archive state, deletion, history recycling, stars and groups over the store and events blocks."""

  def __init__(
      self,
      cfg: CharlieBotConfig,
      store: session_store.SessionStore,
      events: session_events.SessionEvents,
  ) -> None:
    self._cfg = cfg
    self._store = store
    self._events = events
    # The task-tree owner registers its projection invalidator here at wiring
    # time (TaskTreeManager.__init__). Lifecycle writes that move a
    # tree-projection input must drop the tree's rebuildable index — the same
    # policy the task owner applies to its own metadata writes; None only
    # before that wiring exists (no tree consumer constructed in this process).
    self.tree_index_invalidator: Callable[[], None] | None = None

  async def rename_session(self, session_id: str, new_name: str) -> SessionMetadata | None:
    """Rename a session and return the updated metadata."""
    return await self._store.update_field(session_id, "name", new_name, "session_renamed", new_name=new_name)

  async def switch_backend(self, session_id: str, backend: str) -> SessionMetadata | None:
    """Set the session's backend and return the updated metadata.

    Performs only the metadata write — the caller (API layer) is responsible
    for resume-domain validation and for persisting the audit event. Returns
    ``None`` when the session is missing. Broadcasts a sidebar update so other
    open tabs refresh their header.
    """
    meta = await self._store.update_field(session_id, "backend", backend, "session_backend_switched", backend=backend)
    if meta:
      await self._events.broadcast_sidebar(session_id, ET.BACKEND_SWITCHED, backend=backend)
    return meta

  async def mark_read(self, session_id: str) -> SessionMetadata | None:
    """Clear the unread flag for a session."""
    return await self._set_unread_flag(session_id, has_unread=False)

  async def mark_unread(self, session_id: str) -> None:
    """Set the unread flag for a session (called when master/workers produce output)."""
    await self._set_unread_flag(session_id, has_unread=True)

  async def _set_unread_flag(self, session_id: str, has_unread: bool) -> SessionMetadata | None:
    """Write the unread flag and broadcast only when it actually flips.

    Unlike ``SessionStore.update_field`` this must not bump ``updated_at``: the sidebar
    sorts newest-first on that field, and a read/unread flip is not user
    activity worth reordering the list over.
    """
    async with self._store.lock_for(session_id):
      meta = await self._store.get_session(session_id)
      if not meta or meta.has_unread == has_unread:
        return meta
      meta.has_unread = has_unread
      await self._store.save_metadata(meta, lock_held=True)
      # The unread flag is a tree-row projection input (SessionRow.has_unread):
      # drop the rebuildable index so a tree page read after this flip is built
      # from the fresh flag, not from a pre-flip index snapshot.
      if self.tree_index_invalidator is not None:
        self.tree_index_invalidator()
    await self._events.broadcast_sidebar(session_id, ET.UNREAD_CHANGED, has_unread=has_unread)
    return meta

  async def archive_session(self, session_id: str) -> SessionMetadata | None:
    """Mark a session as archived (does not delete files).

    The stored status is a tree-projection input: an archived parent archives
    its task subtree through the read-time inheritance, so the write
    drops the rebuildable index through the same hook the unread flip uses:
    without it, a listing within _TREE_INDEX_TTL_SECONDS would compute the
    children's inherited state from the parent's pre-archive status.
    """
    meta = await self._store.update_field(session_id, "status", SessionStatus.ARCHIVED, "session_archived")
    self._events.drop_session_runtime_state(session_id)
    if meta is not None and self.tree_index_invalidator is not None:
      self.tree_index_invalidator()
    return meta

  async def delete_session_permanently(self, session_id: str) -> bool:
    """Permanently delete a session and all its data from disk."""
    async with self._store.lock_for(session_id):
      session_dir = self._store.session_dir(session_id)
      if not session_dir.exists():
        return False
      await asyncio.to_thread(shutil.rmtree, session_dir)
      # The session's memory-cap cgroup: removed only when empty (no live
      # member left); a retained directory is logged debug and reclaimed by
      # the kernel once its last process exits.
      await asyncio.to_thread(cleanup_session_cgroup, session_id)
      self._events.drop_session_runtime_state(session_id)
      self._store.invalidate_cache(session_id)
      # The sidebar's whole-body memo keys on (requested ids, generation), so a
      # deletion must bump the generation or a poll still carrying the deleted
      # id serves the ghost row. The mark is never consumed — the fold probes
      # resolved sessions only — and that is fine; the bump is the point.
      sidebar_state.mark_sidebar_dirty(session_id)
      # Popping the lock from the dict while holding it is safe: the popped lock
      # object stays valid for this holder until the ``async with`` exits.
      self._store.metadata_locks.pop(session_id, None)
    log.info("session_deleted_permanently", session_id=session_id)
    return True

  async def unarchive_session(self, session_id: str) -> SessionMetadata | None:
    """Restore an archived session back to active.

    The status flip moves the tree projection the same way the archive write
    does (descendants inherit the ancestor's restored visibility), so the
    rebuildable index drops through the same hook.
    """
    meta = await self._store.update_field(session_id, "status", SessionStatus.ACTIVE, "session_unarchived")
    if meta is not None and self.tree_index_invalidator is not None:
      self.tree_index_invalidator()
    return meta

  async def recycle_history_before(self, session_id: str, cutoff_utc: datetime) -> dict:
    """Move old chat events out of the live log into weekly archive files."""
    archive_result = await asyncio.to_thread(
        self._events.chat_events.archive_old_chat_events_sync, session_id, cutoff_utc)
    events_archived = archive_result["events_archived"]
    archive_file = archive_result["archive_file"]
    if events_archived:
      async with self._store.lock_for(session_id):
        fresh = await self._store.get_session(session_id)
        if fresh is not None:
          fresh.archive_offset += events_archived
          await self._store.save_metadata(fresh, lock_held=True)
      self._events.drop_session_runtime_state(session_id)
    return {"events_archived": events_archived, "archive_file": archive_file}

  async def star_session(self, session_id: str) -> SessionMetadata | None:
    """Star a session."""
    return await self._store.update_field(session_id, "starred", value=True, log_event="session_starred")

  async def unstar_session(self, session_id: str) -> SessionMetadata | None:
    """Unstar a session."""
    return await self._store.update_field(session_id, "starred", value=False, log_event="session_unstarred")

  async def set_group(self, session_id: str, group: str | None) -> SessionMetadata | None:
    """Set or clear the group for a session."""
    meta = await self._store.update_field(session_id, "group", group, "session_group_set")
    if meta:
      await self._events.broadcast_sidebar(session_id, ET.SESSION_GROUP_CHANGED, group=group)
    return meta

  async def rename_group(self, old_name: str, new_name: str) -> int:
    """Rename a group across all sessions. Returns the count of updated sessions."""
    count = await self._rewrite_group(old_name, new_name)
    if count:
      log.info("group_renamed", old_name=old_name, new_name=new_name, count=count)
    return count

  async def delete_group(self, group: str) -> int:
    """Remove a group from all sessions (set to null). Returns the count of updated sessions."""
    count = await self._rewrite_group(group, None)
    if count:
      log.info("group_deleted", group=group, count=count)
    return count

  async def _rewrite_group(self, old_name: str, new_name: str | None) -> int:
    """Set old_name's group to new_name on every matching session. Returns the count updated."""
    # Membership reads only, so the shared cached metas serve directly (read-only);
    # the leaving-the-manager copy is paid per matching row by get_session below.
    all_sessions = await self._store.load_session_metas()
    count = 0
    for meta in all_sessions:
      if meta.group != old_name:
        continue
      async with self._store.lock_for(meta.id):
        fresh = await self._store.get_session(meta.id)
        if not fresh or fresh.group != old_name:
          continue
        fresh.group = new_name
        fresh.updated_at = utc_now()
        await self._store.save_metadata(fresh, lock_held=True)
      count += 1
    return count


# The process owner of the lifecycle block; built on the first ``lifecycle()`` call.
_lifecycle: SessionLifecycle | None = None


def lifecycle() -> SessionLifecycle:
  """The process-wide session lifecycle block."""
  global _lifecycle
  if _lifecycle is None:
    _lifecycle = SessionLifecycle(get_config(), session_store.store(), session_events.events())
  return _lifecycle


def set_lifecycle(replacement: SessionLifecycle | None) -> None:
  """Replace the process lifecycle singleton (tests); None restores lazy construction."""
  global _lifecycle
  _lifecycle = replacement
