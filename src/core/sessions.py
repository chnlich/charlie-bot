"""Session management for CharlieBot."""

import asyncio
import json
import os
import shutil
import time
import uuid
from datetime import datetime
from pathlib import Path
from typing import Any, Optional

import aiofiles
import structlog

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.init import iter_recent_thread_metas
from src.core.json_utils import load_json_meta
from src.core.message_aggregator import MessageAggregator
from src.core.models import (
    CreateSessionRequest,
    SessionCallbacks,
    SessionMetadata,
    SessionStatus,
    parse_utc_datetime,
    utc_now,
)
from src.core.codex_usage import CodexUsageResolver
from src.core.ndjson import append_ndjson, count_ndjson_lines, parse_ndjson_file, parse_ndjson_range, parse_ndjson_tail
from src.core.streaming import streaming_manager

# Raw event types whose render content is now produced by the per-session
# MessageAggregator as `message`/`stream` deltas. We persist these events but
# do not broadcast them raw -- the deltas are the wire-format replacement.
_RAW_EVENTS_REPLACED_BY_DELTAS: frozenset[str] = frozenset({ET.ASSISTANT, ET.USER})

log = structlog.get_logger()

_METADATA_CACHE_TTL = 30.0  # seconds
_UNSET = object()
_DEFAULT_CONTEXT_LIMIT = 200_000


def _extract_usage_from_result(event: dict) -> tuple[int, int, str]:
  """Extract (context_tokens, context_limit, model) from a single 'result' event.

  context_tokens = input_tokens + cache_creation_input_tokens + cache_read_input_tokens.
  context_limit and model come from the first modelUsage entry (defaults:
  200_000 and "").
  """
  usage = event.get("usage", {})
  context_tokens = (
      usage.get("input_tokens", 0) + usage.get("cache_creation_input_tokens", 0) +
      usage.get("cache_read_input_tokens", 0))
  context_limit = _DEFAULT_CONTEXT_LIMIT
  model = ""
  for model_name, info in event.get("modelUsage", {}).items():
    model = model_name
    context_limit = info.get("contextWindow", _DEFAULT_CONTEXT_LIMIT)
    break
  return context_tokens, context_limit, model


_TRANSIENT_METADATA_FIELDS = {
    "has_running_tasks",
    "has_pending_trigger",
    "pending_trigger_count",
    "next_trigger_at",
    "schedule_cron",
    "schedule_enabled",
    "schedule_next_run",
    "schedule_timezone",
    "schedule_project",
    "schedule_allow_failure",
}


class ScheduledSessionBusyError(RuntimeError):
  """Raised when a scheduled session cannot rotate backends because it is busy."""


