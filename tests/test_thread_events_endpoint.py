"""GET /api/threads/{sid}/threads/{tid}/events incremental (after=) semantics."""

import json
import pathlib
from unittest.mock import AsyncMock

import conftest
import fastapi
import pytest
from fastapi import testclient

from src.infra.models import CreateSessionRequest, RunRecord
from src.runtime.api import threads as threads_api

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


async def _client_with_log(tmp_path: pathlib.Path,
                           monkeypatch: pytest.MonkeyPatch) -> tuple[testclient.TestClient, str, pathlib.Path]:
  cfg, session_mgr, tree = conftest.build_env(tmp_path)
  session = await session_mgr.create_session(CreateSessionRequest(name="Events"))
  run_id = "run-events"
  await tree.runs.register_run(RunRecord(id=run_id, session_id=session.id, kind="work"))
  path = tree.runs.run_dir(session.id, run_id) / "events.jsonl"
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text("".join(json.dumps(e) + "\n" for e in EVENTS), encoding="utf-8")
  app = fastapi.FastAPI()
  app.include_router(threads_api.router, prefix="/api/threads")
  monkeypatch.setattr(threads_api, "get_task_manager", AsyncMock(return_value=tree))
  monkeypatch.setattr(threads_api, "get_run_store", AsyncMock(return_value=tree.runs))
  url = f"/api/threads/{session.id}/threads/{run_id}/events"
  return testclient.TestClient(app), url, path


@pytest.mark.asyncio
async def test_after_envelope_slice_reset_and_rejection(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  client, url, _path = await _client_with_log(tmp_path, monkeypatch)
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
async def test_full_fetch_serves_stored_render_and_renders_after_append(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  client, url, _path = await _client_with_log(tmp_path, monkeypatch)
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
