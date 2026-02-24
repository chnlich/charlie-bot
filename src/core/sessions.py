"""Session management for CharlieBot."""

import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import aiofiles
import structlog

from src.agents.master_cc import ensure_master_claude_md
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

  # ---------------------------------------------------------------------------
  # Session CRUD
  # ---------------------------------------------------------------------------

  async def create_session(self, req: CreateSessionRequest) -> SessionMetadata:
    """Create a new session."""
    name = req.name or await self._next_session_name()
    meta = SessionMetadata(name=name)

    session_dir = self._session_dir(meta.id)
    # Create directory structure
    for subdir in ["data", "threads"]:
      (session_dir / subdir).mkdir(parents=True, exist_ok=True)

    await self._save_metadata(meta)

    # Write CLAUDE.md immediately so it's ready before the first message
    ensure_master_claude_md(meta, self._cfg)

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
      if not meta or meta.status != SessionStatus.ACTIVE:
        continue
      # Check session name first
      if query_lower in (meta.name or '').lower():
        meta.has_running_tasks = bool(meta.thinking_since) or await self._has_running_tasks(meta.id)
        results.append(meta)
        continue
      # Check chat events
      events_path = self._chat_events_path(meta.id)
      if events_path.exists():
        try:
          text = events_path.read_text(encoding='utf-8').lower()
          if query_lower in text:
            meta.has_running_tasks = bool(meta.thinking_since) or await self._has_running_tasks(meta.id)
            results.append(meta)
        except OSError as e:
          log.debug('search_read_failed', session_id=meta.id, error=str(e))
    for s in results:
      if s.updated_at.tzinfo is None:
        s.updated_at = s.updated_at.replace(tzinfo=timezone.utc)
    results.sort(key=lambda s: s.updated_at, reverse=True)
    return results

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

  # ---------------------------------------------------------------------------
  # Chat event persistence (NDJSON — for WebSocket catch-up)
  # ---------------------------------------------------------------------------

  async def save_chat_event(self, session_id: str, event: dict) -> None:
    """Append a single NDJSON event line to chat_events.jsonl."""
    await append_ndjson(self._chat_events_path(session_id), event)

  def load_chat_events_sync(self, session_id: str) -> list[dict]:
    """Read all chat events for catch-up (sync, for WebSocket handler)."""
    return parse_ndjson_file(self._chat_events_path(session_id))

  def _chat_events_path(self, session_id: str) -> Path:
    return self._session_dir(session_id) / "data" / "chat_events.jsonl"

  # ---------------------------------------------------------------------------
  # Usage / token tracking
  # ---------------------------------------------------------------------------

  def get_session_usage(self, session_id: str) -> dict | None:
    """Extract context-window token usage from the last result event.

    Scans chat_events.jsonl backwards for the most recent 'result' event and
    accumulates total_cost_usd across ALL result events.

    Returns a dict with:
      context_tokens  – input + cache_creation + cache_read from last result
      context_limit   – from modelUsage contextWindow (default 200000)
      total_cost_usd  – sum across every result event
      model           – primary model name
    Returns None if no result events exist.
    """
    events = parse_ndjson_file(self._chat_events_path(session_id))
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
      # Track last result with actual token data (Claude Code sometimes emits
      # result events with all-zero usage).
      u = ev.get("usage", {})
      if u.get("input_tokens", 0) + u.get("cache_creation_input_tokens", 0) + u.get("cache_read_input_tokens", 0) > 0:
        last_usage_result = ev

    if last_result is None:
      return None

    # Prefer the last result that has real token data; fall back to last_result.
    usage_source = last_usage_result or last_result
    usage = usage_source.get("usage", {})
    context_tokens = (
        usage.get("input_tokens", 0) + usage.get("cache_creation_input_tokens", 0) +
        usage.get("cache_read_input_tokens", 0))

    # Extract context limit and model from modelUsage
    model_usage = usage_source.get("modelUsage", {})
    context_limit = 200_000
    model = ""
    for model_name, info in model_usage.items():
      model = model_name
      context_limit = info.get("contextWindow", 200_000)
      break  # use the first (primary) model

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
