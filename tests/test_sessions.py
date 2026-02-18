"""Tests for src/core/sessions.py (SessionManager)."""

import pytest
from unittest.mock import AsyncMock, MagicMock

from src.core.config import load_config
from src.core.models import CreateSessionRequest, SessionStatus, ConversationHistory, ChatMessage, MessageRole
from src.core.sessions import SessionManager


@pytest.fixture()
def session_manager(tmp_home):
    cfg = load_config()
    cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
    cfg.repos_dir.mkdir(parents=True, exist_ok=True)
    git_manager = MagicMock()
    git_manager.clone_bare = AsyncMock()
    git_manager.link_local_repo = AsyncMock()
    git_manager.add_worktree = AsyncMock()
    return SessionManager(cfg, git_manager)


@pytest.mark.asyncio
class TestSessionManager:
    async def test_create_session_returns_metadata(self, session_manager):
        req = CreateSessionRequest(name="test-session")
        meta = await session_manager.create_session(req)
        assert meta.name == "test-session"
        assert meta.status == SessionStatus.ACTIVE
        assert meta.base_branch == "main"

    async def test_create_session_creates_directories(self, session_manager):
        cfg = load_config()
        req = CreateSessionRequest(name="dir-test")
        meta = await session_manager.create_session(req)
        session_dir = cfg.sessions_dir / meta.id
        assert (session_dir / "worktree").is_dir()
        assert (session_dir / "data").is_dir()
        assert (session_dir / "threads").is_dir()

    async def test_create_session_creates_task_queue(self, session_manager):
        cfg = load_config()
        req = CreateSessionRequest(name="queue-test")
        meta = await session_manager.create_session(req)
        queue_file = cfg.sessions_dir / meta.id / "task_queue.json"
        assert queue_file.exists()

    async def test_create_session_creates_conversation_history(self, session_manager):
        cfg = load_config()
        req = CreateSessionRequest(name="history-test")
        meta = await session_manager.create_session(req)
        history_file = cfg.sessions_dir / meta.id / "data" / "conversation.json"
        assert history_file.exists()

    async def test_get_session_returns_saved_metadata(self, session_manager):
        req = CreateSessionRequest(name="get-test")
        created = await session_manager.create_session(req)
        loaded = await session_manager.get_session(created.id)
        assert loaded is not None
        assert loaded.id == created.id
        assert loaded.name == "get-test"

    async def test_get_session_returns_none_for_unknown(self, session_manager):
        result = await session_manager.get_session("no-such-id")
        assert result is None

    async def test_list_sessions_empty(self, session_manager):
        sessions = await session_manager.list_sessions()
        assert sessions == []

    async def test_list_sessions_returns_all(self, session_manager):
        await session_manager.create_session(CreateSessionRequest(name="alpha"))
        await session_manager.create_session(CreateSessionRequest(name="beta"))
        sessions = await session_manager.list_sessions()
        assert len(sessions) == 2
        names = {s.name for s in sessions}
        assert "alpha" in names
        assert "beta" in names

    async def test_list_sessions_ordered_newest_first(self, session_manager):
        import asyncio
        s1 = await session_manager.create_session(CreateSessionRequest(name="first"))
        await asyncio.sleep(0.01)
        s2 = await session_manager.create_session(CreateSessionRequest(name="second"))
        sessions = await session_manager.list_sessions()
        # Newest (s2) should be first
        assert sessions[0].id == s2.id
        assert sessions[1].id == s1.id

    async def test_archive_session(self, session_manager):
        req = CreateSessionRequest(name="to-archive")
        meta = await session_manager.create_session(req)
        await session_manager.archive_session(meta.id)
        loaded = await session_manager.get_session(meta.id)
        assert loaded.status == SessionStatus.ARCHIVED

    async def test_archive_nonexistent_session_is_noop(self, session_manager):
        # Should not raise
        await session_manager.archive_session("ghost-id")

    async def test_load_history_empty(self, session_manager):
        req = CreateSessionRequest(name="history-load")
        meta = await session_manager.create_session(req)
        history = await session_manager.load_history(meta.id)
        assert history.session_id == meta.id
        assert history.messages == []

    async def test_save_and_load_history(self, session_manager):
        req = CreateSessionRequest(name="history-save")
        meta = await session_manager.create_session(req)
        history = ConversationHistory(session_id=meta.id)
        history.messages.append(ChatMessage(role=MessageRole.USER, content="hello"))
        await session_manager.save_history(history)

        loaded = await session_manager.load_history(meta.id)
        assert len(loaded.messages) == 1
        assert loaded.messages[0].content == "hello"

    async def test_load_history_missing_returns_empty(self, session_manager):
        """If history file doesn't exist, return an empty ConversationHistory."""
        history = await session_manager.load_history("phantom-session")
        assert history.session_id == "phantom-session"
        assert history.messages == []

    async def test_create_session_with_repo_url_calls_git(self, session_manager):
        req = CreateSessionRequest(name="git-session", repo_url="https://example.com/repo.git")
        await session_manager.create_session(req)
        session_manager._git.clone_bare.assert_awaited_once()

    async def test_create_session_git_failure_does_not_abort(self, session_manager):
        from src.core.git import GitError
        session_manager._git.clone_bare = AsyncMock(side_effect=GitError("clone failed"))
        req = CreateSessionRequest(name="git-fail", repo_url="https://example.com/fail.git")
        # Should not raise; session still created
        meta = await session_manager.create_session(req)
        assert meta.name == "git-fail"
