"""Session management for CharlieBot."""

import asyncio
import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import aiofiles
import structlog

from src.core.config import CharlieBotConfig
from src.core.models import (
    CreateSessionRequest,
    SessionMetadata,
    SessionStatus,
)
from src.core.ndjson import append_ndjson, parse_ndjson_file
from src.core.streaming import streaming_manager

log = structlog.get_logger()


class SessionManager:
  """CRUD operations for CharlieBot sessions."""

  def __init__(self, cfg: CharlieBotConfig):
    self._cfg = cfg
    # In-memory cache: session_id -> list[dict] of parsed NDJSON events.
    # Populated on first read, kept in sync by save_chat_event().
    self._events_cache: dict[str, list[dict]] = {}

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
    """Load session metadata from disk."""
    path = self._metadata_path(session_id)
    if not path.exists():
      return None
    async with aiofiles.open(path, "r") as f:
      raw = await f.read()
    if not raw.strip():
      log.warning("session_metadata_empty", session_id=session_id, path=str(path))
      return None
    return SessionMetadata.model_validate_json(raw)

  async def list_sessions(
      self,
      status: Optional[SessionStatus] = None,
      starred: Optional[bool] = None,
      scheduled: Optional[bool] = None,
  ) -> list[SessionMetadata]:
    """List sessions, newest first. Optionally filter by status, starred, and/or scheduled."""
    sessions: list[SessionMetadata] = []
    if not self._cfg.sessions_dir.exists():
      return sessions
    for d in self._cfg.sessions_dir.iterdir():
      if not d.is_dir():
        continue
      meta = await self.get_session(d.name)
      if not meta:
        continue
      if status is not None and meta.status != status:
        continue
      if starred is not None and meta.starred != starred:
        continue
      if scheduled is not None and bool(meta.scheduled_task) != scheduled:
        continue
      meta.has_running_tasks = bool(meta.thinking_since) or await self._has_running_tasks(meta.id)
      sessions.append(meta)
    # Normalise to offset-aware (UTC) so naive vs aware datetimes don't explode
    for s in sessions:
      if s.updated_at.tzinfo is None:
        s.updated_at = s.updated_at.replace(tzinfo=timezone.utc)
    sessions.sort(key=lambda s: s.updated_at, reverse=True)
    return sessions

  async def search_sessions(self, query: str) -> list[SessionMetadata]:
    """Search active sessions by name and chat event content (case-insensitive)."""
    query_lower = query.lower()
    results: list[SessionMetadata] = []
    if not self._cfg.sessions_dir.exists():
      return results
    for d in self._cfg.sessions_dir.iterdir():
      if not d.is_dir():
        continue
      meta = await self.get_session(d.name)
      if not meta or meta.status not in (SessionStatus.ACTIVE, SessionStatus.WAITING):
        continue
      # Check session name first
      if query_lower in (meta.name or '').lower():
        meta.has_running_tasks = bool(meta.thinking_since) or await self._has_running_tasks(meta.id)
        results.append(meta)
        continue
      # Check chat events (offload sync I/O to thread pool)
      events_path = self._chat_events_path(meta.id)
      if events_path.exists():
        try:
          text = await asyncio.to_thread(events_path.read_text, encoding='utf-8')
          if query_lower in text.lower():
            meta.has_running_tasks = bool(meta.thinking_since) or await self._has_running_tasks(meta.id)
            results.append(meta)
        except OSError as e:
          log.debug('search_read_failed', session_id=meta.id, error=str(e))
    for s in results:
      if s.updated_at.tzinfo is None:
        s.updated_at = s.updated_at.replace(tzinfo=timezone.utc)
    results.sort(key=lambda s: s.updated_at, reverse=True)
    return results

  async def rewind_session(self, parent_id: str, event_index: int) -> Optional[SessionMetadata]:
    """Create a new session by rewinding an existing one to a specific event index.

    Copies chat_events.jsonl lines 0..event_index (inclusive) from the parent session,
    generates a context summary, and creates a new session with the rewound history.
    """
    parent = await self.get_session(parent_id)
    if not parent:
      return None

    # Read parent events and slice up to event_index (inclusive)
    parent_events_path = self._chat_events_path(parent_id)
    if not parent_events_path.exists():
      return None
    lines_text = await asyncio.to_thread(parent_events_path.read_text, encoding='utf-8')
    lines = lines_text.splitlines()
    kept_lines = lines[:event_index + 1]

    # Generate a text summary from user+assistant messages for CC context
    summary_parts = []
    for line_text in kept_lines:
      try:
        ev = json.loads(line_text)
      except (json.JSONDecodeError, ValueError):
        continue
      t = ev.get('type')
      if t == 'user' and 'content' in ev:
        summary_parts.append(f'User: {ev["content"]}')
      elif t == 'assistant':
        msg = ev.get('message') or {}
        blocks = msg.get('content') or []
        text = ''.join(b.get('text', '') for b in blocks if isinstance(b, dict) and b.get('type') == 'text')
        if text:
          summary_parts.append(f'Assistant: {text}')
    summary = '\n\n'.join(summary_parts)
    if len(summary) > 4000:
      summary = summary[:4000] + '\n\n[... truncated]'

    # Create the new session
    meta = SessionMetadata(name=f'Rewind: {parent.name}', parent_session_id=parent_id, rewind_summary=summary)
    session_dir = self._session_dir(meta.id)
    for subdir in ['data', 'threads']:
      (session_dir / subdir).mkdir(parents=True, exist_ok=True)

    # Write the truncated chat events
    events_path = self._chat_events_path(meta.id)
    await asyncio.to_thread(events_path.write_text, '\n'.join(kept_lines) + '\n', encoding='utf-8')

    await self._save_metadata(meta)

    log.info('session_rewound', new_session=meta.id, parent=parent_id, event_index=event_index)
    return meta

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
            "type": "unread_changed",
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
            "type": "unread_changed",
            "session_id": session_id,
            "has_unread": True,
        })

  async def archive_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Mark a session as archived (does not delete files)."""
    meta = await self.get_session(session_id)
    if not meta:
      return None
    meta.status = SessionStatus.ARCHIVED
    meta.updated_at = datetime.now(timezone.utc)
    await self._save_metadata(meta)
    self._events_cache.pop(session_id, None)
    log.info("session_archived", session_id=session_id)
    return meta

  async def unarchive_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Restore an archived session back to active."""
    meta = await self.get_session(session_id)
    if not meta:
      return None
    meta.status = SessionStatus.ACTIVE
    meta.updated_at = datetime.now(timezone.utc)
    await self._save_metadata(meta)
    log.info("session_unarchived", session_id=session_id)
    return meta

  async def mark_waiting(self, session_id: str) -> Optional[SessionMetadata]:
    """Mark a session as waiting for confirmation."""
    meta = await self.get_session(session_id)
    if not meta:
      return None
    meta.status = SessionStatus.WAITING
    meta.updated_at = datetime.now(timezone.utc)
    await self._save_metadata(meta)
    log.info("session_mark_waiting", session_id=session_id)
    return meta

  async def unmark_waiting(self, session_id: str) -> Optional[SessionMetadata]:
    """Restore a waiting session back to active."""
    meta = await self.get_session(session_id)
    if not meta:
      return None
    meta.status = SessionStatus.ACTIVE
    meta.updated_at = datetime.now(timezone.utc)
    await self._save_metadata(meta)
    log.info("session_unmark_waiting", session_id=session_id)
    return meta

  async def star_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Star a session."""
    meta = await self.get_session(session_id)
    if not meta:
      return None
    meta.starred = True
    meta.updated_at = datetime.now(timezone.utc)
    await self._save_metadata(meta)
    log.info("session_starred", session_id=session_id)
    return meta

  async def unstar_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Unstar a session."""
    meta = await self.get_session(session_id)
    if not meta:
      return None
    meta.starred = False
    meta.updated_at = datetime.now(timezone.utc)
    await self._save_metadata(meta)
    log.info("session_unstarred", session_id=session_id)
    return meta

  async def save_metadata(self, meta: SessionMetadata) -> None:
    """Public wrapper for _save_metadata."""
    await self._save_metadata(meta)

  def list_active_session_ids(self) -> list[str]:
    """Return IDs of active sessions by reading only metadata.json status.

    Sync method — skips _has_running_tasks and full SessionMetadata hydration,
    much cheaper than list_sessions() for the /usage endpoint.
    """
    if not self._cfg.sessions_dir.exists():
      return []
    ids: list[str] = []
    for d in self._cfg.sessions_dir.iterdir():
      if not d.is_dir():
        continue
      meta_path = d / "metadata.json"
      if not meta_path.exists():
        continue
      try:
        raw = json.loads(meta_path.read_text(encoding="utf-8"))
        if raw.get("status") == SessionStatus.ACTIVE:
          ids.append(d.name)
      except (json.JSONDecodeError, OSError) as e:
        log.debug("list_active_ids_skip", dir=d.name, error=str(e))
    return ids

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

  def load_chat_events_sync(self, session_id: str) -> list[dict]:
    """Read all chat events for catch-up. Uses in-memory cache after first read."""
    if session_id in self._events_cache:
      return self._events_cache[session_id]
    events = parse_ndjson_file(self._chat_events_path(session_id))
    self._events_cache[session_id] = events
    return events

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

  # ---------------------------------------------------------------------------
  # Private helpers
  # ---------------------------------------------------------------------------

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
        except (json.JSONDecodeError, OSError):
          continue
      return False

    return await asyncio.to_thread(_check)

  async def _next_session_name(self) -> str:
    """Generate 'Session 0', 'Session 1', etc. based on existing count."""
    existing = await self.list_sessions()
    return f"Session {len(existing)}"

  async def _save_metadata(self, meta: SessionMetadata) -> None:
    path = self._metadata_path(meta.id)
    path.parent.mkdir(parents=True, exist_ok=True)
    async with aiofiles.open(path, "w") as f:
      await f.write(meta.model_dump_json(indent=2))

  def _session_dir(self, session_id: str) -> Path:
    return self._cfg.sessions_dir / session_id

  def _metadata_path(self, session_id: str) -> Path:
    return self._session_dir(session_id) / "metadata.json"
