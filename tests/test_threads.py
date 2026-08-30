"""ThreadManager.list_threads scan behavior."""

from pathlib import Path

import pytest
from conftest import make_home_config

from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager


@pytest.mark.asyncio
async def test_list_threads_empty_without_threads_dir(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Scan"))

  assert await thread_mgr.list_threads(session.id) == []


@pytest.mark.asyncio
async def test_list_threads_returns_newest_first_and_skips_dir_without_metadata(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Scan"))
  first = await thread_mgr.create_thread(session, "first")
  second = await thread_mgr.create_thread(session, "second")
  (thread_mgr.thread_dir(session.id, "incomplete") / "data").mkdir(parents=True)

  threads = await thread_mgr.list_threads(session.id)

  assert [t.id for t in threads] == [second.id, first.id]
  assert [t.description for t in threads] == ["second", "first"]
