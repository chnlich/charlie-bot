"""Session management for CharlieBot."""

import json
from datetime import datetime
from pathlib import Path
from typing import Optional

import aiofiles
import structlog

from src.agents.master_cc import ensure_master_claude_md
from src.core.config import CharliBotConfig
from src.core.models import (
  ConversationHistory,
  CreateSessionRequest,
  SessionMetadata,
  SessionStatus,
  TaskQueue,
)

log = structlog.get_logger()


class SessionManager:
  """CRUD operations for CharlieBot sessions."""

  def __init__(self, cfg: CharliBotConfig):
    self._cfg = cfg

  # ---------------------------------------------------------------------------
  # Session CRUD
  # ---------------------------------------------------------------------------

  async def create_session(self, req: CreateSessionRequest) -> SessionMetadata:
    """Create a new session with optional repo_path."""
    name = req.name or await self._next_session_name()
    meta = SessionMetadata(
      name=name,
      repo_path=req.repo_path,
    )

    # Validate repo_path if provided
    if req.repo_path:
      repo = Path(req.repo_path)
      if not repo.is_dir() or not (repo / ".git").exists():
        log.warning("invalid_repo_path", path=req.repo_path)

    session_dir = self._session_dir(meta.id)
    # Create directory structure
    for subdir in ["data", "threads"]:
      (session_dir / subdir).mkdir(parents=True, exist_ok=True)

    # Initialize empty task queue
    queue_path = session_dir / "task_queue.json"
    async with aiofiles.open(queue_path, "w") as f:
      await f.write(json.dumps({"session_id": meta.id, "tasks": [], "updated_at": datetime.utcnow().isoformat()}))

    # Initialize empty conversation history
    await self._save_history(ConversationHistory(session_id=meta.id))

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
        meta.has_running_tasks = await self._has_running_tasks(meta.id)
        sessions.append(meta)
    sessions.sort(key=lambda s: s.created_at, reverse=True)
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
    return meta

  async def mark_unread(self, session_id: str) -> None:
    """Set the unread flag for a session (called when workers complete)."""
    meta = await self.get_session(session_id)
    if not meta or meta.has_unread:
      return
    meta.has_unread = True
    await self._save_metadata(meta)

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
  # Conversation history
  # ---------------------------------------------------------------------------

  async def load_history(self, session_id: str) -> ConversationHistory:
    """Load conversation history for a session."""
    path = self._history_path(session_id)
    if not path.exists():
      return ConversationHistory(session_id=session_id)
    async with aiofiles.open(path, "r") as f:
      raw = await f.read()
    return ConversationHistory.model_validate_json(raw)

  async def save_history(self, history: ConversationHistory) -> None:
    await self._save_history(history)

  # ---------------------------------------------------------------------------
  # Private helpers
  # ---------------------------------------------------------------------------

  async def _has_running_tasks(self, session_id: str) -> bool:
    """Check if a session has any tasks with status 'running'."""
    queue_path = self._session_dir(session_id) / "task_queue.json"
    if not queue_path.exists():
      return False
    try:
      async with aiofiles.open(queue_path, "r") as f:
        raw = await f.read()
      queue = TaskQueue.model_validate_json(raw)
      return any(t.status == "running" for t in queue.tasks)
    except Exception:
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

  async def _save_history(self, history: ConversationHistory) -> None:
    path = self._history_path(history.session_id)
    path.parent.mkdir(parents=True, exist_ok=True)
    async with aiofiles.open(path, "w") as f:
      await f.write(history.model_dump_json(indent=2))

  def _session_dir(self, session_id: str) -> Path:
    return self._cfg.sessions_dir / session_id

  def _metadata_path(self, session_id: str) -> Path:
    return self._session_dir(session_id) / "metadata.json"

  def _history_path(self, session_id: str) -> Path:
    return self._session_dir(session_id) / "data" / "conversation.json"
