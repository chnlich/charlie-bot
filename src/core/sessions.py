"""Session management for CharlieBot."""

import json
from datetime import datetime
from pathlib import Path
from typing import Optional

import aiofiles
import structlog

from src.core.config import CharliBotConfig
from src.core.models import (
  ConversationHistory,
  CreateSessionRequest,
  SessionMetadata,
  SessionStatus,
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

  async def list_sessions(self) -> list[SessionMetadata]:
    """List all sessions, newest first."""
    sessions: list[SessionMetadata] = []
    if not self._cfg.sessions_dir.exists():
      return sessions
    for d in self._cfg.sessions_dir.iterdir():
      if not d.is_dir():
        continue
      meta = await self.get_session(d.name)
      if meta:
        sessions.append(meta)
    sessions.sort(key=lambda s: s.created_at, reverse=True)
    return sessions

  async def archive_session(self, session_id: str) -> None:
    """Mark a session as archived (does not delete files)."""
    meta = await self.get_session(session_id)
    if not meta:
      return
    meta.status = SessionStatus.ARCHIVED
    meta.updated_at = datetime.utcnow()
    await self._save_metadata(meta)

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
