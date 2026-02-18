"""Session management for CharlieBot."""

import json
from datetime import datetime
from pathlib import Path
from typing import Optional

import aiofiles
import structlog

from src.core.config import CharliBotConfig
from src.core.git import GitError, GitManager
from src.core.models import (
  ConversationHistory,
  CreateSessionRequest,
  SessionMetadata,
  SessionStatus,
)

log = structlog.get_logger()


class SessionManager:
  """CRUD operations for CharlieBot sessions."""

  def __init__(self, cfg: CharliBotConfig, git_manager: GitManager):
    self._cfg = cfg
    self._git = git_manager

  # ---------------------------------------------------------------------------
  # Session CRUD
  # ---------------------------------------------------------------------------

  async def create_session(self, req: CreateSessionRequest) -> SessionMetadata:
    """Create a new session with optional git worktree setup."""
    meta = SessionMetadata(
      name=req.name,
      repo_url=req.repo_url,
      repo_path=req.repo_path,
      base_branch=req.base_branch,
    )

    session_dir = self._session_dir(meta.id)
    # Create directory structure
    for subdir in ["worktree", "data", "threads"]:
      (session_dir / subdir).mkdir(parents=True, exist_ok=True)

    # Initialize empty task queue
    queue_path = session_dir / "task_queue.json"
    async with aiofiles.open(queue_path, "w") as f:
      await f.write(json.dumps({"session_id": meta.id, "tasks": [], "updated_at": datetime.utcnow().isoformat()}))

    # Initialize empty conversation history
    await self._save_history(ConversationHistory(session_id=meta.id))

    # Set up git if a repo is provided
    if req.repo_url or req.repo_path:
      try:
        await self._setup_git(meta)
      except GitError as e:
        log.warning("git_setup_failed", session=meta.id, error=str(e))
        # Don't fail session creation if git fails; session still usable without git

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

  async def _setup_git(self, meta: SessionMetadata) -> None:
    """Set up bare repo + session worktree."""
    bare_path = self._cfg.repos_dir / f"{meta.id}.git"

    if meta.repo_url:
      await self._git.clone_bare(meta.repo_url, bare_path)
    elif meta.repo_path:
      await self._git.link_local_repo(Path(meta.repo_path), bare_path)

    worktree_path = self._session_dir(meta.id) / "worktree"
    await self._git.add_worktree(bare_path, worktree_path, meta.base_branch)

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
