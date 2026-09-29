"""GET /api/threads/{sid}/threads/{tid}/events incremental (after=) semantics."""

import json
from pathlib import Path

import pytest
from conftest import make_home_config, seed_thread
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.deps import get_thread_manager
from src.api.threads import router as threads_router
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

EVENTS = [
    {
        "type": "assistant",
        "message": {
            "content": [{
                "type": "text",
                "text": "hello"
            }]
        },
        "timestamp": "2026-09-02T10:00:00Z"
    },
    {
        "type": "ping",
        "timestamp": "2026-09-02T10:00:01Z"
    },
    {
        "type": "complete",
        "status": "completed",
        "message": "done",
        "timestamp": "2026-09-02T10:00:02Z"
    },
]


async def _client_with_log(tmp_path: Path) -> tuple[TestClient, str, Path]:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Events"))
  meta = await seed_thread(thread_mgr, session, "events")
  path = await thread_mgr.get_events_log_path(session.id, meta.id)
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text("".join(json.dumps(e) + "\n" for e in EVENTS), encoding="utf-8")
  app = FastAPI()
  app.include_router(threads_router, prefix="/api/threads")
  app.dependency_overrides[get_thread_manager] = lambda: thread_mgr
  url = f"/api/threads/{session.id}/threads/{meta.id}/events"
  return TestClient(app), url, path


@pytest.mark.asyncio
async def test_after_envelope_slice_reset_and_rejection(tmp_path: Path) -> None:
  client, url, _path = await _client_with_log(tmp_path)
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


@pytest.mark.asyncio
async def test_full_fetch_serves_stored_render_and_renders_after_append(tmp_path: Path) -> None:
  client, url, _path = await _client_with_log(tmp_path)
  first = client.get(url)
  second = client.get(url)
  # The unchanged-log re-open rides the stored render: byte-identical body.
  assert second.content == first.content
  assert second.json() == first.json()

  with _path.open("a", encoding="utf-8") as f:
    f.write(json.dumps({"type": "ping", "timestamp": "2026-09-02T10:00:03Z"}) + "\n")
  third = client.get(url)
  # The append dropped the stored render, so the re-render carries the new row.
  assert [e["type"] for e in third.json()] == ["assistant", "ping", "complete", "ping"]
  reopened = client.get(url)
  assert reopened.content == third.content
