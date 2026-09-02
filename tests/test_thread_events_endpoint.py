"""GET /api/threads/{sid}/threads/{tid}/events incremental (after=) semantics."""

import json
from pathlib import Path

import pytest
from conftest import make_home_config
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.deps import get_thread_manager
from src.api.threads import router as threads_router
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

EVENTS = [
  {"type": "assistant", "message": {"content": [{"type": "text", "text": "hello"}]}, "timestamp": "2026-09-02T10:00:00Z"},
  {"type": "ping", "timestamp": "2026-09-02T10:00:01Z"},
  {"type": "complete", "status": "completed", "message": "done", "timestamp": "2026-09-02T10:00:02Z"},
]


async def _client_with_log(tmp_path: Path) -> tuple[TestClient, str]:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Events"))
  meta = await thread_mgr.create_thread(session, "events")
  path = await thread_mgr.get_events_log_path(session.id, meta.id)
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text("".join(json.dumps(e) + "\n" for e in EVENTS), encoding="utf-8")
  app = FastAPI()
  app.include_router(threads_router, prefix="/api/threads")
  app.dependency_overrides[get_thread_manager] = lambda: thread_mgr
  return TestClient(app), f"/api/threads/{session.id}/threads/{meta.id}/events"


@pytest.mark.asyncio
async def test_after_envelope_slice_reset_and_rejection(tmp_path: Path) -> None:
  client, url = await _client_with_log(tmp_path)
  full = client.get(url).json()
  assert isinstance(full, list)  # no-after keeps the plain-list shape
  assert [e["type"] for e in full] == ["assistant", "ping", "complete"]
  env = client.get(url, params={"after": 0}).json()
  assert env["total"] == 3 and env["reset"] is False
  assert env["events"] == full  # envelope rows serialize like the plain-list rows
  tail = client.get(url, params={"after": 2}).json()
  assert tail["total"] == 3 and tail["reset"] is False
  assert full[:2] + tail["events"] == full
  reset = client.get(url, params={"after": 99}).json()
  assert reset["reset"] is True and reset["events"] == full
  assert client.get(url, params={"after": -1}).status_code == 422
