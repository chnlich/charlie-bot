"""Description prefix + body-memo contract of the workers-panel list payload
(src/api/threads.py list_threads)."""

import asyncio
from pathlib import Path

from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import threads as threads_api
from src.api.deps import get_thread_manager, get_trigger_manager
from src.api.threads import _LIST_DESCRIPTION_CAP
from src.api.threads import router as threads_router
from src.core.config import CharlieBotConfig, get_config
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
  app.dependency_overrides[get_thread_manager] = lambda: ThreadManager(cfg)
  app.dependency_overrides[get_trigger_manager] = lambda: TriggerManager(cfg, sessions)
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
