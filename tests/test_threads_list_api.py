"""Description prefix + body-memo contract of the thread-row payloads
(src/api/threads.py list_threads, src/api/sessions.py get_session_view)."""

import asyncio
from pathlib import Path

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import threads as threads_api
from src.api.deps import (
    get_config,
    get_session_manager,
    get_thread_manager,
    get_trigger_manager,
)
from src.api.sessions import router as sessions_router
from src.api.threads import _LIST_DESCRIPTION_CAP
from src.api.threads import router as threads_router
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, ThreadStatus
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.triggers import TriggerManager

LONG_DESCRIPTION = "spec " * 300  # 1500 chars, over the list cap


def _seeded_client(tmp_path: Path):
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  sessions = SessionManager(cfg)

  async def seed() -> tuple[str, str]:
    session = await sessions.create_session(CreateSessionRequest(name="list-payload"))
    threads = ThreadManager(cfg)
    await threads.create_thread(session, "short description")
    long_thread = await threads.create_thread(session, LONG_DESCRIPTION)
    return session.id, long_thread.id

  session_id, long_thread_id = asyncio.run(seed())

  app = FastAPI()
  app.include_router(threads_router, prefix="/api/threads")
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_thread_manager] = lambda: ThreadManager(cfg)
  app.dependency_overrides[get_trigger_manager] = lambda: TriggerManager(cfg, sessions)
  app.dependency_overrides[get_session_manager] = lambda: sessions
  app.dependency_overrides[get_config] = lambda: cfg
  return TestClient(app), session_id, long_thread_id


def test_list_caps_long_descriptions_and_marks_truncation(tmp_path: Path) -> None:
  client, session_id, _ = _seeded_client(tmp_path)

  rows = {row["id"]: row for row in client.get(f"/api/threads/{session_id}/list").json()}
  descriptions = {row["description"] for row in rows.values()}

  assert "short description" in descriptions
  short_row = next(row for row in rows.values() if row["description"] == "short description")
  assert "description_full_len" not in short_row

  long_row = next(row for row in rows.values() if row is not short_row)
  assert long_row["description"] == LONG_DESCRIPTION[:_LIST_DESCRIPTION_CAP]
  assert long_row["description_full_len"] == len(LONG_DESCRIPTION)


def test_thread_detail_still_serves_the_full_description(tmp_path: Path) -> None:
  client, session_id, long_thread_id = _seeded_client(tmp_path)

  meta = client.get(f"/api/threads/{session_id}/threads/{long_thread_id}").json()

  assert meta["description"] == LONG_DESCRIPTION


def test_session_view_ships_the_same_truncated_rows(tmp_path: Path) -> None:
  client, session_id, long_thread_id = _seeded_client(tmp_path)

  list_rows = {row["id"]: row for row in client.get(f"/api/threads/{session_id}/list").json()}
  view_rows = {row["id"]: row for row in client.get(f"/api/sessions/{session_id}/view").json()["threads"]}

  assert set(view_rows) == set(list_rows)
  for tid, list_row in list_rows.items():
    view_row = view_rows[tid]
    assert view_row["description"] == list_row["description"]
    assert view_row.get("description_full_len") == list_row.get("description_full_len")
  assert view_rows[long_thread_id]["description"] == LONG_DESCRIPTION[:_LIST_DESCRIPTION_CAP]
  assert view_rows[long_thread_id]["description_full_len"] == len(LONG_DESCRIPTION)


def test_list_body_memo_invalidates_on_metadata_rewrite(tmp_path: Path) -> None:
  client, session_id, _ = _seeded_client(tmp_path)
  threads_api._list_body_memo.clear()
  url = f"/api/threads/{session_id}/list"

  first = client.get(url)
  second = client.get(url)
  assert second.content == first.content

  rows = {row["id"]: row for row in first.json()}
  any_id = next(iter(rows))
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  asyncio.run(ThreadManager(cfg).update_status(session_id, any_id, ThreadStatus.RUNNING))

  third = client.get(url)
  assert third.json()[next(i for i, row in enumerate(third.json()) if row["id"] == any_id)]["status"] == "running"
  assert third.content != first.content


def test_list_poll_repeating_the_rendered_etag_gets_a_bodyless_204(tmp_path: Path) -> None:
  client, session_id, _ = _seeded_client(tmp_path)
  threads_api._list_body_memo.clear()
  url = f"/api/threads/{session_id}/list"

  first = client.get(url)
  etag = first.headers["ETag"]
  assert first.headers["Cache-Control"] == "no-store"
  conditional = client.get(url, params={"etag": etag})
  assert conditional.status_code == 204
  assert conditional.content == b""
  assert conditional.headers["ETag"] == etag

  # A signature move publishes a new body and tag; the stale tag re-serves 200.
  rows = {row["id"]: row for row in first.json()}
  any_id = next(iter(rows))
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  asyncio.run(ThreadManager(cfg).update_status(session_id, any_id, ThreadStatus.RUNNING))
  stale = client.get(url, params={"etag": etag})
  assert stale.status_code == 200
  assert stale.content != first.content
  assert stale.headers["ETag"] != etag
  assert client.get(url, params={"etag": stale.headers["ETag"]}).status_code == 204


