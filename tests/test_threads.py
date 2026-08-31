"""ThreadManager.list_threads scan behavior."""

import shutil
import threading
from pathlib import Path

import pytest
from conftest import make_home_config

from src.core.models import CreateSessionRequest, ThreadMetadata
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


@pytest.mark.asyncio
async def test_list_threads_memo_reuses_unchanged_files_and_refreshes_on_save(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Memo"))
  meta = await thread_mgr.create_thread(session, "target")

  first = await thread_mgr.list_threads(session.id)
  second = await thread_mgr.list_threads(session.id)
  assert second[0] is first[0]

  meta.description = "renamed"
  await thread_mgr.save_metadata(meta)
  third = await thread_mgr.list_threads(session.id)
  assert third[0] is not first[0]
  assert third[0].description == "renamed"


@pytest.mark.asyncio
async def test_list_threads_memo_drops_deleted_threads(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Memo"))
  keep = await thread_mgr.create_thread(session, "keep")
  drop = await thread_mgr.create_thread(session, "drop")
  await thread_mgr.list_threads(session.id)

  shutil.rmtree(thread_mgr.thread_dir(session.id, drop.id))

  threads = await thread_mgr.list_threads(session.id)
  assert [t.id for t in threads] == [keep.id]


@pytest.mark.asyncio
async def test_save_metadata_publishes_whole_file_under_concurrent_reads(tmp_path: Path) -> None:
  """A reader never observes a half-written metadata.json while a save is in flight.

  list_threads reads the file from an executor thread with no coordination against
  _save_metadata's rewrite; only an atomic swap keeps a validation failure (the
  list endpoint's 500) from surfacing whenever a poll races a status update.
  """
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Atomic"))
  meta = await thread_mgr.create_thread(session, "target")
  path = thread_mgr.thread_dir(session.id, meta.id) / "metadata.json"

  torn = 0
  done = False

  def reader() -> None:
    nonlocal torn
    while not done:
      try:
        ThreadMetadata.model_validate_json(path.read_text(encoding="utf-8"))
      except ValueError:
        torn += 1

  readers = [threading.Thread(target=reader) for _ in range(4)]
  for t in readers:
    t.start()
  for _ in range(200):
    await thread_mgr.save_metadata(meta)
  done = True
  for t in readers:
    t.join()

  assert torn == 0
