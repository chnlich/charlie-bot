"""Session management for CharlieBot."""

import asyncio
import json
import os
import shutil
import time
import uuid
from collections import OrderedDict
from datetime import datetime
from pathlib import Path
from typing import Any, Optional

import aiofiles
import structlog

from src.core import event_types as ET
from src.core import plan_paths
from src.core.chat_events import ChatEventStore
from src.core.config import CharlieBotConfig
from src.core.init import iter_recent_thread_metas
from src.core.json_utils import load_json_meta
from src.core.message_aggregator import MessageAggregator
from src.core.message_projection import MessageProjection
from src.core.models import (
  CreateSessionRequest,
  MasterRunRecord,
  SessionCallbacks,
  SessionMetadata,
  SessionStatus,
  parse_utc_datetime,
  utc_now,
)
from src.core.ndjson import append_ndjson
from src.core.scheduled_sessions import (  # noqa: F401  # re-export: src/api/cron.py imports ScheduledSessionBusyError from this module
  ScheduledSessionBusyError,
  ScheduledSessionStore,
)
from src.core.session_usage import SessionUsageResolver
from src.core.streaming import streaming_manager
from src.core.thinking_state import busy_since

# Raw event types whose render content is now produced by the per-session
# MessageAggregator as `message`/`stream` deltas. We persist these events but
# do not broadcast them raw -- the deltas are the wire-format replacement.
_RAW_EVENTS_REPLACED_BY_DELTAS: frozenset[str] = frozenset({ET.ASSISTANT, ET.USER, ET.SCHEDULED_TRIGGER})

log = structlog.get_logger()

_METADATA_CACHE_TTL = 30.0  # seconds
_PROJECTION_LRU_LIMIT = 8

_TRANSIENT_METADATA_FIELDS = {
    "has_running_tasks",
    "has_pending_trigger",
    "pending_trigger_count",
    "next_trigger_at",
    "has_pending_plan_approval",
    "schedule_cron",
    "schedule_enabled",
    "schedule_next_run",
    "schedule_timezone",
    "schedule_project",
    "schedule_allow_failure",
    "thinking_since",
}


def _stamp_thinking_since(meta: SessionMetadata) -> SessionMetadata:
  """Overwrite thinking_since with the live value before *meta* reaches a caller.

  thinking_since is a derived runtime fact owned by
  :mod:`src.core.thinking_state`; it is never persisted (see
  ``_TRANSIENT_METADATA_FIELDS``). Every return path that hands a
  SessionMetadata to a caller applies this stamp unconditionally — the field
  stays declared on the model, so a stale value parsed from an old metadata.json
  or restored into the cache by a post-save rebuild must not leak out.
  """
  meta.thinking_since = busy_since(meta.id)
  return meta