def test_list_poll_skips_the_signature_walk_until_a_mark_or_the_sweep(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  client, session_id, _ = _seeded_client(tmp_path)
  threads_api._list_body_memo.clear()
  threads_api._sig_gate.clear()
  url = f"/api/threads/{session_id}/list"

  walks = {"n": 0}
  real = threads_api._row_source_stats

  def counting(threads_dir: str, triggers_dir: str):
    walks["n"] += 1
    return real(threads_dir, triggers_dir)

  monkeypatch.setattr(threads_api, "_row_source_stats", counting)

  client.get(url)
  assert walks["n"] == 1
  for _ in range(9):
    assert client.get(url).status_code == 200
  assert walks["n"] == 1

  client.get(url)
  assert walks["n"] == 2

  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  rows = {row["id"]: row for row in client.get(url).json()}
  any_id = next(iter(rows))
  asyncio.run(ThreadManager(cfg).update_status(session_id, any_id, ThreadStatus.RUNNING))
  updated = client.get(url)
  assert next(row for row in updated.json() if row["id"] == any_id)["status"] == "running"
  assert walks["n"] == 3


def test_marked_rebuild_reuses_rows_and_parses_from_one_walk(tmp_path: Path) -> None:
  """A marked rebuild rebuilds only the moved file's row, and the rows parse from
  the walked pairs (the signature and the rows describe one file instant)."""
  client, session_id, _ = _seeded_client(tmp_path)
  threads_api._list_body_memo.clear()
  threads_api._sig_gate.clear()
  threads_api._thread_row_memo.clear()
  url = f"/api/threads/{session_id}/list"

  first = client.get(url)
  rows_first = {row["id"]: row for row in first.json()}
  any_id = next(iter(rows_first))

  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  thread_mgr = ThreadManager(cfg)
  asyncio.run(thread_mgr.update_status(session_id, any_id, ThreadStatus.RUNNING))

  second = client.get(url)
  rows_second = {row["id"]: row for row in second.json()}
  assert rows_second[any_id]["status"] == "running"
  assert second.content != first.content
  # Every row is byte-identical to the whole-parse rows (the memo's identity is
  # the parse memo's), and the moved row is the only one rebuilt.
  assert rows_second.keys() == rows_first.keys()
  for tid, row in rows_second.items():
    if tid == any_id:
      assert row != rows_first[tid]
    else:
      assert row == rows_first[tid]

  # The stored row dicts are the ones the next rebuild serves: after another
  # mark, the unmoved files' rows are the same objects in the memo, the moved
  # file's row is a fresh dict.
  rows_objects = threads_api._thread_row_memo.get(session_id)
  assert rows_objects is not None
  stored = {v[2]["id"]: v[2] for v in rows_objects.values()}
  asyncio.run(thread_mgr.update_status(session_id, any_id, ThreadStatus.COMPLETED))
  third = client.get(url)
  assert next(row for row in third.json() if row["id"] == any_id)["status"] == "completed"
  rows_objects_third = threads_api._thread_row_memo.get(session_id)
  assert rows_objects_third is not None
  stored_third = {v[2]["id"]: v[2] for v in rows_objects_third.values()}
  for tid, row in stored_third.items():
    if tid == any_id:
      assert row is not stored[tid]
    else:
      assert row is stored[tid]


def test_list_threads_from_stats_matches_list_threads(tmp_path: Path) -> None:
  """The shared parse-merge serves the same metas from pre-walked pairs as from its own scan."""
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  sessions = SessionManager(cfg)

  async def seed() -> str:
    session = await sessions.create_session(CreateSessionRequest(name="from-stats"))
    threads = ThreadManager(cfg)
    await threads.create_thread(session, "one")
    await threads.create_thread(session, "two")
    return session.id

  session_id = asyncio.run(seed())
  threads_dir = cfg.sessions_dir / session_id / "threads"
  mgr = ThreadManager(cfg)
  scanned = asyncio.run(mgr.list_threads(session_id))

  from src.core.threads import iter_thread_meta_stats
  pairs = list(iter_thread_meta_stats(str(threads_dir)))
  from_stats = mgr.list_threads_from_stats(pairs)

  assert {t.id for t in from_stats} == {t.id for t in scanned}
  assert {t.description for t in from_stats} == {"one", "two"}
