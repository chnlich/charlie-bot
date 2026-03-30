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
from src.api.message_utils import extract_text_from_message
from src.core.ndjson import append_ndjson, parse_ndjson_file, parse_ndjson_range, parse_ndjson_tail
from src.core.streaming import streaming_manager

log = structlog.get_logger()

_METADATA_CACHE_TTL = 5.0  # seconds


def _summarize_event_lines(lines: list[str]) -> str:
  """Parse JSONL event lines and build a user/assistant text summary, truncated to 4000 chars."""
  summary_parts = []
  for line_text in lines:
    try:
      ev = json.loads(line_text)
    except (json.JSONDecodeError, ValueError) as e:
      log.debug('summary_parse_skip', line=line_text[:120], error=str(e))
      continue
    t = ev.get('type')
    if t == 'user' and 'content' in ev:
      summary_parts.append(f'User: {ev["content"]}')
    elif t == 'assistant':
      text = extract_text_from_message(ev.get('message'))
      if text:
        summary_parts.append(f'Assistant: {text}')
  summary = '\n\n'.join(summary_parts)
  if len(summary) > 4000:
    summary = summary[:4000] + '\n\n[... truncated]'
  return summary


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

  # ---------------------------------------------------------------------------
  # Session CRUD
  # ---------------------------------------------------------------------------

  async def create_session(self, req: CreateSessionRequest, backend: str | None = None) -> SessionMetadata:
    """Create a new session."""
    name = req.name or await self._next_session_name()
    meta = SessionMetadata(name=name, scheduled_task=req.scheduled_task, backend=backend or "claude-opus-4.6")

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
  ) -> list[SessionMetadata]:
    """List sessions, newest first. Optionally filter by status, starred, and/or scheduled."""
    if not self._cfg.sessions_dir.exists():
      return []
    dirs = await asyncio.to_thread(lambda: [d for d in self._cfg.sessions_dir.iterdir() if d.is_dir()])
    all_meta = await asyncio.gather(*(self.get_session(d.name) for d in dirs))
    sessions: list[SessionMetadata] = []
    for meta in all_meta:
      if not meta:
        continue
      if status is not None and meta.status != status:
        continue
      if starred is not None and meta.starred != starred:
        continue
      if scheduled is not None and bool(meta.scheduled_task) != scheduled:
        continue
      sessions.append(meta)
    if include_running_status:
      running_flags = await asyncio.gather(*(self._has_running_tasks(m.id) for m in sessions))
      for meta, running in zip(sessions, running_flags):
        meta.has_running_tasks = bool(meta.thinking_since) or running
    # Normalise to offset-aware (UTC) so naive vs aware datetimes don't explode
    for s in sessions:
      if s.updated_at.tzinfo is None:
        s.updated_at = s.updated_at.replace(tzinfo=timezone.utc)
    sessions.sort(key=lambda s: s.updated_at, reverse=True)
    return sessions

  async def search_sessions(
      self,
      query: str,
      include_running_status: bool = False,
  ) -> list[SessionMetadata]:
    """Search active sessions by name and chat event content (case-insensitive)."""
    query_lower = query.lower()
    if not self._cfg.sessions_dir.exists():
      return []
    dirs = await asyncio.to_thread(lambda: [d for d in self._cfg.sessions_dir.iterdir() if d.is_dir()])
    all_meta = await asyncio.gather(*(self.get_session(d.name) for d in dirs))
    results: list[SessionMetadata] = []
    for meta in all_meta:
      if not meta or meta.status not in (SessionStatus.ACTIVE, SessionStatus.WAITING):
        continue
      # Check session name first
      if query_lower in (meta.name or '').lower():
        results.append(meta)
        continue
      # Check chat events (offload sync I/O to thread pool)
      events_path = self._chat_events_path(meta.id)
      if events_path.exists():
        try:
          text = await asyncio.to_thread(events_path.read_text, encoding='utf-8')
          if query_lower in text.lower():
            results.append(meta)
        except OSError as e:
          log.debug('search_read_failed', session_id=meta.id, error=str(e))
    if include_running_status:
      running_flags = await asyncio.gather(*(self._has_running_tasks(m.id) for m in results))
      for meta, running in zip(results, running_flags):
        meta.has_running_tasks = bool(meta.thinking_since) or running
    for s in results:
      if s.updated_at.tzinfo is None:
        s.updated_at = s.updated_at.replace(tzinfo=timezone.utc)
    results.sort(key=lambda s: s.updated_at, reverse=True)
    return results

  async def fork_session(self, parent_id: str, event_index: int | None = None) -> Optional[SessionMetadata]:
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

    summary = _summarize_event_lines(lines)

    # Create the new session inheriting parent's backend
    meta = SessionMetadata(
      name=f'Clone: {parent.name}',
      parent_session_id=parent_id,
      rewind_summary=summary,
      backend=parent.backend,
    )
    session_dir = self._session_dir(meta.id)
    for subdir in ['data', 'threads']:
      (session_dir / subdir).mkdir(parents=True, exist_ok=True)

    # Copy chat events to new session
    events_path = self._chat_events_path(meta.id)
    await asyncio.to_thread(events_path.write_text, '\n'.join(lines) + '\n', encoding='utf-8')

    await self._save_metadata(meta)

    log.info('session_cloned', new_session=meta.id, parent=parent_id, event_index=event_index)
    return meta

  async def elone_session(self, parent_id: str, event_index: int) -> Optional[SessionMetadata]:
    """Create an Elon-e session: empty history, archive + thumbs-down the parent."""
    parent = await self.get_session(parent_id)
    if not parent:
      return None

    # Create new session with empty history
    meta = SessionMetadata(
      name=f'Elon-e: {parent.name}',
      parent_session_id=parent_id,
      backend=parent.backend,
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

    log.info('session_eloned', new_session=meta.id, parent=parent_id, event_index=event_index)
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

  async def mark_waiting(self, session_id: str) -> Optional[SessionMetadata]:
    """Mark a session as waiting for confirmation."""
    return await self._update_field(session_id, "status", SessionStatus.WAITING, "session_mark_waiting")

  async def unmark_waiting(self, session_id: str) -> Optional[SessionMetadata]:
    """Restore a waiting session back to active."""
    return await self._update_field(session_id, "status", SessionStatus.ACTIVE, "session_unmark_waiting")

  async def star_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Star a session."""
    return await self._update_field(session_id, "starred", True, "session_starred")

  async def unstar_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Unstar a session."""
    return await self._update_field(session_id, "starred", False, "session_unstarred")

  async def save_metadata(self, meta: SessionMetadata) -> None:
    """Public wrapper for _save_metadata."""
    await self._save_metadata(meta)

  async def persist_cc_session_id(self, session_id: str, cc_session_id: str) -> None:
    """Persist a cc_session_id without clobbering unrelated metadata fields."""
    fresh = await self.get_session(session_id)
    if fresh:
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
        "context_tokens": 0, "context_limit": 200_000, "total_cost_usd": 0.0, "model": "",
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
    async with aiofiles.open(path, "w") as f:
      await f.write(meta.model_dump_json(indent=2))
    self._metadata_cache[meta.id] = (meta.model_copy(), time.monotonic())

  def _session_dir(self, session_id: str) -> Path:
    return self._cfg.sessions_dir / session_id

  def _metadata_path(self, session_id: str) -> Path:
    return self._session_dir(session_id) / "metadata.json"