class SessionManager:
  """CRUD operations for CharlieBot sessions."""

  def __init__(self, cfg: CharlieBotConfig):
    self._cfg = cfg
    # In-memory metadata cache: session_id -> (metadata, monotonic_timestamp).
    # TTL-based to avoid repeated disk reads within the same poll cycle.
    self._metadata_cache: dict[str, tuple[SessionMetadata, float]] = {}
    # Per-session asyncio.Lock guarding metadata read-modify-write operations.
    # Prevents clobber races between concurrent mutators (e.g. mark_unread vs
    # update_thinking_state), which both load meta, mutate disjoint fields, and
    # save back — without a lock the second save overwrites the first's change.
    self._metadata_locks: dict[str, asyncio.Lock] = {}
    self._chat_events = ChatEventStore(self._session_dir, self._metadata_path, self._metadata_cache)
    self._session_usage = SessionUsageResolver(
        cfg,
        self._chat_events.events_cache,
        self._chat_events_path,
        self.load_chat_events_sync,
    )
    self._scheduled_sessions = ScheduledSessionStore(self)
    # Per-session MessageAggregator instance carrying live streaming state
    # (assistant_buf, tools_buf). Lazy-initialized from disk on first
    # persist_and_broadcast for a session after server start, then maintained
    # in memory across calls so consecutive assistant chunks accumulate into
    # a single bubble and tool-only events attach to the prior text bubble.
    self._aggregators: dict[str, MessageAggregator] = {}
    # Per-session MessageProjection cache (LRU, cap _PROJECTION_LRU_LIMIT).
    # Lazy-built from events_to_view; dirty-marked on master_done, cleared at
    # the same four sites as _aggregators. Only applies when archive_offset == 0.
    self._projection_cache: OrderedDict[str, MessageProjection] = OrderedDict()

  # ---------------------------------------------------------------------------
  # Session CRUD
  # ---------------------------------------------------------------------------

  async def create_session(self, req: CreateSessionRequest, backend: str | None = None) -> SessionMetadata:
    """Create a new session."""
    name = req.name or await self._next_session_name()
    overrides: dict = {}
    if req.session_id:
      overrides["id"] = req.session_id
    meta = SessionMetadata(
        name=name,
        scheduled_task=req.scheduled_task,
        role=req.role,
        backend=backend or self._cfg.backend_options[0].id,
        slack_origin=req.slack_origin,
        **overrides)

    session_dir = self._session_dir(meta.id)
    # Create directory structure
    for subdir in ["data", "threads"]:
      (session_dir / subdir).mkdir(parents=True, exist_ok=True)

    await self._save_metadata(meta)
    await self._backend_create_hook(meta)

    log.info("session_created", session_id=meta.id, name=meta.name)
    return _stamp_thinking_since(meta)

  async def ensure_scheduled_session_backend(
      self,
      task_name: str,
      backend: str,
      session_cache: Optional[dict[str, list[SessionMetadata]]] = None,
      skip_if_busy: bool = False,
      role: Optional[str] = None,
      group: Optional[str] = None,
  ) -> Optional[SessionMetadata]:
    """Return the active scheduled session for task_name/backend, rotating history if needed.

    Backend changes are generation changes: the old active session is archived and a new
    scheduled session is created with only scheduler bookkeeping copied over. ``role`` and
    ``group`` (mode: master PM tasks) carry onto the created session.
    """
    return await self._scheduled_sessions.ensure_scheduled_session_backend(
        task_name,
        backend,
        session_cache,
        skip_if_busy,
        role,
        group,
    )

  async def _backend_create_hook(self, meta: SessionMetadata) -> None:
    """Run backend-specific session-create work (e.g. spawn tmux for tui-cli)."""
    option = self._cfg.get_backend_option(meta.backend)
    if option is None or option.type != "tui-cli":
      return
    from src.agents.backends.tui import ensure_tmux_session
    try:
      await ensure_tmux_session(meta.id, self._session_dir(meta.id))
    except Exception:
      log.exception("backend_create_hook_failed", session_id=meta.id, backend=meta.backend)
      raise

  async def _backend_destroy_hook(self, session_id: str, meta: Optional[SessionMetadata]) -> None:
    """Run backend-specific teardown (e.g. kill tmux for tui-cli)."""
    # Only called on permanent delete. Archive is a status-only flag and must
    # NOT kill the underlying tmux/claude process for tui-cli sessions.
    if meta is None:
      return
    option = self._cfg.get_backend_option(meta.backend)
    if option is None or option.type != "tui-cli":
      return
    from src.agents.backends.tui import kill_tmux_session
    await kill_tmux_session(session_id)

  async def get_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Load session metadata, using in-memory cache when available."""
    cached = self._metadata_cache.get(session_id)
    if cached is not None:
      meta, ts = cached
      if (time.monotonic() - ts) < _METADATA_CACHE_TTL:
        if self._migrate_round_rating_keys(meta):
          await self._save_metadata(meta)
        return _stamp_thinking_since(meta.model_copy())
      del self._metadata_cache[session_id]
    path = self._metadata_path(session_id)
    if not path.exists():
      return None
    async with aiofiles.open(path, "r") as f:
      raw = await f.read()
    if not raw.strip():
      log.warning("session_metadata_empty", session_id=session_id, path=str(path))
      return None
    meta = SessionMetadata.model_validate_json(raw)
    if self._migrate_round_rating_keys(meta):
      await self._save_metadata(meta)
    else:
      self._metadata_cache[session_id] = (meta, time.monotonic())
    return _stamp_thinking_since(meta.model_copy())

  async def list_sessions(
      self,
      status: Optional[SessionStatus] = None,
      starred: Optional[bool] = None,
      scheduled: Optional[bool] = None,
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
      include_pending_plan_approval: bool = False,
  ) -> list[SessionMetadata]:
    """List sessions, newest first. Optionally filter by status, starred, and/or scheduled."""
    all_meta = await self._iter_session_metas()
    sessions = [
        meta for meta in all_meta if (status is None or meta.status == status) and
        (starred is None or meta.starred == starred) and (scheduled is None or bool(meta.scheduled_task) == scheduled)
    ]
    return await self._enrich_and_sort(
        sessions,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
        include_pending_plan_approval=include_pending_plan_approval,
    )

  async def search_sessions(
      self,
      query: str,
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
  ) -> list[SessionMetadata]:
    """Search active sessions by name and chat event content (case-insensitive)."""
    query_lower = query.lower()
    all_meta = await self._iter_session_metas()
    results: list[SessionMetadata] = []
    content_candidates: list[tuple[SessionMetadata, Path]] = []
    for meta in all_meta:
      if meta.status != SessionStatus.ACTIVE:
        continue
      # Check session name first
      if query_lower in (meta.name or '').lower():
        results.append(meta)
        continue
      # Collect non-name-matched sessions for parallel content scan
      content_candidates.append((meta, self._chat_events_path(meta.id)))

    async def _check_content(meta: SessionMetadata, path: Path) -> Optional[SessionMetadata]:
      """Check if a session's chat events contain the query (runs file I/O in thread pool)."""

      def _read_and_check() -> bool:
        if not path.exists():
          return False
        try:
          return query_lower in path.read_text(encoding='utf-8').lower()
        except OSError as e:
          log.debug('search_read_failed', session_id=meta.id, error=str(e))
          return False

      return meta if await asyncio.to_thread(_read_and_check) else None

    content_hits = await asyncio.gather(*(_check_content(m, p) for m, p in content_candidates))
    results.extend(meta for meta in content_hits if meta is not None)
    return await self._enrich_and_sort(
        results,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
    )

  async def fork_session(
      self,
      parent_id: str,
      event_index: int | None = None,
      backend: str | None = None,
  ) -> SessionMetadata:
    """Create a new session with parent events stored as a reference file."""
    meta = await self._spawn_with_reference(parent_id, event_index, backend, "C")

    log.info(
        'session_cloned',
        new_session=meta.id,
        parent=parent_id,
        event_index=event_index,
        backend=meta.backend,
    )
    return meta

  async def elone_session(
      self,
      parent_id: str,
      event_index: int,
      backend: str | None = None,
  ) -> SessionMetadata:
    """Create an Elon-e session: reference handoff, archive + thumbs-down parent."""
    meta = await self._spawn_with_reference(parent_id, event_index, backend, "E")

    # Auto-archive and thumbs-down the parent (re-read under lock so concurrent
    # mutations to the parent aren't clobbered).
    async with self._lock_for(parent_id):
      fresh_parent = await self.get_session(parent_id)
      if fresh_parent:
        fresh_parent.status = SessionStatus.ARCHIVED
        fresh_parent.rating = 'thumbs_down'
        fresh_parent.updated_at = utc_now()
        await self._save_metadata(fresh_parent)
    self._chat_events.clear_cache(parent_id)
    self._aggregators.pop(parent_id, None)
    self._clear_projection(parent_id)

    log.info(
        'session_eloned',
        new_session=meta.id,
        parent=parent_id,
        event_index=event_index,
        backend=meta.backend,
    )
    return meta

  async def _spawn_with_reference(
      self,
      parent_id: str,
      event_index: int | None,
      backend: str | None,
      name_prefix: str,
  ) -> SessionMetadata:
    """Create a child session whose parent history lives in data/parent_reference.jsonl."""
    parent = await self.get_session(parent_id)
    if not parent:
      raise FileNotFoundError(f"parent session not found: {parent_id}")

    count = await asyncio.to_thread(self.get_chat_event_count_sync, parent_id, parent)
    if event_index is None:
      end = count
    else:
      if event_index < 0 or event_index >= count:
        raise ValueError(f"event_index {event_index} out of range for parent session {parent_id} with {count} events")
      end = event_index + 1

    events, _ = await asyncio.to_thread(self.load_chat_events_range, parent_id, 0, end)
    if len(events) != end:
      raise ValueError(f"loaded {len(events)} parent events for requested range [0, {end})")

    meta = SessionMetadata(
        name=f'{name_prefix}{parent.name}',
        parent_session_id=parent_id,
        backend=backend or parent.backend,
        group=parent.group,
    )
    session_dir = self._session_dir(meta.id)
    for subdir in ['data', 'threads']:
      (session_dir / subdir).mkdir(parents=True, exist_ok=True)

    reference_path = session_dir / 'data' / 'parent_reference.jsonl'
    await asyncio.to_thread(self._write_reference_events_sync, reference_path, events)

    events_path = self._chat_events_path(meta.id)
    await asyncio.to_thread(events_path.write_text, '', encoding='utf-8')
    clone_event = {
        "type": ET.CLONE_START,
        "parent_session_id": parent_id,
        "parent_session_name": parent.name,
        "timestamp": utc_now().isoformat(),
    }
    await append_ndjson(events_path, clone_event)
    await self._save_metadata(meta)
    await self._copy_plans_to_child(parent_id, session_dir)
    return _stamp_thinking_since(meta)

  async def _copy_plans_to_child(self, parent_id: str, child_session_dir: Path) -> None:
    """Copy parent plans.json and every referenced artifact file into the child.

    The child registry rewrites each ``versions[].file`` to a POSIX path
    relative to the child session directory. All other registry fields carry
    over unchanged. A missing or outside-parent artifact logs a warning and
    does not abort the fork. No plans.json in the parent means nothing to copy.
    """
    parent_dir = self._session_dir(parent_id).resolve()
    parent_plans_path = parent_dir / "plans.json"
    if not parent_plans_path.exists():
      return
    await asyncio.to_thread(self._copy_plans_sync, parent_plans_path, parent_dir, child_session_dir.resolve())

  @staticmethod
  def _copy_plans_sync(parent_plans_path: Path, parent_dir: Path, child_dir: Path) -> None:
    raw = parent_plans_path.read_text(encoding="utf-8")
    data = json.loads(raw)
    reserved_relative_paths = {"plans.json"}
    for plan in data.get("plans", []):
      for ver in plan.get("versions", []):
        file_rel = ver.get("file")
        if not file_rel:
          continue
        _candidate, normalized_rel = plan_paths.resolve_plan_file(parent_dir, file_rel)
        if normalized_rel is not None:
          reserved_relative_paths.add(normalized_rel.as_posix())

    for plan in data.get("plans", []):
      for ver in plan.get("versions", []):
        file_rel = ver.get("file")
        if not file_rel:
          continue
        _candidate, normalized_rel = plan_paths.resolve_plan_file(parent_dir, file_rel)
        inside_parent = normalized_rel is not None
        if normalized_rel is None:
          fallback_rel = plan_paths.fallback_relative_path(parent_dir, _candidate)
          normalized_rel = fallback_rel
          suffix_number = 1
          while (normalized_rel.as_posix() in reserved_relative_paths or (child_dir / normalized_rel).exists()):
            normalized_rel = fallback_rel.with_name(f"{fallback_rel.name}.outside-{suffix_number}")
            suffix_number += 1
          reserved_relative_paths.add(normalized_rel.as_posix())
          log.warning(
              "plan_artifact_outside_parent_on_fork",
              file=str(_candidate),
              relative_file=normalized_rel.as_posix(),
              plan=plan.get("id"),
              v=ver.get("v"),
          )
        src = parent_dir / normalized_rel
        dst = child_dir / normalized_rel
        ver["file"] = normalized_rel.as_posix()
        if not inside_parent:
          continue
        if not src.exists():
          log.warning("plan_artifact_missing_on_fork", file=str(src), plan=plan.get("id"), v=ver.get("v"))
          continue
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, dst)
    child_plans_path = child_dir / "plans.json"
    child_plans_path.parent.mkdir(parents=True, exist_ok=True)
    tmp = child_plans_path.with_suffix(child_plans_path.suffix + ".tmp")
    tmp.write_text(json.dumps(data, indent=2), encoding="utf-8")
    os.replace(tmp, child_plans_path)

  @staticmethod
  def _write_reference_events_sync(path: Path, events: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = path.with_suffix(path.suffix + ".tmp")
    with open(tmp_path, "w", encoding="utf-8") as f:
      for event in events:
        f.write(json.dumps(event) + "\n")
    os.replace(tmp_path, path)

  def get_chat_events_path(self, session_id: str) -> Path:
    """Return the absolute path to a session's chat_events.jsonl."""
    return self._chat_events.get_chat_events_path(session_id)

  async def rename_session(self, session_id: str, new_name: str) -> Optional[SessionMetadata]:
    """Rename a session and return the updated metadata."""
    async with self._lock_for(session_id):
      meta = await self.get_session(session_id)
      if not meta:
        return None
      meta.name = new_name
      meta.updated_at = utc_now()
      await self._save_metadata(meta)
    log.info("session_renamed", session_id=session_id, new_name=new_name)
    return meta

  async def switch_backend(self, session_id: str, backend: str) -> Optional[SessionMetadata]:
    """Set the session's backend and return the updated metadata.

    Performs only the metadata write — the caller (API layer) is responsible
    for resume-domain validation and for persisting the audit event. Returns
    ``None`` when the session is missing. Broadcasts a sidebar update so other
    open tabs refresh their header.
    """
    async with self._lock_for(session_id):
      meta = await self.get_session(session_id)
      if not meta:
        return None
      meta.backend = backend
      meta.updated_at = utc_now()
      await self._save_metadata(meta)
    await streaming_manager.broadcast(
        "sidebar", {
            "type": ET.BACKEND_SWITCHED,
            "session_id": session_id,
            "backend": backend,
        })
    log.info("session_backend_switched", session_id=session_id, backend=backend)
    return meta

  async def mark_read(self, session_id: str) -> Optional[SessionMetadata]:
    """Clear the unread flag for a session."""
    async with self._lock_for(session_id):
      meta = await self.get_session(session_id)
      if not meta or not meta.has_unread:
        return meta
      meta.has_unread = False
      await self._save_metadata(meta)
    await streaming_manager.broadcast(
        "sidebar", {
            "type": ET.UNREAD_CHANGED,
            "session_id": session_id,
            "has_unread": False,
        })
    return meta

  async def mark_unread(self, session_id: str) -> None:
    """Set the unread flag for a session (called when master/workers produce output)."""
    async with self._lock_for(session_id):
      meta = await self.get_session(session_id)
      if not meta or meta.has_unread:
        return
      meta.has_unread = True
      await self._save_metadata(meta)
    await streaming_manager.broadcast(
        "sidebar", {
            "type": ET.UNREAD_CHANGED,
            "session_id": session_id,
            "has_unread": True,
        })

  async def archive_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Mark a session as archived (does not delete files)."""
    meta = await self._update_field(session_id, "status", SessionStatus.ARCHIVED, "session_archived")
    self._chat_events.clear_cache(session_id)
    self._aggregators.pop(session_id, None)
    self._clear_projection(session_id)
    return meta

  async def delete_session_permanently(self, session_id: str) -> bool:
    """Permanently delete a session and all its data from disk."""
    session_dir = self._session_dir(session_id)
    if not session_dir.exists():
      return False
    meta = await self.get_session(session_id)
    await self._backend_destroy_hook(session_id, meta)
    await asyncio.to_thread(shutil.rmtree, session_dir)
    self._chat_events.clear_cache(session_id)
    self._aggregators.pop(session_id, None)
    self._clear_projection(session_id)
    self._invalidate_cache(session_id)
    self._metadata_locks.pop(session_id, None)
    log.info("session_deleted_permanently", session_id=session_id)
    return True

  async def unarchive_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Restore an archived session back to active."""
    return await self._update_field(session_id, "status", SessionStatus.ACTIVE, "session_unarchived")

  async def recycle_scheduled_session(self, session_id: str, cutoff_utc: datetime) -> dict:
    """GC old threads and archive old chat_events for a scheduled session.

    Threads whose status is completed/failed/cancelled and whose ``completed_at``
    is earlier than ``cutoff_utc`` are removed. Chat events with timestamp
    earlier than ``cutoff_utc`` are moved out of the live ``chat_events.jsonl``
    into a weekly archive file under ``data/archives/``. Best-effort per-thread
    and per-event-line: a single bad file is logged and skipped, not raised.
    """
    threads_deleted = await asyncio.to_thread(self._gc_old_threads_sync, session_id, cutoff_utc)
    archive_result = await asyncio.to_thread(self._archive_old_chat_events_sync, session_id, cutoff_utc)
    events_archived = archive_result["events_archived"]
    archive_file = archive_result["archive_file"]

    if events_archived:
      async with self._lock_for(session_id):
        fresh = await self.get_session(session_id)
        if fresh is not None:
          fresh.archive_offset += events_archived
          await self._save_metadata(fresh)
      self._chat_events.clear_cache(session_id)
      self._aggregators.pop(session_id, None)
      self._clear_projection(session_id)

    log.info(
        "scheduled_session_recycle_done",
        session_id=session_id,
        threads_deleted=threads_deleted,
        events_archived=events_archived,
        archive_file=archive_file,
    )
    return {
        "threads_deleted": threads_deleted,
        "events_archived": events_archived,
        "archive_file": archive_file,
    }

  def _gc_old_threads_sync(self, session_id: str, cutoff_utc: datetime) -> int:
    """Remove thread dirs whose status is terminal and completed_at < cutoff."""
    threads_dir = self._session_dir(session_id) / "threads"
    if not threads_dir.exists():
      return 0
    gc_statuses = {"completed", "failed", "cancelled"}
    deleted = 0
    for thread_dir in threads_dir.iterdir():
      try:
        if not thread_dir.is_dir():
          continue
        meta = load_json_meta(thread_dir / "metadata.json", "thread_meta_read_failed_during_recycle")
        if meta is None:
          continue
        if meta.get("status") not in gc_statuses:
          continue
        completed_at_raw = meta.get("completed_at")
        if not completed_at_raw:
          continue
        try:
          completed_at = parse_utc_datetime(completed_at_raw)
        except ValueError as e:
          log.debug("thread_completed_at_parse_failed", thread=str(thread_dir), error=str(e))
          continue
        if completed_at >= cutoff_utc:
          continue
        shutil.rmtree(thread_dir)
        deleted += 1
      except Exception:
        log.exception("thread_gc_failed", thread=str(thread_dir))
    return deleted

  def _archive_old_chat_events_sync(self, session_id: str, cutoff_utc: datetime) -> dict:
    """Split live chat_events.jsonl at cutoff_utc, append the head to a weekly archive."""
    return self._chat_events._archive_old_chat_events_sync(session_id, cutoff_utc)

  async def star_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Star a session."""
    return await self._update_field(session_id, "starred", True, "session_starred")

  async def unstar_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Unstar a session."""
    return await self._update_field(session_id, "starred", False, "session_unstarred")

  async def set_group(self, session_id: str, group: Optional[str]) -> Optional[SessionMetadata]:
    """Set or clear the group for a session."""
    meta = await self._update_field(session_id, "group", group, "session_group_set")
    if meta:
      await streaming_manager.broadcast(
          "sidebar", {
              "type": ET.SESSION_GROUP_CHANGED,
              "session_id": session_id,
              "group": group,
          })
    return meta

  async def rename_group(self, old_name: str, new_name: str) -> int:
    """Rename a group across all sessions. Returns the count of updated sessions."""
    all_sessions = await self.list_sessions()
    count = 0
    for meta in all_sessions:
      if meta.group != old_name:
        continue
      async with self._lock_for(meta.id):
        fresh = await self.get_session(meta.id)
        if not fresh or fresh.group != old_name:
          continue
        fresh.group = new_name
        fresh.updated_at = utc_now()
        await self._save_metadata(fresh)
      count += 1
    if count:
      log.info("group_renamed", old_name=old_name, new_name=new_name, count=count)
    return count

  async def delete_group(self, group: str) -> int:
    """Remove a group from all sessions (set to null). Returns the count of updated sessions."""
    all_sessions = await self.list_sessions()
    count = 0
    for meta in all_sessions:
      if meta.group != group:
        continue
      async with self._lock_for(meta.id):
        fresh = await self.get_session(meta.id)
        if not fresh or fresh.group != group:
          continue
        fresh.group = None
        fresh.updated_at = utc_now()
        await self._save_metadata(fresh)
      count += 1
    if count:
      log.info("group_deleted", group=group, count=count)
    return count

  async def save_metadata(self, meta: SessionMetadata) -> None:
    """Public wrapper for _save_metadata."""
    await self._save_metadata(meta)

  async def persist_cc_session_id(self, session_id: str, cc_session_id: str) -> Optional[str]:
    """Persist a cc_session_id without clobbering unrelated metadata fields.

    Re-reads fresh metadata from disk under the per-session lock, mutates only
    ``cc_session_id`` (and ``cc_session_started_at`` when the on-disk id actually
    changes), saves, then re-reads and returns the ``cc_session_id`` now on
    disk. Never falls back to a whole-object save — that is the 2026-03-30
    defect that clobbered ``has_unread``. ``update_thinking_state`` is the
    reference pattern.
    """
    async with self._lock_for(session_id):
      self._invalidate_cache(session_id)
      fresh = await self.get_session(session_id)
      if fresh is None:
        return None
      if fresh.cc_session_id != cc_session_id:
        fresh.cc_session_id = cc_session_id
        fresh.cc_session_started_at = utc_now()
      await self._save_metadata(fresh)
      self._invalidate_cache(session_id)
      read_back = await self.get_session(session_id)
    return read_back.cc_session_id if read_back is not None else None

  async def has_completed_round(self, session_id: str) -> bool:
    """True when the live event stream contains a master_done event.

    Reads through the existing ``load_chat_events_sync`` cache; adds no new
    persistent state.
    """
    events = self.load_chat_events_sync(session_id)
    return any(ev.get("type") == ET.MASTER_DONE for ev in events)

  async def persist_master_run(self, session_id: str, record: Optional[MasterRunRecord]) -> None:
    """Set or clear the session's in-flight master-turn record.

    Same read-modify-write-under-lock pattern as ``persist_cc_session_id``: a
    whole-object save would clobber concurrent single-field writes.
    """
    async with self._lock_for(session_id):
      self._invalidate_cache(session_id)
      fresh = await self.get_session(session_id)
      if fresh is None:
        return
      fresh.master_run = record
      await self._save_metadata(fresh)
      self._invalidate_cache(session_id)

  async def update_thinking_state(
      self,
      session_id: str,
      updated_at: datetime,
  ) -> None:
    """Persist updated_at without clobbering unrelated fields.

    Re-reads fresh metadata from disk before writing, so concurrent changes
    to fields like 'group' are preserved.
    """
    async with self._lock_for(session_id):
      self._invalidate_cache(session_id)
      fresh = await self.get_session(session_id)
      if fresh:
        fresh.updated_at = updated_at
        await self._save_metadata(fresh)

  def list_active_session_ids(self) -> list[SessionMetadata]:
    """Return metadata for active sessions by reading metadata.json files.

    Sync method — returns full SessionMetadata objects so callers avoid
    a second disk read. Populates the metadata cache as a side-effect.
    """
    if not self._cfg.sessions_dir.exists():
      return []
    results: list[SessionMetadata] = []
    now = time.monotonic()
    for d in self._cfg.sessions_dir.iterdir():
      if not d.is_dir():
        continue
      meta_path = self._metadata_path(d.name)
      if not meta_path.exists():
        continue
      try:
        raw = meta_path.read_text(encoding="utf-8")
        meta = SessionMetadata.model_validate_json(raw)
        if meta.status == SessionStatus.ACTIVE:
          self._metadata_cache[d.name] = (meta, now)
          results.append(_stamp_thinking_since(meta.model_copy()))
      except (json.JSONDecodeError, OSError, ValueError) as e:
        log.debug("list_active_ids_skip", dir=d.name, error=str(e))
    return results

  # ---------------------------------------------------------------------------
  # Chat event persistence (NDJSON — for WebSocket catch-up)
  # ---------------------------------------------------------------------------

  async def save_chat_event(self, session_id: str, event: dict) -> None:
    """Append a single NDJSON event line to chat_events.jsonl."""
    await self._chat_events.save_chat_event(session_id, event)

  async def persist_and_broadcast(self, session_id: str, event: dict) -> None:
    """Persist event, run it through the session's aggregator, broadcast deltas + raw event.

    Raw `assistant` and `user` events are no longer broadcast on the wire; the
    aggregator emits ``message``/``stream`` deltas in their place. All other
    event types still flow as before because clients use them for state
    side-effects (e.g. ``master_done`` → stopThinking).
    """
    # Prime the events cache + aggregator before persisting so event_index
    # injection works on the very first call after server start (and so the
    # aggregator state matches what SSR/SPA-switch produced for the same
    # events).
    aggregator = self._get_or_init_aggregator(session_id)
    meta = await self.get_session(session_id)
    archive_offset = meta.archive_offset if meta else 0
    await self.save_chat_event(session_id, event)
    event['event_index'] = archive_offset + self._chat_events.cached_event_count(session_id) - 1

    channel = f"session:{session_id}"
    deltas = list(aggregator.feed(event))
    for delta in deltas:
      await streaming_manager.broadcast(channel, delta)
    if event.get('type') not in _RAW_EVENTS_REPLACED_BY_DELTAS:
      await streaming_manager.broadcast(channel, event)

  async def broadcast_only(self, session_id: str, event: dict) -> None:
    """Broadcast an event on the session channel without persisting it as a chat event.

    Used for state-change notifications (e.g. ``plan_updated``) that must not
    pollute the chat history or replay on reconnect.
    """
    channel = f"session:{session_id}"
    await streaming_manager.broadcast(channel, event)

  def _get_or_init_aggregator(self, session_id: str) -> MessageAggregator:
    """Return the live aggregator for *session_id*, lazy-initialized from disk.

    On first use after server start, the aggregator catches up to the current
    on-disk state by silently consuming all persisted events. Their deltas are
    discarded -- subscribed clients have already rendered them via SSR or
    SPA-switch which both use the same aggregator logic.
    """
    aggregator = self._aggregators.get(session_id)
    if aggregator is not None:
      return aggregator
    # Live file only holds events from index archive_offset onward; seed the
    # aggregator's offset so the deltas it emits carry the same GLOBAL
    # event_index that persist_and_broadcast stamps on the raw event.
    aggregator = MessageAggregator(event_index_offset=self._read_archive_offset_sync(session_id))
    for ev in self.load_chat_events_sync(session_id):
      for _ in aggregator.feed(ev):
        pass
    self._aggregators[session_id] = aggregator
    return aggregator

  def callbacks(self) -> SessionCallbacks:
    """Return a bundle of session-related callbacks for run_message()."""
    return SessionCallbacks(
        persist_and_broadcast=self.persist_and_broadcast,
        update_thinking_state=self.update_thinking_state,
        mark_unread=self.mark_unread,
        persist_cc_session_id=self.persist_cc_session_id,
        has_completed_round=self.has_completed_round,
        persist_master_run=self.persist_master_run,
    )

  def load_chat_events_sync(self, session_id: str) -> list[dict]:
    """Read all chat events for catch-up. Uses in-memory cache after first read."""
    return self._chat_events.load_chat_events_sync(session_id)

  def load_chat_events_tail(self, session_id: str, limit: int = 200) -> tuple[list[dict], int, bool]:
    """Load only the last *limit* events from disk. Does NOT populate _events_cache.

    Returns (events, total_line_count, has_more).
    """
    return self._chat_events.load_chat_events_tail(session_id, limit)

  def get_chat_event_count_sync(self, session_id: str, session_meta: SessionMetadata | None = None) -> int:
    """Return the current global chat event count without parsing event payloads."""
    return self._chat_events.get_chat_event_count_sync(session_id, session_meta)

  def load_chat_events_range(self, session_id: str, start: int, end: int) -> tuple[list[dict], bool]:
    """Load events in GLOBAL index range [start, end). Returns (events, has_more).

    Indices are global (archive_offset + line_in_live_file). When the requested
    range starts before the live file, archived chat_events files under
    ``data/archives/`` are read in chronological order to fill the gap.
    """
    return self._chat_events.load_chat_events_range(session_id, start, end)

  def get_message_projection(self, session_id: str) -> MessageProjection | None:
    """Return the memoized message-list projection for *session_id*.

    Lazily builds from ``events_to_view(load_chat_events_sync(session_id))``
    and caches per session (LRU, cap ``_PROJECTION_LRU_LIMIT``). Returns None
    when ``archive_offset != 0`` — those sessions fall back entirely to the
    event-index cursor path and never mix the two cursor domains.
    """
    if self._read_archive_offset_sync(session_id) != 0:
      return None
    live = self.load_chat_events_sync(session_id)
    snapshot = list(live)
    cached = self._projection_cache.get(session_id)
    if cached is not None and cached.event_count == len(snapshot):
      self._projection_cache.move_to_end(session_id)
      return cached
    projection = MessageProjection(snapshot)
    self._projection_cache[session_id] = projection
    while len(self._projection_cache) > _PROJECTION_LRU_LIMIT:
      self._projection_cache.popitem(last=False)
    return projection

  def _clear_projection(self, session_id: str) -> None:
    """Drop the cached projection and any pending dirty mark for *session_id*."""
    self._projection_cache.pop(session_id, None)

  def _read_archive_offset_sync(self, session_id: str) -> int:
    """Synchronously read the archive_offset from metadata.json.

    Used by sync read paths (e.g. ``load_chat_events_range``) so they don't
    have to go async just to learn the live/archive split. Falls back to 0 if
    the metadata file is missing or unreadable.
    """
    return self._chat_events._read_archive_offset_sync(session_id)

  def _load_archive_range(self, session_id: str, start: int, end: int) -> list[dict]:
    """Read events at global indices [start, end) from archive files.

    Archives live in ``<session>/data/archives/chat_events.<YYYY>-W<WW>.jsonl``.
    Files are walked in chronological order (filename sort happens to match).
    """
    return self._chat_events._load_archive_range(session_id, start, end)

  def _chat_events_path(self, session_id: str) -> Path:
    return self._chat_events.get_chat_events_path(session_id)

  async def resolve_session_usage(
      self,
      session_id: str,
      session_meta: SessionMetadata,
  ) -> dict | None:
    """Resolve display usage for a session view as a projection over its events.

    Usage is computed on demand from the full chat-event stream (no incremental
    cache). See ``src/core/session_usage.py`` for the tier contract.
    """
    return await self._session_usage.resolve_session_usage(session_id, session_meta)

  # ---------------------------------------------------------------------------
  # Private helpers
  # ---------------------------------------------------------------------------

  def _invalidate_cache(self, session_id: str) -> None:
    """Remove a session from the metadata cache."""
    self._metadata_cache.pop(session_id, None)

  @staticmethod
  def _migrate_round_rating_keys(meta: SessionMetadata) -> bool:
    """Rewrite pre-UUID rating keys from event_index strings to legacy ids."""
    if not meta.round_ratings:
      return False
    migrated = {}
    changed = False
    for key, value in meta.round_ratings.items():
      if key.isdigit():
        migrated[f"legacy:{key}"] = value
        changed = True
      else:
        migrated[key] = value
    if changed:
      meta.round_ratings = migrated
    return changed

  async def _iter_session_metas(self) -> list[SessionMetadata]:
    """Load all session metadata, batching disk reads for cache misses.

    Performs the standard three-step preamble shared by every listing entry point:
    (1) return [] if sessions_dir does not exist, (2) list session directories under
    asyncio.to_thread to avoid blocking the event loop, (3) use fresh cache entries
    directly and load all missing metadata files serially in one asyncio.to_thread
    call, logging and dropping any session that fails to load.
    """
    if not self._cfg.sessions_dir.exists():
      return []
    dirs = await asyncio.to_thread(lambda: [d for d in self._cfg.sessions_dir.iterdir() if d.is_dir()])

    cached_metas: dict[str, SessionMetadata] = {}
    missing_ids: list[str] = []
    for d in dirs:
      cached = self._metadata_cache.get(d.name)
      if cached is None:
        missing_ids.append(d.name)
        continue
      meta, ts = cached
      if (time.monotonic() - ts) < _METADATA_CACHE_TTL:
        cached_metas[d.name] = meta
      else:
        del self._metadata_cache[d.name]
        missing_ids.append(d.name)

    raw_by_id: dict[str, str] = {}
    read_failures: dict[str, Exception] = {}
    if missing_ids:
      def _read_missing() -> tuple[dict[str, str], dict[str, Exception]]:
        loaded: dict[str, str] = {}
        failures: dict[str, Exception] = {}
        for session_id in missing_ids:
          path = self._metadata_path(session_id)
          try:
            if not path.exists():
              continue
            loaded[session_id] = path.read_text(encoding="utf-8")
          except Exception as exc:
            failures[session_id] = exc
        return loaded, failures

      raw_by_id, read_failures = await asyncio.to_thread(_read_missing)

    result: list[SessionMetadata] = []
    for d in dirs:
      session_id = d.name
      loaded_from_cache = session_id in cached_metas
      if loaded_from_cache:
        meta = cached_metas[session_id]
      else:
        if session_id in read_failures:
          log.warning('session_load_failed', session_id=session_id, error=str(read_failures[session_id]))
          continue
        if session_id not in raw_by_id:
          continue
        raw = raw_by_id[session_id]
        if not raw.strip():
          log.warning("session_metadata_empty", session_id=session_id, path=str(self._metadata_path(session_id)))
          continue
        try:
          meta = SessionMetadata.model_validate_json(raw)
        except Exception as exc:
          log.warning('session_load_failed', session_id=session_id, error=str(exc))
          continue

      try:
        migrated = self._migrate_round_rating_keys(meta)
        if migrated:
          await self._save_metadata(meta)
      except Exception as exc:
        log.warning('session_load_failed', session_id=session_id, error=str(exc))
        continue

      if not loaded_from_cache and not migrated:
        self._metadata_cache.setdefault(session_id, (meta, time.monotonic()))
      result.append(_stamp_thinking_since(meta.model_copy()))
    return result

  async def _active_scheduled_sessions(
      self,
      task_name: str,
      session_cache: Optional[dict[str, list[SessionMetadata]]] = None,
  ) -> list[SessionMetadata]:
    """Return active scheduled sessions for task_name, newest first."""
    return await self._scheduled_sessions._active_scheduled_sessions(task_name, session_cache)

  async def _scheduled_session_busy(self, session: SessionMetadata) -> bool:
    """Return whether a scheduled session has active master thinking or worker threads."""
    return await self._scheduled_sessions._scheduled_session_busy(session)

  def _lock_for(self, session_id: str) -> asyncio.Lock:
    """Return (creating on first use) the per-session metadata RMW lock."""
    lock = self._metadata_locks.get(session_id)
    if lock is None:
      lock = asyncio.Lock()
      self._metadata_locks[session_id] = lock
    return lock

  async def _update_field(self, session_id: str, field: str, value: Any, log_event: str) -> Optional[SessionMetadata]:
    """Get a session, set one field, save, and log. Returns None if session not found."""
    async with self._lock_for(session_id):
      meta = await self.get_session(session_id)
      if not meta:
        return None
      setattr(meta, field, value)
      meta.updated_at = utc_now()
      await self._save_metadata(meta)
    log.info(log_event, session_id=session_id)
    return meta

  async def _has_running_tasks(self, session_id: str) -> bool:
    """Check if a session has any thread currently marked 'running'.

    A running thread's metadata.json is recent by definition (written at start,
    never rewritten while it runs), so only threads whose metadata mtime is within
    RUNNING_SCAN_WINDOW are read+parsed; older thread dirs are dropped after a cheap
    scandir+stat with zero content reads, and a session whose only threads are old
    returns False without reading any metadata. Runs its filesystem work in a thread
    to keep the event loop responsive (called per-session by the sidebar/status polls).
    """

    def _check() -> bool:
      threads_dir = self._session_dir(session_id) / "threads"
      now = utc_now()
      for _thread_dir, _meta_path, meta in iter_recent_thread_metas(threads_dir, now, "thread_meta_read_failed"):
        if meta.get("status") == "running":
          return True
      return False

    return await asyncio.to_thread(_check)

  async def _get_pending_trigger_state(self, session_id: str) -> tuple[int, Optional[datetime]]:
    """Return the number of pending delayed triggers and the earliest fire time."""

    def _check() -> tuple[int, Optional[datetime]]:
      triggers_dir = self._session_dir(session_id) / "triggers"
      if not triggers_dir.exists():
        return 0, None

      pending_count = 0
      next_trigger_at: Optional[datetime] = None
      for trigger_path in triggers_dir.glob("*.json"):
        trigger = load_json_meta(
            trigger_path,
            "trigger_meta_read_failed",
            catch=(json.JSONDecodeError, OSError, ValueError),
        )
        if trigger is None:
          continue
        if trigger.get("status") != "pending":
          continue

        pending_count += 1
        fire_at_raw = trigger.get("fire_at")
        if not fire_at_raw:
          continue
        try:
          fire_at = parse_utc_datetime(fire_at_raw)
        except ValueError as e:
          log.debug("trigger_fire_at_parse_failed", trigger_path=str(trigger_path), error=str(e))
          continue
        if next_trigger_at is None or fire_at < next_trigger_at:
          next_trigger_at = fire_at

      return pending_count, next_trigger_at

    return await asyncio.to_thread(_check)

  def _has_pending_plan_approval(self, session_id: str) -> bool:
    """Return True if the session has a lineage whose derived state is 'awaiting approval'.

    Delegates to the tolerant read in src.core.plans (single authority for catch-and-derive).
    Any error entry is logged via ``plan_registry_read_failed`` and contributes no pending
    approval. The probe must never raise — a corrupt single-session file cannot 5xx the
    sidebar poll for all sessions.
    """
    from src.core.plans import read_plans_tolerant

    plans_path = self._session_dir(session_id) / "plans.json"
    result = read_plans_tolerant(plans_path, session_id)
    for error in result["errors"]:
      log.warning(
          "plan_registry_read_failed",
          session_id=error.get("session_id"),
          error=error.get("error"),
      )
    return any(plan.get("state") == "awaiting approval" for plan in result["plans"])

  async def _enrich_and_sort(
      self,
      sessions: list[SessionMetadata],
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
      include_pending_plan_approval: bool = False,
  ) -> list[SessionMetadata]:
    """Populate sidebar state and sort newest first."""
    await self._populate_sidebar_state(
        sessions,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
        include_pending_plan_approval=include_pending_plan_approval,
    )
    sessions.sort(key=lambda s: s.updated_at, reverse=True)
    return sessions

  async def populate_sidebar_state(
      self,
      sessions: list[SessionMetadata],
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
      include_pending_plan_approval: bool = False,
  ) -> None:
    """Public wrapper for _populate_sidebar_state."""
    await self._populate_sidebar_state(
        sessions,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
        include_pending_plan_approval=include_pending_plan_approval,
    )

  async def _populate_sidebar_state(
      self,
      sessions: list[SessionMetadata],
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
      include_pending_plan_approval: bool = False,
  ) -> None:
    """Populate derived sidebar-only state on session metadata objects."""
    if not sessions:
      return

    # Archived sessions cannot have running tasks or pending triggers, so skip
    # the per-session filesystem work for them.
    active_sessions = [m for m in sessions if m.status != SessionStatus.ARCHIVED]
    archived_sessions = [m for m in sessions if m.status == SessionStatus.ARCHIVED]

    if include_running_status:
      for meta in archived_sessions:
        meta.has_running_tasks = False
    if include_pending_trigger_status:
      for meta in archived_sessions:
        meta.has_pending_trigger = False
        meta.pending_trigger_count = 0
        meta.next_trigger_at = None
    if include_pending_plan_approval:
      for meta in archived_sessions:
        meta.has_pending_plan_approval = False

    if not active_sessions:
      return

    running_future = None
    trigger_future = None
    plan_approval_future = None
    if include_running_status:
      running_future = asyncio.gather(*(self._has_running_tasks(meta.id) for meta in active_sessions))
    if include_pending_trigger_status:
      trigger_future = asyncio.gather(*(self._get_pending_trigger_state(meta.id) for meta in active_sessions))
    if include_pending_plan_approval:
      plan_approval_future = asyncio.gather(
          *(asyncio.to_thread(self._has_pending_plan_approval, meta.id) for meta in active_sessions))

    pending_futures = [f for f in (running_future, trigger_future, plan_approval_future) if f is not None]
    if not pending_futures:
      return
    gathered = await asyncio.gather(*pending_futures)
    idx = 0
    running_flags = None
    trigger_states = None
    plan_approval_flags = None
    if running_future is not None:
      running_flags = gathered[idx]
      idx += 1
    if trigger_future is not None:
      trigger_states = gathered[idx]
      idx += 1
    if plan_approval_future is not None:
      plan_approval_flags = gathered[idx]
      idx += 1

    if running_flags is not None:
      for meta, running in zip(active_sessions, running_flags):
        meta.has_running_tasks = bool(meta.thinking_since) or running

    if trigger_states is not None:
      for meta, (pending_count, next_trigger_at) in zip(active_sessions, trigger_states):
        meta.has_pending_trigger = pending_count > 0
        meta.pending_trigger_count = pending_count
        meta.next_trigger_at = next_trigger_at

    if plan_approval_flags is not None:
      for meta, has_pending in zip(active_sessions, plan_approval_flags):
        meta.has_pending_plan_approval = bool(has_pending)

  async def _next_session_name(self) -> str:
    """Generate 'Session 0', 'Session 1', etc. using a persistent counter file.

    Reads the next number from sessions_dir/.counter (O(1) instead of listing
    all sessions). Falls back to counting directories if the file is missing.
    """
    counter_path = self._cfg.sessions_dir / ".counter"

    def _read_and_increment() -> int:
      self._cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
      if counter_path.exists():
        try:
          n = int(counter_path.read_text().strip())
        except (ValueError, OSError):
          n = self._count_session_dirs()
      else:
        n = self._count_session_dirs()
      counter_path.write_text(str(n + 1))
      return n

    n = await asyncio.to_thread(_read_and_increment)
    return f"Session {n}"

  def _count_session_dirs(self) -> int:
    """Count existing session directories for backward-compat counter init."""
    if not self._cfg.sessions_dir.exists():
      return 0
    return sum(1 for d in self._cfg.sessions_dir.iterdir() if d.is_dir())

  async def _save_metadata(self, meta: SessionMetadata) -> None:
    path = self._metadata_path(meta.id)
    path.parent.mkdir(parents=True, exist_ok=True)
    serialized = meta.model_dump_json(indent=2, exclude=_TRANSIENT_METADATA_FIELDS)

    def _atomic_write() -> None:
      tmp = path.with_name(f"{path.name}.{os.getpid()}.{uuid.uuid4().hex}.tmp")
      try:
        with open(tmp, "w", encoding="utf-8") as f:
          f.write(serialized)
        os.replace(tmp, path)
      except BaseException:
        try:
          tmp.unlink()
        except OSError:
          pass
        raise

    await asyncio.to_thread(_atomic_write)
    self._metadata_cache[meta.id] = (SessionMetadata.model_validate_json(serialized), time.monotonic())

  def _session_dir(self, session_id: str) -> Path:
    return self._cfg.sessions_dir / session_id

  def _metadata_path(self, session_id: str) -> Path:
    return self._session_dir(session_id) / "metadata.json"
