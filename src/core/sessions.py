"""Session management for CharlieBot."""

import json
from datetime import datetime
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
    return SessionMetadata.model_validate_json(raw)

  async def list_sessions(self, status: Optional[SessionStatus] = None) -> list[SessionMetadata]:
    """List sessions, newest first. Optionally filter by status."""
    sessions: list[SessionMetadata] = []
    if not self._cfg.sessions_dir.exists():
      return sessions
    for d in self._cfg.sessions_dir.iterdir():
      if not d.is_dir():
        continue
      meta = await self.get_session(d.name)
      if meta and (status is None or meta.status == status):
        meta.has_running_tasks = bool(meta.thinking_since) or await self._has_running_tasks(meta.id)
        sessions.append(meta)
    sessions.sort(key=lambda s: s.updated_at, reverse=True)
    return sessions

  async def rename_session(self, session_id: str, new_name: str) -> Optional[SessionMetadata]:
    """Rename a session and return the updated metadata."""
    meta = await self.get_session(session_id)
    if not meta:
      return None
    meta.name = new_name
    meta.updated_at = datetime.utcnow()
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
    await streaming_manager.broadcast("sidebar", {
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
    await streaming_manager.broadcast("sidebar", {
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
    meta.updated_at = datetime.utcnow()
    await self._save_metadata(meta)
    log.info("session_archived", session_id=session_id)
    return meta

  async def unarchive_session(self, session_id: str) -> Optional[SessionMetadata]:
    """Restore an archived session back to active."""
    meta = await self.get_session(session_id)
    if not meta:
      return None
    meta.status = SessionStatus.ACTIVE
    meta.updated_at = datetime.utcnow()
    await self._save_metadata(meta)
    log.info("session_unarchived", session_id=session_id)
    return meta

  async def save_metadata(self, meta: SessionMetadata) -> None:
    """Public wrapper for _save_metadata."""
    await self._save_metadata(meta)

  # ---------------------------------------------------------------------------
  # Chat event persistence (NDJSON — for WebSocket catch-up)
  # ---------------------------------------------------------------------------

  async def save_chat_event(self, session_id: str, event: dict) -> None:
    """Append a single NDJSON event line to chat_events.jsonl."""
    path = self._chat_events_path(session_id)
    path.parent.mkdir(parents=True, exist_ok=True)
    async with aiofiles.open(path, "a", encoding="utf-8") as f:
      await f.write(json.dumps(event) + "\n")

  def load_chat_events_sync(self, session_id: str) -> list[dict]:
    """Read all chat events for catch-up (sync, for WebSocket handler)."""
    path = self._chat_events_path(session_id)
    if not path.exists():
      return []
    events = []
    with open(path, "r", encoding="utf-8") as f:
      for line in f:
        line = line.strip()
        if line:
          try:
            events.append(json.loads(line))
          except json.JSONDecodeError:
            pass
    return events

  def _chat_events_path(self, session_id: str) -> Path:
    return self._session_dir(session_id) / "data" / "chat_events.jsonl"

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
