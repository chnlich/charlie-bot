"""Session management for CharlieBot."""

import asyncio
import json
import shutil
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

import aiofiles
import structlog

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import (
    CreateSessionRequest,
    SessionMetadata,
    SessionStatus,
)
from src.core.codex_usage import CodexUsageResolver
from src.core.ndjson import append_ndjson, parse_ndjson_file, parse_ndjson_range, parse_ndjson_tail
from src.core.streaming import streaming_manager

log = structlog.get_logger()

_METADATA_CACHE_TTL = 5.0  # seconds
_UNSET = object()
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

    log.info("session_created", session_id=meta.id, name=meta.name)
    return meta

  async def get_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Load session metadata, using in-memory cache when available."""
    cached = self._metadata_cache.get(session_id)
    if cached is not None:
      meta, ts = cached
      if (time.monotonic() - ts) < _METADATA_CACHE_TTL:
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
    if not self._cfg.sessions_dir.exists():
      return []
    dirs = await asyncio.to_thread(lambda: [d for d in self._cfg.sessions_dir.iterdir() if d.is_dir()])
    all_meta = await asyncio.gather(*(self.get_session(d.name) for d in dirs), return_exceptions=True)
    sessions: list[SessionMetadata] = []
    for i, meta in enumerate(all_meta):
      if isinstance(meta, BaseException):
        log.warning('session_load_failed', session_id=dirs[i].name, error=str(meta))
        continue
      if not meta:
        continue
      if status is not None and meta.status != status:
        continue
      if starred is not None and meta.starred != starred:
        continue
      if scheduled is not None and bool(meta.scheduled_task) != scheduled:
        continue
      sessions.append(meta)
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
    if not self._cfg.sessions_dir.exists():
      return []
    dirs = await asyncio.to_thread(lambda: [d for d in self._cfg.sessions_dir.iterdir() if d.is_dir()])
    all_meta = await asyncio.gather(*(self.get_session(d.name) for d in dirs))
    results: list[SessionMetadata] = []
    content_candidates: list[tuple[SessionMetadata, Path]] = []
    for meta in all_meta:
      if not meta or meta.status != SessionStatus.ACTIVE:
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
        name=f'Clone: {parent.name}',
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
        "timestamp": datetime.now(timezone.utc).isoformat(),
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
        name=f'Elon-e: {parent.name}',
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

    # Auto-archive and thumbs-down the parent
    parent.status = SessionStatus.ARCHIVED
    parent.rating = 'thumbs_down'
    parent.updated_at = datetime.now(timezone.utc)
    await self._save_metadata(parent)
    self._events_cache.pop(parent_id, None)
    self._usage_cache.pop(parent_id, None)

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
    meta = await self.get_session(session_id)
    if not meta:
      return None
    meta.name = new_name
    meta.updated_at = datetime.now(timezone.utc)
    await self._save_metadata(meta)
    log.info("session_renamed", session_id=session_id, new_name=new_name)
    return meta

  async def mark_read(self, session_id: str) -> Optional[SessionMetadata]:
    """Clear the unread flag for a session."""
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
    return meta

  async def delete_session_permanently(self, session_id: str) -> bool:
    """Permanently delete a session and all its data from disk."""
    session_dir = self._session_dir(session_id)
    if not session_dir.exists():
      return False
    await asyncio.to_thread(shutil.rmtree, session_dir)
    self._events_cache.pop(session_id, None)
    self._usage_cache.pop(session_id, None)
    self._invalidate_cache(session_id)
    log.info("session_deleted_permanently", session_id=session_id)
    return True

  async def unarchive_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Restore an archived session back to active."""
    return await self._update_field(session_id, "status", SessionStatus.ACTIVE, "session_unarchived")

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
      if meta.group == old_name:
        meta.group = new_name
        meta.updated_at = datetime.now(timezone.utc)
        await self._save_metadata(meta)
        count += 1
    if count:
      log.info("group_renamed", old_name=old_name, new_name=new_name, count=count)
    return count

  async def delete_group(self, group: str) -> int:
    """Remove a group from all sessions (set to null). Returns the count of updated sessions."""
    all_sessions = await self.list_sessions()
    count = 0
    for meta in all_sessions:
      if meta.group == group:
        meta.group = None
        meta.updated_at = datetime.now(timezone.utc)
        await self._save_metadata(meta)
        count += 1
    if count:
      log.info("group_deleted", group=group, count=count)
    return count

  async def save_metadata(self, meta: SessionMetadata) -> None:
    """Public wrapper for _save_metadata."""
    await self._save_metadata(meta)

  async def persist_cc_session_id(self, session_id: str, cc_session_id: str) -> None:
    """Persist a cc_session_id without clobbering unrelated metadata fields."""
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
    if 'timestamp' not in event:
      event['timestamp'] = datetime.now(timezone.utc).isoformat()
    await append_ndjson(self._chat_events_path(session_id), event)
    # Keep in-memory cache in sync
    if session_id in self._events_cache:
      self._events_cache[session_id].append(event)
    # Incrementally update usage cache on result events
    if event.get('type') == 'result':
      self._update_usage_cache(session_id, event)

  async def persist_and_broadcast(self, session_id: str, event: dict) -> None:
    """Persist event (injecting timestamp) then broadcast to the session WebSocket channel."""
    await self.save_chat_event(session_id, event)
    # Inject event_index so the frontend can render clone/elone buttons in real-time.
    if session_id in self._events_cache:
      event['event_index'] = len(self._events_cache[session_id]) - 1
    await streaming_manager.broadcast(f"session:{session_id}", event)

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

  def load_chat_events_range(self, session_id: str, start: int, end: int) -> tuple[list[dict], bool]:
    """Load events in line range [start, end). Returns (events, has_more)."""
    return parse_ndjson_range(self._chat_events_path(session_id), start, end)

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

    for ev in events:
      if ev.get("type") != "result":
        continue
      last_result = ev
      total_cost += ev.get("total_cost_usd", 0.0)
      u = ev.get("usage", {})
      if u.get("input_tokens", 0) + u.get("cache_creation_input_tokens", 0) + u.get("cache_read_input_tokens", 0) > 0:
        last_usage_result = ev

    if last_result is None:
      return None

    usage_source = last_usage_result or last_result
    usage = usage_source.get("usage", {})
    context_tokens = (
        usage.get("input_tokens", 0) + usage.get("cache_creation_input_tokens", 0) +
        usage.get("cache_read_input_tokens", 0))

    model_usage = usage_source.get("modelUsage", {})
    context_limit = 200_000
    model = ""
    for model_name, info in model_usage.items():
      model = model_name
      context_limit = info.get("contextWindow", 200_000)
      break

    return {
        "context_tokens": context_tokens,
        "context_limit": context_limit,
        "total_cost_usd": round(total_cost, 4),
        "model": model,
    }

  def get_usage_cached(self, session_id: str) -> dict | None:
    """Return cached usage data for a session, or None if not yet computed."""
    return self._usage_cache.get(session_id)

  def _update_usage_cache(self, session_id: str, result_event: dict) -> None:
    """Incrementally update the usage cache from a single 'result' event."""
    cached = self._usage_cache.get(session_id) or {
        "context_tokens": 0,
        "context_limit": 200_000,
        "total_cost_usd": 0.0,
        "model": "",
    }
    cached["total_cost_usd"] = round(cached["total_cost_usd"] + result_event.get("total_cost_usd", 0.0), 4)
    u = result_event.get("usage", {})
    ctx = u.get("input_tokens", 0) + u.get("cache_creation_input_tokens", 0) + u.get("cache_read_input_tokens", 0)
    if ctx > 0:
      cached["context_tokens"] = ctx
      model_usage = result_event.get("modelUsage", {})
      for model_name, info in model_usage.items():
        cached["model"] = model_name
        cached["context_limit"] = info.get("contextWindow", 200_000)
        break
    self._usage_cache[session_id] = cached

  # ---------------------------------------------------------------------------
  # Private helpers
  # ---------------------------------------------------------------------------

  def _invalidate_cache(self, session_id: str) -> None:
    """Remove a session from the metadata cache."""
    self._metadata_cache.pop(session_id, None)

  async def _update_field(self, session_id: str, field: str, value: Any, log_event: str) -> Optional[SessionMetadata]:
    """Get a session, set one field, save, and log. Returns None if session not found."""
    meta = await self.get_session(session_id)
    if not meta:
      return None
    setattr(meta, field, value)
    meta.updated_at = datetime.now(timezone.utc)
    await self._save_metadata(meta)
    log.info(log_event, session_id=session_id)
    return meta

  async def _has_running_tasks(self, session_id: str) -> bool:
    """Check if a session has any threads with status 'running'."""

    def _check():
      threads_dir = self._session_dir(session_id) / "threads"
      if not threads_dir.exists():
        return False
      for thread_dir in threads_dir.iterdir():
        meta_path = thread_dir / "metadata.json"
        if not meta_path.exists():
          continue
        try:
          meta = json.loads(meta_path.read_text(encoding="utf-8"))
          if meta.get("status") == "running":
            return True
        except (json.JSONDecodeError, OSError) as e:
          log.debug('thread_meta_read_failed', thread_dir=thread_dir.name, error=str(e))
          continue
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
        try:
          trigger = json.loads(trigger_path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError, ValueError) as e:
          log.debug("trigger_meta_read_failed", trigger_path=str(trigger_path), error=str(e))
          continue
        if trigger.get("status") != "pending":
          continue

        pending_count += 1
        fire_at_raw = trigger.get("fire_at")
        if not fire_at_raw:
          continue
        try:
          fire_at = datetime.fromisoformat(fire_at_raw.replace("Z", "+00:00"))
        except ValueError as e:
          log.debug("trigger_fire_at_parse_failed", trigger_path=str(trigger_path), error=str(e))
          continue
        if fire_at.tzinfo is None:
          fire_at = fire_at.replace(tzinfo=timezone.utc)
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

    running_future = None
    trigger_future = None
    if include_running_status:
      running_future = asyncio.gather(*(self._has_running_tasks(meta.id) for meta in sessions))
    if include_pending_trigger_status:
      trigger_future = asyncio.gather(*(self._get_pending_trigger_state(meta.id) for meta in sessions))

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
      for meta, running in zip(sessions, running_flags):
        meta.has_running_tasks = bool(meta.thinking_since) or running

    if trigger_states is not None:
      for meta, (pending_count, next_trigger_at) in zip(sessions, trigger_states):
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
