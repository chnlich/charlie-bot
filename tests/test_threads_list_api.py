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
  real = threads_api._list_body_signature

  def counting(threads_dir: str, triggers_dir: str):
    walks["n"] += 1
    return real(threads_dir, triggers_dir)

  monkeypatch.setattr(threads_api, "_list_body_signature", counting)

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
