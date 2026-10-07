"""Tests for the agent_message cross-session relay (A1 event, A2 route, A3 CLI).

The contract: an ``agent_message`` event carries a message from another agent
session into a session's event log and wakes its master — but it is NOT a real
user message, so the takeoff gate excludes it purely by event type. The
spawner gate code itself stays untouched (the exclusion is by type, like
``scheduled_trigger``).
"""

import asyncio
import pathlib
from collections.abc import Coroutine
from typing import Any
from unittest import mock

import conftest
import pytest

from src.infra import event_types as ET
from src.infra import models
from src.runtime import sessions
from src.runtime.api import internal
from src.runtime.cli import session

# ---------------------------------------------------------------------------
# A2 route boundaries
# ---------------------------------------------------------------------------


class RouteSessionManager:
  """Session-manager double for the session-message route tests."""

  def __init__(self, by_id: dict[str, models.SessionMetadata]) -> None:
    self.sessions = by_id
    self.persisted: list[tuple[str, dict[str, Any]]] = []

  async def get_session(self, session_id: str) -> models.SessionMetadata | None:
    return self.sessions.get(session_id)

  async def persist_and_broadcast(self, session_id: str, event: dict[str, Any]) -> None:
    self.persisted.append((session_id, event))


def _payload() -> dict[str, str]:
  return {"session_id": "caller", "target_session_id": "target", "content": "status please"}


@pytest.mark.asyncio
async def test_session_message_to_archived_target_relays_and_pulls_back(tmp_path: pathlib.Path) -> None:
  """No 409: the relay returns success, persists the event, and the wake that
  follows (default pull_back) leaves the archived target ACTIVE."""
  cfg = conftest.make_home_config(tmp_path)
  session_mgr = sessions.SessionManager(cfg)
  caller = await session_mgr.create_session(models.CreateSessionRequest(name="Caller"))
  target = await session_mgr.create_session(models.CreateSessionRequest(name="Target"))
  await session_mgr.archive_session(target.id)

  spawned: list[asyncio.Task] = []

  with (
      mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()),
      mock.patch(conftest.MASTER_TRIGGER_RUN_MESSAGE_WITH_RESUME_RECOVERY_PATCH_TARGET, new=mock.AsyncMock()) as
      mock_run,
      mock.patch.object(internal, "create_logged_task", conftest.make_task_spawner(spawned)),
  ):
    resp = await internal.session_message(
        models.SessionMessageRequest(session_id=caller.id, target_session_id=target.id, content="status please"),
        session_mgr=session_mgr,
        cfg=cfg,
    )
    assert resp == {"status": "accepted"}
    await asyncio.wait_for(spawned[0], timeout=5)

  events = session_mgr.load_chat_events_sync(target.id)
  assert any(ev.get("type") == ET.AGENT_MESSAGE and ev.get("content") == "status please" for ev in events)
  mock_run.assert_awaited_once()
  assert mock_run.await_args.args[1].id == target.id
  fresh = await session_mgr.get_session(target.id)
  assert fresh is not None
  assert fresh.status == models.SessionStatus.ACTIVE


def test_session_message_relay_persists_event_and_wakes_master(monkeypatch: pytest.MonkeyPatch,) -> None:
  session_mgr = RouteSessionManager(
      {
          "caller": models.SessionMetadata(id="caller", name="Caller PM"),
          "target": models.SessionMetadata(id="target", name="Target Task"),
      })
  triggered: list[tuple[str, str]] = []

  def fake_trigger_master(session_id: str, summary: str, *args: Any, **kwargs: Any) -> Coroutine[Any, Any, None]:
    triggered.append((session_id, summary))
    return conftest._noop()

  created: list[str] = []

  monkeypatch.setattr(internal, "trigger_master", fake_trigger_master)
  monkeypatch.setattr(internal, "create_logged_task", conftest.record_create_logged_task(created))

  with conftest.make_internal_router_client(mock.MagicMock(), session_mgr) as client:
    resp = client.post("/api/internal/session-message", json=_payload())

  assert resp.status_code == 200
  assert resp.json() == {"status": "accepted"}
  assert len(session_mgr.persisted) == 1
  target_id, event = session_mgr.persisted[0]
  assert target_id == "target"
  assert event["type"] == ET.AGENT_MESSAGE
  assert event["content"] == "status please"
  assert event["from_session"] == "caller"
  assert event["from_session_name"] == "Caller PM"
  assert created == ["session-message-relay-target"]
  assert triggered == [("target", "[Message from session Caller PM] status please")]


def test_session_message_request_rejects_extra_fields() -> None:
  session_mgr = RouteSessionManager({})
  with conftest.make_internal_router_client(mock.MagicMock(), session_mgr) as client:
    resp = client.post(
        "/api/internal/session-message",
        json={
            **_payload(), "surprise": "field"
        },
    )
  assert resp.status_code == 422


# ---------------------------------------------------------------------------
# A3 CLI
# ---------------------------------------------------------------------------


def _mock_cli_config(tmp_path: pathlib.Path) -> mock.MagicMock:
  cfg = mock.MagicMock()
  cfg.server.port = 9443
  cfg.server_base_url = "http://localhost:9443"
  conftest.stub_credentials({"charliebot": {"access_key": ""}})
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  return cfg


def test_cli_session_send_relays_message(tmp_path: pathlib.Path) -> None:
  cfg = _mock_cli_config(tmp_path)
  resp = conftest.make_json_response({"status": "accepted"})

  with conftest.patched_cli_post(cfg,
                                 ["session", "send", "target-id", "--message", "relay this", "--session", "caller-id"],
                                 return_value=resp) as post_mock:
    session.main()

  assert post_mock.call_count == 1
  url = post_mock.call_args[0][0]
  assert url.endswith("/api/internal/session-message")
  assert post_mock.call_args[1]["json"] == {
      "session_id": "caller-id",
      "target_session_id": "target-id",
      "content": "relay this",
  }