class SessionManager:
  """CRUD operations for CharlieBot sessions."""

  def __init__(self, cfg: CharlieBotConfig):
    self._cfg = cfg
    # In-memory cache: session_id -> list[dict] of parsed NDJSON events.
    # Populated on first read, kept in sync by save_chat_event().
    self._events_cache: dict[str, list[dict]] = {}
    # In-memory usage cache: session_id -> usage dict (context_tokens, context_limit, total_cost_usd, model).
    # Incrementally updated on each 'result' event via save_chat_event(), avoiding O(n) rescans.
    self._usage_cache: dict[str, dict] = {}
    # In-memory metadata cache: session_id -> (metadata, monotonic_timestamp).
    # TTL-based to avoid repeated disk reads within the same poll cycle.
    self._metadata_cache: dict[str, tuple[SessionMetadata, float]] = {}
    # Per-session asyncio.Lock guarding metadata read-modify-write operations.
    # Prevents clobber races between concurrent mutators (e.g. mark_unread vs
    # clear_thinking_since), which both load meta, mutate disjoint fields, and
    # save back — without a lock the second save overwrites the first's change.
    self._metadata_locks: dict[str, asyncio.Lock] = {}
    # Per-session MessageAggregator instance carrying live streaming state
    # (assistant_buf, tools_buf). Lazy-initialized from disk on first
    # persist_and_broadcast for a session after server start, then maintained
    # in memory across calls so consecutive assistant chunks accumulate into
    # a single bubble and tool-only events attach to the prior text bubble.
    self._aggregators: dict[str, MessageAggregator] = {}
    # Codex-specific usage resolution delegated to a dedicated module.
    self._codex_resolver = CodexUsageResolver(cfg, self._events_cache, self._chat_events_path)

  # ---------------------------------------------------------------------------
  # Session CRUD
  # ---------------------------------------------------------------------------

  async def create_session(self, req: CreateSessionRequest, backend: str | None = None) -> SessionMetadata:
    """Create a new session."""
    name = req.name or await self._next_session_name()
    meta = SessionMetadata(
        name=name, scheduled_task=req.scheduled_task, backend=backend or self._cfg.backend_options[0].id)

    session_dir = self._session_dir(meta.id)
    # Create directory structure
    for subdir in ["data", "threads"]:
      (session_dir / subdir).mkdir(parents=True, exist_ok=True)

    await self._save_metadata(meta)
    await self._backend_create_hook(meta)

    log.info("session_created", session_id=meta.id, name=meta.name)
    return meta

  async def ensure_scheduled_session_backend(
      self,
      task_name: str,
      backend: str,
      session_cache: Optional[dict[str, list[SessionMetadata]]] = None,
      skip_if_busy: bool = False,
  ) -> Optional[SessionMetadata]:
    """Return the active scheduled session for task_name/backend, rotating history if needed.

    Backend changes are generation changes: the old active session is archived and a new
    scheduled session is created with only scheduler bookkeeping copied over.
    """
    active_sessions = await self._active_scheduled_sessions(task_name, session_cache)
    for session in active_sessions:
      if session.backend == backend:
        return session

    old_session = active_sessions[0] if active_sessions else None
    if old_session is None:
      meta = await self.create_session(
          CreateSessionRequest(name=f"Scheduled: {task_name}", scheduled_task=task_name), backend=backend)
      log.info("scheduled_session_created", task=task_name, session=meta.id, backend=backend)
      if session_cache is not None:
        session_cache.setdefault(task_name, []).insert(0, meta)
      return meta

    if await self._scheduled_session_busy(old_session):
      message = (
          f"scheduled task '{task_name}' backend switch from '{old_session.backend}' to '{backend}' is blocked "
          f"because session '{old_session.id}' has running work")
      if skip_if_busy:
        log.warning(
            "scheduled_session_rotation_busy",
            task=task_name,
            session=old_session.id,
            old_backend=old_session.backend,
            backend=backend,
        )
        return None
      raise ScheduledSessionBusyError(message)

    await self.archive_session(old_session.id)
    meta = await self.create_session(
        CreateSessionRequest(name=f"Scheduled: {task_name}", scheduled_task=task_name), backend=backend)
    meta.last_scheduled_run = old_session.last_scheduled_run
    meta.last_scheduled_cron = old_session.last_scheduled_cron
    meta.last_run_status = old_session.last_run_status
    meta.updated_at = utc_now()
    await self.save_metadata(meta)
    if session_cache is not None:
      cached_sessions = session_cache.setdefault(task_name, [])
      for cached in cached_sessions:
        if cached.id == old_session.id:
          cached.status = SessionStatus.ARCHIVED
      cached_sessions.insert(0, meta)
    log.info(
        "scheduled_session_rotated",
        task=task_name,
        old_session=old_session.id,
        new_session=meta.id,
        old_backend=old_session.backend,
        backend=backend,
    )
    return meta

  async def _backend_create_hook(self, meta: SessionMetadata) -> None:
    """Run backend-specific session-create work (e.g. spawn tmux for tui-cli)."""
    option = self._cfg.get_backend_option(meta.backend)
    if option is None or option.type != "tui-cli":
      return
    from src.agents.backends.tui import ensure_tmux_session
    try:
      await ensure_tmux_session(meta.id, self._session_dir(meta.id))
    except Exception as e:
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
        return meta.model_copy()
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
    return meta.model_copy()

  async def list_sessions(
      self,
      status: Optional[SessionStatus] = None,
      starred: Optional[bool] = None,
      scheduled: Optional[bool] = None,
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
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
  ) -> Optional[SessionMetadata]:
    """Create a new session by cloning an existing one.

    If event_index is set, only events [0..event_index] (inclusive) are copied.
    Otherwise all events are copied (full clone).
    """
    parent = await self.get_session(parent_id)
    if not parent:
      return None

    parent_events_path = self._chat_events_path(parent_id)
    if not parent_events_path.exists():
      return None
    lines_text = await asyncio.to_thread(parent_events_path.read_text, encoding='utf-8')
    lines = lines_text.splitlines()

    if event_index is not None:
      lines = lines[:event_index + 1]

    # Create the new session, inheriting the parent backend unless overridden.
    meta = SessionMetadata(
        name=f'C{parent.name}',
        parent_session_id=parent_id,
        backend=backend or parent.backend,
        group=parent.group,
    )
    session_dir = self._session_dir(meta.id)
    for subdir in ['data', 'threads']:
      (session_dir / subdir).mkdir(parents=True, exist_ok=True)

    # Copy chat events to new session
    events_path = self._chat_events_path(meta.id)
    await asyncio.to_thread(events_path.write_text, '\n'.join(lines) + '\n', encoding='utf-8')

    # Append clone_start banner event
    clone_event = {
        "type": ET.CLONE_START,
        "parent_session_id": parent_id,
        "parent_session_name": parent.name,
        "timestamp": utc_now().isoformat(),
    }
    await append_ndjson(events_path, clone_event)

    await self._save_metadata(meta)

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
  ) -> Optional[SessionMetadata]:
    """Create an Elon-e session: empty history, archive + thumbs-down the parent."""
    parent = await self.get_session(parent_id)
    if not parent:
      return None

    # Create new session with empty history
    meta = SessionMetadata(
        name=f'E{parent.name}',
        parent_session_id=parent_id,
        backend=backend or parent.backend,
        group=parent.group,
    )
    session_dir = self._session_dir(meta.id)
    for subdir in ['data', 'threads']:
      (session_dir / subdir).mkdir(parents=True, exist_ok=True)

    # Create empty chat_events.jsonl
    events_path = self._chat_events_path(meta.id)
    await asyncio.to_thread(events_path.write_text, '', encoding='utf-8')

    await self._save_metadata(meta)

    # Auto-archive and thumbs-down the parent (re-read under lock so concurrent
    # mutations to the parent aren't clobbered).
    async with self._lock_for(parent_id):
      fresh_parent = await self.get_session(parent_id)
      if fresh_parent:
        fresh_parent.status = SessionStatus.ARCHIVED
        fresh_parent.rating = 'thumbs_down'
        fresh_parent.updated_at = utc_now()
        await self._save_metadata(fresh_parent)
    self._events_cache.pop(parent_id, None)
    self._usage_cache.pop(parent_id, None)
    self._aggregators.pop(parent_id, None)

    log.info(
        'session_eloned',
        new_session=meta.id,
        parent=parent_id,
        event_index=event_index,
        backend=meta.backend,
    )
    return meta

  def get_chat_events_path(self, session_id: str) -> Path:
    """Return the absolute path to a session's chat_events.jsonl."""
    return self._chat_events_path(session_id)

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
    self._events_cache.pop(session_id, None)
    self._usage_cache.pop(session_id, None)
    self._aggregators.pop(session_id, None)
    return meta

  async def delete_session_permanently(self, session_id: str) -> bool:
    """Permanently delete a session and all its data from disk."""
    session_dir = self._session_dir(session_id)
    if not session_dir.exists():
      return False
    meta = await self.get_session(session_id)
    await self._backend_destroy_hook(session_id, meta)
    await asyncio.to_thread(shutil.rmtree, session_dir)
    self._events_cache.pop(session_id, None)
    self._usage_cache.pop(session_id, None)
    self._aggregators.pop(session_id, None)
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
      self._events_cache.pop(session_id, None)
      self._usage_cache.pop(session_id, None)
      self._aggregators.pop(session_id, None)

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
      except Exception as e:
        log.exception("thread_gc_failed", thread=str(thread_dir))
    return deleted

  def _archive_old_chat_events_sync(self, session_id: str, cutoff_utc: datetime) -> dict:
    """Split live chat_events.jsonl at cutoff_utc, append the head to a weekly archive."""
    live_path = self._chat_events_path(session_id)
    if not live_path.exists():
      return {"events_archived": 0, "archive_file": None}

    archived_raw: list[str] = []
    kept_raw: list[str] = []
    split_reached = False
    with open(live_path, "r", encoding="utf-8") as f:
      for line in f:
        raw = line.rstrip("\n")
        if split_reached:
          kept_raw.append(raw)
          continue
        stripped = raw.strip()
        if not stripped:
          continue
        try:
          event = json.loads(stripped)
        except json.JSONDecodeError as e:
          log.debug("chat_event_archive_parse_skip", session_id=session_id, error=str(e))
          split_reached = True
          kept_raw.append(raw)
          continue
        ts_raw = event.get("timestamp")
        if not ts_raw:
          split_reached = True
          kept_raw.append(raw)
          continue
        try:
          ts = parse_utc_datetime(ts_raw)
        except ValueError as e:
          log.debug("chat_event_archive_ts_parse_skip", session_id=session_id, error=str(e))
          split_reached = True
          kept_raw.append(raw)
          continue
        if ts < cutoff_utc:
          archived_raw.append(raw)
        else:
          split_reached = True
          kept_raw.append(raw)

    if not archived_raw:
      log.info("chat_events_archive_noop", session_id=session_id)
      return {"events_archived": 0, "archive_file": None}

    iso = cutoff_utc.isocalendar()
    archives_dir = self._session_dir(session_id) / "data" / "archives"
    archives_dir.mkdir(parents=True, exist_ok=True)
    archive_path = archives_dir / f"chat_events.{iso.year}-W{iso.week:02d}.jsonl"
    with open(archive_path, "a", encoding="utf-8") as f:
      for raw in archived_raw:
        f.write(raw + "\n")

    tmp_path = live_path.with_suffix(live_path.suffix + ".tmp")
    with open(tmp_path, "w", encoding="utf-8") as f:
      for raw in kept_raw:
        f.write(raw + "\n")
    os.replace(tmp_path, live_path)

    return {
        "events_archived": len(archived_raw),
        "archive_file": str(archive_path),
    }

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

  async def persist_cc_session_id(self, session_id: str, cc_session_id: str) -> None:
    """Persist a cc_session_id without clobbering unrelated metadata fields."""
    async with self._lock_for(session_id):
      fresh = await self.get_session(session_id)
      if fresh:
        fresh.cc_session_id = cc_session_id
        await self._save_metadata(fresh)

  async def update_thinking_state(
      self,
      session_id: str,
      thinking_since: Optional[datetime] | object = _UNSET,
      updated_at: Optional[datetime] = None,
  ) -> None:
    """Persist thinking_since and/or updated_at without clobbering unrelated fields.

    Re-reads fresh metadata from disk before writing, so concurrent changes
    to fields like 'group' are preserved.
    """
    async with self._lock_for(session_id):
      self._invalidate_cache(session_id)
      fresh = await self.get_session(session_id)
      if fresh:
        if thinking_since is not _UNSET:
          fresh.thinking_since = thinking_since
        if updated_at is not None:
          fresh.updated_at = updated_at
        await self._save_metadata(fresh)

  async def clear_thinking_since(self, session_id: str, cc_session_id: Optional[str] = None) -> None:
    """Clear thinking_since without clobbering unrelated metadata fields (e.g. has_unread).

    If cc_session_id is provided, also persist it (avoids race with persist_cc_session_id).
    """
    async with self._lock_for(session_id):
      fresh = await self.get_session(session_id)
      if fresh:
        fresh.thinking_since = None
        if cc_session_id and fresh.cc_session_id != cc_session_id:
          fresh.cc_session_id = cc_session_id
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
      meta_path = d / "metadata.json"
      if not meta_path.exists():
        continue
      try:
        raw = meta_path.read_text(encoding="utf-8")
        meta = SessionMetadata.model_validate_json(raw)
        if meta.status == SessionStatus.ACTIVE:
          self._metadata_cache[d.name] = (meta, now)
          results.append(meta.model_copy())
      except (json.JSONDecodeError, OSError, ValueError) as e:
        log.debug("list_active_ids_skip", dir=d.name, error=str(e))
    return results

  # ---------------------------------------------------------------------------
  # Chat event persistence (NDJSON — for WebSocket catch-up)
  # ---------------------------------------------------------------------------

  async def save_chat_event(self, session_id: str, event: dict) -> None:
    """Append a single NDJSON event line to chat_events.jsonl."""
    if 'id' not in event:
      event['id'] = str(uuid.uuid4())
    if 'timestamp' not in event:
      event['timestamp'] = utc_now().isoformat()
    await append_ndjson(self._chat_events_path(session_id), event)
    # Keep in-memory cache in sync
    if session_id in self._events_cache:
      self._events_cache[session_id].append(event)
    # Incrementally update usage cache on result events
    if event.get('type') == 'result':
      self._update_usage_cache(session_id, event)

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
    event['event_index'] = archive_offset + len(self._events_cache[session_id]) - 1

    channel = f"session:{session_id}"
    deltas = list(aggregator.feed(event))
    for delta in deltas:
      await streaming_manager.broadcast(channel, delta)
    if event.get('type') not in _RAW_EVENTS_REPLACED_BY_DELTAS:
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
        clear_thinking_since=self.clear_thinking_since,
    )

  def load_chat_events_sync(self, session_id: str) -> list[dict]:
    """Read all chat events for catch-up. Uses in-memory cache after first read."""
    if session_id in self._events_cache:
      return self._events_cache[session_id]
    events = parse_ndjson_file(self._chat_events_path(session_id))
    self._events_cache[session_id] = events
    # Always derive usage from a full load so the cache stays exact.
    usage = self.usage_from_events(events)
    if usage:
      self._usage_cache[session_id] = usage
    else:
      self._usage_cache.pop(session_id, None)
    return events

  def load_chat_events_tail(self, session_id: str, limit: int = 200) -> tuple[list[dict], int, bool]:
    """Load only the last *limit* events from disk. Does NOT populate _events_cache.

    Returns (events, total_line_count, has_more).
    """
    events, total, has_more = parse_ndjson_tail(self._chat_events_path(session_id), limit)
    return events, total, has_more

  def get_chat_event_count_sync(self, session_id: str, session_meta: SessionMetadata | None = None) -> int:
    """Return the current global chat event count without parsing event payloads."""
    if session_meta is not None:
      archive_offset = session_meta.archive_offset
    else:
      archive_offset = self._read_archive_offset_sync(session_id)
    if session_id in self._events_cache:
      return archive_offset + len(self._events_cache[session_id])
    return archive_offset + count_ndjson_lines(self._chat_events_path(session_id))

  def load_chat_events_range(self, session_id: str, start: int, end: int) -> tuple[list[dict], bool]:
    """Load events in GLOBAL index range [start, end). Returns (events, has_more).

    Indices are global (archive_offset + line_in_live_file). When the requested
    range starts before the live file, archived chat_events files under
    ``data/archives/`` are read in chronological order to fill the gap.
    """
    if end <= start:
      return [], start > 0
    archive_offset = self._read_archive_offset_sync(session_id)
    live_path = self._chat_events_path(session_id)
    if end <= archive_offset:
      return self._load_archive_range(session_id, start, end), start > 0
    if start >= archive_offset:
      rel_start = start - archive_offset
      rel_end = end - archive_offset
      events, _ = parse_ndjson_range(live_path, rel_start, rel_end)
      return events, start > 0
    archive_events = self._load_archive_range(session_id, start, archive_offset)
    live_end = end - archive_offset
    live_events, _ = parse_ndjson_range(live_path, 0, live_end)
    return archive_events + live_events, start > 0

  def _read_archive_offset_sync(self, session_id: str) -> int:
    """Synchronously read the archive_offset from metadata.json.

    Used by sync read paths (e.g. ``load_chat_events_range``) so they don't
    have to go async just to learn the live/archive split. Falls back to 0 if
    the metadata file is missing or unreadable.
    """
    cached = self._metadata_cache.get(session_id)
    if cached is not None:
      return cached[0].archive_offset
    path = self._metadata_path(session_id)
    if not path.exists():
      return 0
    try:
      raw = path.read_text(encoding="utf-8")
      if not raw.strip():
        return 0
      return SessionMetadata.model_validate_json(raw).archive_offset
    except (OSError, ValueError) as e:
      log.debug("archive_offset_read_failed", session_id=session_id, error=str(e))
      return 0

  def _load_archive_range(self, session_id: str, start: int, end: int) -> list[dict]:
    """Read events at global indices [start, end) from archive files.

    Archives live in ``<session>/data/archives/chat_events.<YYYY>-W<WW>.jsonl``.
    Files are walked in chronological order (filename sort happens to match).
    """
    if end <= start:
      return []
    archives_dir = self._session_dir(session_id) / "data" / "archives"
    if not archives_dir.exists():
      return []
    archive_files = sorted(archives_dir.glob("chat_events.*.jsonl"))
    events: list[dict] = []
    cursor = 0
    for path in archive_files:
      if cursor >= end:
        break
      try:
        with open(path, "r", encoding="utf-8") as f:
          for line in f:
            if cursor >= end:
              break
            if cursor >= start:
              stripped = line.strip()
              if stripped:
                try:
                  events.append(json.loads(stripped))
                except json.JSONDecodeError as e:
                  log.debug("archive_parse_skip", session_id=session_id, error=str(e))
            cursor += 1
      except OSError as e:
        log.debug("archive_read_failed", path=str(path), error=str(e))
    return events

  def _chat_events_path(self, session_id: str) -> Path:
    return self._session_dir(session_id) / "data" / "chat_events.jsonl"

  async def resolve_session_usage(
      self,
      session_id: str,
      session_meta: SessionMetadata,
      events: list[dict] | None = None,
  ) -> dict | None:
    """Resolve display usage for a session view.

    Non-Codex backends keep the existing CharlieBot result-derived behavior.
    Codex backends override context usage from the native rollout log when the
    native thread id can be resolved.
    """
    usage = self.get_usage_cached(session_id)
    if usage is None and events is not None:
      usage = self.usage_from_events(events)
      if usage is not None:
        self._usage_cache[session_id] = usage
    if usage is None and events is None:
      await asyncio.to_thread(self.load_chat_events_sync, session_id)
      usage = self.get_usage_cached(session_id)

    if not self._codex_resolver.is_codex_backend(session_meta.backend):
      return usage

    merged = await asyncio.to_thread(
        self._codex_resolver.resolve, session_id, session_meta.cc_session_id, events, usage)
    if merged is not None:
      self._usage_cache[session_id] = merged
      return merged
    return usage

  # ---------------------------------------------------------------------------
  # Usage / token tracking
  # ---------------------------------------------------------------------------

  @staticmethod
  def usage_from_events(events: list[dict]) -> dict | None:
    """Extract context-window token usage from pre-loaded events.

    Scans for the most recent 'result' event and accumulates total_cost_usd
    across ALL result events.

    Returns a dict with:
      context_tokens  – input + cache_creation + cache_read from last result
      context_limit   – from modelUsage contextWindow (default 200000)
      total_cost_usd  – sum across every result event
      model           – primary model name
    Returns None if no result events exist.
    """
    if not events:
      return None

    last_result: dict | None = None
    last_usage_result: dict | None = None
    total_cost = 0.0
    unknown_cost = False

    for ev in events:
      if ev.get("type") != ET.RESULT:
        continue
      last_result = ev
      event_cost = ev.get("total_cost_usd", 0.0)
      if event_cost is None:
        unknown_cost = True
      else:
        total_cost += event_cost
      if _extract_usage_from_result(ev)[0] > 0:
        last_usage_result = ev

    if last_result is None:
      return None

    context_tokens, context_limit, model = _extract_usage_from_result(last_usage_result or last_result)
    return {
        "context_tokens": context_tokens,
        "context_limit": context_limit,
        "total_cost_usd": None if unknown_cost else round(total_cost, 4),
        "model": model,
    }

  def get_usage_cached(self, session_id: str) -> dict | None:
    """Return cached usage data for a session, or None if not yet computed."""
    return self._usage_cache.get(session_id)

  def _update_usage_cache(self, session_id: str, result_event: dict) -> None:
    """Incrementally update the usage cache from a single 'result' event."""
    cached = self._usage_cache.get(session_id) or {
        "context_tokens": 0,
        "context_limit": _DEFAULT_CONTEXT_LIMIT,
        "total_cost_usd": 0.0,
        "model": "",
    }
    event_cost = result_event.get("total_cost_usd", 0.0)
    if event_cost is None:
      cached["total_cost_usd"] = None
    elif cached["total_cost_usd"] is not None:
      cached["total_cost_usd"] = round(cached["total_cost_usd"] + event_cost, 4)
    ctx, context_limit, model = _extract_usage_from_result(result_event)
    if ctx > 0:
      cached["context_tokens"] = ctx
      cached["context_limit"] = context_limit
      cached["model"] = model
    self._usage_cache[session_id] = cached

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
    """Load all session metadata from disk concurrently.

    Performs the standard three-step preamble shared by every listing entry point:
    (1) return [] if sessions_dir does not exist, (2) list session directories under
    asyncio.to_thread to avoid blocking the event loop, (3) concurrently load each
    session's metadata via asyncio.gather, logging and dropping any that fail to load.
    """
    if not self._cfg.sessions_dir.exists():
      return []
    dirs = await asyncio.to_thread(lambda: [d for d in self._cfg.sessions_dir.iterdir() if d.is_dir()])
    all_meta = await asyncio.gather(*(self.get_session(d.name) for d in dirs), return_exceptions=True)
    result: list[SessionMetadata] = []
    for i, meta in enumerate(all_meta):
      if isinstance(meta, BaseException):
        log.warning('session_load_failed', session_id=dirs[i].name, error=str(meta))
        continue
      if not meta:
        continue
      result.append(meta)
    return result

  async def _active_scheduled_sessions(
      self,
      task_name: str,
      session_cache: Optional[dict[str, list[SessionMetadata]]] = None,
  ) -> list[SessionMetadata]:
    """Return active scheduled sessions for task_name, newest first."""
    if session_cache is not None:
      candidates = session_cache.get(task_name, [])
    else:
      candidates = await self.list_sessions(status=SessionStatus.ACTIVE, scheduled=True)
    sessions = [
        session for session in candidates
        if session.scheduled_task == task_name and session.status == SessionStatus.ACTIVE
    ]
    sessions.sort(key=lambda session: session.updated_at, reverse=True)
    return sessions

  async def _scheduled_session_busy(self, session: SessionMetadata) -> bool:
    """Return whether a scheduled session has active master thinking or worker threads."""
    return bool(session.thinking_since) or await self._has_running_tasks(session.id)

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

  async def _enrich_and_sort(
      self,
      sessions: list[SessionMetadata],
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
  ) -> list[SessionMetadata]:
    """Populate sidebar state and sort newest first."""
    await self._populate_sidebar_state(
        sessions,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
    )
    sessions.sort(key=lambda s: s.updated_at, reverse=True)
    return sessions

  async def populate_sidebar_state(
      self,
      sessions: list[SessionMetadata],
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
  ) -> None:
    """Public wrapper for _populate_sidebar_state."""
    await self._populate_sidebar_state(
        sessions,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
    )

  async def _populate_sidebar_state(
      self,
      sessions: list[SessionMetadata],
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
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

    if not active_sessions:
      return

    running_future = None
    trigger_future = None
    if include_running_status:
      running_future = asyncio.gather(*(self._has_running_tasks(meta.id) for meta in active_sessions))
    if include_pending_trigger_status:
      trigger_future = asyncio.gather(*(self._get_pending_trigger_state(meta.id) for meta in active_sessions))

    if running_future and trigger_future:
      running_flags, trigger_states = await asyncio.gather(running_future, trigger_future)
    elif running_future:
      running_flags = await running_future
      trigger_states = None
    elif trigger_future:
      running_flags = None
      trigger_states = await trigger_future
    else:
      return

    if running_flags is not None:
      for meta, running in zip(active_sessions, running_flags):
        meta.has_running_tasks = bool(meta.thinking_since) or running

    if trigger_states is not None:
      for meta, (pending_count, next_trigger_at) in zip(active_sessions, trigger_states):
        meta.has_pending_trigger = pending_count > 0
        meta.pending_trigger_count = pending_count
        meta.next_trigger_at = next_trigger_at

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
    async with aiofiles.open(path, "w") as f:
      await f.write(serialized)
    self._metadata_cache[meta.id] = (SessionMetadata.model_validate_json(serialized), time.monotonic())

  def _session_dir(self, session_id: str) -> Path:
    return self._cfg.sessions_dir / session_id

  def _metadata_path(self, session_id: str) -> Path:
    return self._session_dir(session_id) / "metadata.json"
