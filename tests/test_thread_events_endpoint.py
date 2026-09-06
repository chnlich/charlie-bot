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


def test_worker_events_memo_hit_contract(tmp_path: Path) -> None:
  """The hit path serves exactly the unchanged-log steady state: cold miss,
  warm hit equal to the threaded reader, grown/missing log back to a miss."""
  from src.api.threads import (
      read_thread_worker_events,
      read_thread_worker_events_memo_hit,
  )

  log = tmp_path / "events.jsonl"
  log.write_text("".join(json.dumps(e) + "\n" for e in EVENTS), encoding="utf-8")

  assert read_thread_worker_events_memo_hit(log) is None
  read_thread_worker_events(log)  # warm, as the first panel open does

  hit = read_thread_worker_events_memo_hit(log)
  assert hit is not None
  assert [e.model_dump(mode="json") for e in hit] == [e.model_dump(mode="json") for e in read_thread_worker_events(log)]

  with log.open("a", encoding="utf-8") as f:
    f.write(json.dumps(EVENTS[0]) + "\n")
  assert read_thread_worker_events_memo_hit(log) is None
  assert len(read_thread_worker_events(log)) == len(EVENTS) + 1
  assert read_thread_worker_events_memo_hit(log) is not None

  log.unlink()
  assert read_thread_worker_events_memo_hit(log) is None
  assert read_thread_worker_events(log) == []


def test_worker_events_memo_hit_never_waits_on_a_contended_lock(tmp_path: Path) -> None:
  """A concurrent reader holding the lock mid-read sends the poll to the
  threaded path instead of blocking the caller's thread on the file read."""

  from src.api.threads import (
      _thread_events_lock,
      read_thread_worker_events,
      read_thread_worker_events_memo_hit,
  )

  log = tmp_path / "events.jsonl"
  log.write_text("".join(json.dumps(e) + "\n" for e in EVENTS), encoding="utf-8")
  read_thread_worker_events(log)  # warm

  with _thread_events_lock:
    assert read_thread_worker_events_memo_hit(log) is None
