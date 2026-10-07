"""threads.ThreadManager.list_threads scan behavior."""

import pathlib
import shutil
import threading

import conftest
import pytest

from src.infra import models
from src.runtime import sessions, threads


@pytest.mark.asyncio
async def test_list_threads_returns_newest_first_and_skips_dir_without_metadata(tmp_path: pathlib.Path) -> None:
  cfg = conftest.make_home_config(tmp_path)
  session_mgr = sessions.SessionManager(cfg)
  thread_mgr = threads.ThreadManager(cfg)
  session = await session_mgr.create_session(models.CreateSessionRequest(name="Scan"))
  first = await conftest.seed_thread(thread_mgr, session, "first")
  second = await conftest.seed_thread(thread_mgr, session, "second")
  (thread_mgr.thread_dir(session.id, "incomplete") / "data").mkdir(parents=True)

  listed = await thread_mgr.list_threads(session.id)

  assert [t.id for t in listed] == [second.id, first.id]
  assert [t.description for t in listed] == ["second", "first"]


@pytest.mark.asyncio
async def test_list_threads_memo_reuses_unchanged_files_and_refreshes_on_save(tmp_path: pathlib.Path) -> None:
  cfg = conftest.make_home_config(tmp_path)
  session_mgr = sessions.SessionManager(cfg)
  thread_mgr = threads.ThreadManager(cfg)
  session = await session_mgr.create_session(models.CreateSessionRequest(name="Memo"))
  meta = await conftest.seed_thread(thread_mgr, session, "target")

  first = await thread_mgr.list_threads(session.id)
  second = await thread_mgr.list_threads(session.id)
  assert second[0] is first[0]

  meta.description = "renamed"
  await thread_mgr.save_metadata(meta)
  third = await thread_mgr.list_threads(session.id)
  assert third[0] is not first[0]
  assert third[0].description == "renamed"


@pytest.mark.asyncio
async def test_list_threads_memo_drops_deleted_threads(tmp_path: pathlib.Path) -> None:
  cfg = conftest.make_home_config(tmp_path)
  session_mgr = sessions.SessionManager(cfg)
  thread_mgr = threads.ThreadManager(cfg)
  session = await session_mgr.create_session(models.CreateSessionRequest(name="Memo"))
  keep = await conftest.seed_thread(thread_mgr, session, "keep")
  drop = await conftest.seed_thread(thread_mgr, session, "drop")
  await thread_mgr.list_threads(session.id)

  shutil.rmtree(thread_mgr.thread_dir(session.id, drop.id))

  listed = await thread_mgr.list_threads(session.id)
  assert [t.id for t in listed] == [keep.id]


@pytest.mark.asyncio
async def test_list_threads_memo_keeps_other_sessions_across_a_walk(tmp_path: pathlib.Path) -> None:
  """One session's walk drops only its own vanished files, never another session's.

  The memo keys the global metadata paths, so a whole-memo drop "not in this
  walk" evicts every other session's parsed files and forces each interleaved
  walk to re-read and re-parse its full thread set — the per-rebuild cost the
  sidebar's marked-session churn pays once per marked session per poll.
  """
  cfg = conftest.make_home_config(tmp_path)
  session_mgr = sessions.SessionManager(cfg)
  thread_mgr = threads.ThreadManager(cfg)
  session_a = await session_mgr.create_session(models.CreateSessionRequest(name="A"))
  session_b = await session_mgr.create_session(models.CreateSessionRequest(name="B"))
  await conftest.seed_thread(thread_mgr, session_a, "a-thread")
  await conftest.seed_thread(thread_mgr, session_b, "b-thread")

  await thread_mgr.list_threads(session_a.id)
  first_b = await thread_mgr.list_threads(session_b.id)
  await thread_mgr.list_threads(session_a.id)

  assert (await thread_mgr.list_threads(session_b.id))[0] is first_b[0]


@pytest.mark.asyncio
async def test_save_metadata_publishes_whole_file_under_concurrent_reads(tmp_path: pathlib.Path) -> None:
  """A reader never observes a half-written metadata.json while a save is in flight.

  list_threads reads the file from an executor thread with no coordination against
  _save_metadata's rewrite; only an atomic swap keeps a validation failure (the
  list endpoint's 500) from surfacing whenever a poll races a status update.
  """
  cfg = conftest.make_home_config(tmp_path)
  session_mgr = sessions.SessionManager(cfg)
  thread_mgr = threads.ThreadManager(cfg)
  session = await session_mgr.create_session(models.CreateSessionRequest(name="Atomic"))
  meta = await conftest.seed_thread(thread_mgr, session, "target")
  path = thread_mgr.thread_dir(session.id, meta.id) / "metadata.json"

  torn = 0
  done = False

  def reader() -> None:
    nonlocal torn
    while not done:
      try:
        models.ThreadMetadata.model_validate_json(path.read_text(encoding="utf-8"))
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
