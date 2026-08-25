"""Tests for the agent_message cross-session relay (A1 event, A2 route, A3 CLI).

The contract: an ``agent_message`` event carries a message from another agent
session into a session's event log and wakes its master — but it is NOT a real
user message, so the takeoff gate excludes it purely by event type. The
spawner gate code itself stays untouched (the exclusion is by type, like
``scheduled_trigger``).
"""

from pathlib import Path
from typing import Any, Coroutine, Optional
from unittest.mock import MagicMock, patch

import pytest
from conftest import FakeSessionManager
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import internal
from src.api.deps import get_session_manager
from src.api.internal import router as internal_router
from src.api.message_utils import build_agent_message_event
from src.cli.session import main as session_cli_main
from src.core import event_types as ET
from src.core.message_aggregator import MessageAggregator
from src.core.models import SessionMetadata, SessionStatus
from src.core.takeoff_gate import DelegationBlockedError, check_takeoff_gate

ROOT = Path(__file__).resolve().parents[1]


def _agent_message_event(content: str, timestamp: Optional[str] = None) -> dict[str, Any]:
  event: dict[str, Any] = {
      "type": ET.AGENT_MESSAGE,
      "content": content,
      "from_session": "caller-id",
      "from_session_name": "Caller",
  }
  if timestamp is not None:
    event["timestamp"] = timestamp
  return event


def _user_event(content: str) -> dict[str, Any]:
  return {"type": ET.USER, "content": content}


# ---------------------------------------------------------------------------
# Gate: agent_message can neither mint nor revoke a takeoff window
# ---------------------------------------------------------------------------


def test_takeoff_gate_agent_message_does_not_mint_takeoff() -> None:
  session_mgr = FakeSessionManager([_agent_message_event("take off")])

  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_agent_message_does_not_revoke_takeoff() -> None:
  session_mgr = FakeSessionManager([
      _user_event("take off"),
      _agent_message_event("One more ordinary relayed message"),
  ])

  check_takeoff_gate("session-id", session_mgr)


# ---------------------------------------------------------------------------
# A1 event shape
# ---------------------------------------------------------------------------


def test_build_agent_message_event_shape() -> None:
  event = build_agent_message_event("hello", from_session="s1", from_session_name="PM")

  assert event["type"] == ET.AGENT_MESSAGE
  assert event["type"] == "agent_message"
  assert event["content"] == "hello"
  assert event["from_session"] == "s1"
  assert event["from_session_name"] == "PM"
  assert event["timestamp"]
  assert set(event) == {"type", "content", "from_session", "from_session_name", "timestamp"}


# ---------------------------------------------------------------------------
# A2 route boundaries
# ---------------------------------------------------------------------------


class RouteSessionManager:
  """Session-manager double for the session-message route tests."""

  def __init__(self, sessions: dict[str, SessionMetadata]) -> None:
    self.sessions = sessions
    self.persisted: list[tuple[str, dict[str, Any]]] = []

  async def get_session(self, session_id: str) -> Optional[SessionMetadata]:
    return self.sessions.get(session_id)

  async def persist_and_broadcast(self, session_id: str, event: dict[str, Any]) -> None:
    self.persisted.append((session_id, event))


def _build_route_client(session_mgr: RouteSessionManager) -> TestClient:
  app = FastAPI()
  app.include_router(internal_router, prefix="/api/internal")
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[internal.get_config] = lambda: MagicMock()
  return TestClient(app)


def _payload(session_id: str = "caller", target: str = "target") -> dict[str, str]:
  return {"session_id": session_id, "target_session_id": target, "content": "status please"}


def test_session_message_404_when_caller_missing() -> None:
  session_mgr = RouteSessionManager({})
  with _build_route_client(session_mgr) as client:
    resp = client.post("/api/internal/session-message", json=_payload())
  assert resp.status_code == 404
  assert resp.json()["detail"] == "Session not found"
  assert session_mgr.persisted == []


def test_session_message_404_when_target_missing() -> None:
  session_mgr = RouteSessionManager({"caller": SessionMetadata(id="caller", name="Caller")})
  with _build_route_client(session_mgr) as client:
    resp = client.post("/api/internal/session-message", json=_payload())
  assert resp.status_code == 404
  assert resp.json()["detail"] == "Target session not found"
  assert session_mgr.persisted == []


def test_session_message_409_when_target_archived() -> None:
  session_mgr = RouteSessionManager({
      "caller": SessionMetadata(id="caller", name="Caller"),
      "target": SessionMetadata(id="target", name="Target", status=SessionStatus.ARCHIVED),
  })
  with _build_route_client(session_mgr) as client:
    resp = client.post("/api/internal/session-message", json=_payload())
  assert resp.status_code == 409
  assert "archived" in resp.json()["detail"]
  assert session_mgr.persisted == []


def test_session_message_relay_persists_event_and_wakes_master(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  session_mgr = RouteSessionManager({
      "caller": SessionMetadata(id="caller", name="Caller PM"),
      "target": SessionMetadata(id="target", name="Target Task"),
  })
  triggered: list[tuple[str, str]] = []

  async def _noop() -> None:
    return None

  def fake_trigger_master(session_id: str, summary: str, *args: Any, **kwargs: Any) -> Coroutine[Any, Any, None]:
    triggered.append((session_id, summary))
    return _noop()

  created: list[str] = []

  def fake_create_logged_task(coro: Coroutine[Any, Any, Any], name: Optional[str] = None) -> None:
    created.append(name or "")
    coro.close()

  monkeypatch.setattr(internal, "trigger_master", fake_trigger_master)
  monkeypatch.setattr(internal, "create_logged_task", fake_create_logged_task)

  with _build_route_client(session_mgr) as client:
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
  with _build_route_client(session_mgr) as client:
    resp = client.post(
        "/api/internal/session-message",
        json={**_payload(), "surprise": "field"},
    )
  assert resp.status_code == 422


# ---------------------------------------------------------------------------
# A3 CLI
# ---------------------------------------------------------------------------


def _mock_cli_config(tmp_path: Path) -> MagicMock:
  cfg = MagicMock()
  cfg.server_port = 9443
  cfg.server_base_url = "http://localhost:9443"
  cfg.charliebot_access_key = ""
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  return cfg


def _ok_response(payload: dict[str, Any]) -> MagicMock:
  resp = MagicMock()
  resp.json.return_value = payload
  resp.raise_for_status = MagicMock()
  return resp


def test_cli_session_create_posts_metadata_only_payload(tmp_path: Path) -> None:
  cfg = _mock_cli_config(tmp_path)
  resp = _ok_response({"id": "new-id", "name": "task-a"})

  with patch("sys.argv", ["session", "create", "--name", "task-a", "--backend", "codex-o3", "--role", "project"]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp) as post_mock:
    session_cli_main()

  assert post_mock.call_count == 1
  url = post_mock.call_args[0][0]
  assert url.endswith("/api/sessions/")
  assert post_mock.call_args[1]["json"] == {"name": "task-a", "backend": "codex-o3", "role": "project"}


def test_cli_session_create_group_triggers_second_group_call(tmp_path: Path) -> None:
  cfg = _mock_cli_config(tmp_path)
  create_resp = _ok_response({"id": "new-id", "name": "task-a"})
  group_resp = _ok_response({"id": "new-id", "name": "task-a", "group": "bp-eval"})

  with patch("sys.argv", ["session", "create", "--name", "task-a", "--group", "bp-eval"]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", side_effect=[create_resp, group_resp]) as post_mock:
    session_cli_main()

  assert post_mock.call_count == 2
  first_url = post_mock.call_args_list[0][0][0]
  second = post_mock.call_args_list[1]
  assert first_url.endswith("/api/sessions/")
  assert second[0][0].endswith("/api/sessions/new-id/group")
  assert second[1]["json"] == {"group": "bp-eval"}


def test_cli_session_send_relays_message(tmp_path: Path) -> None:
  cfg = _mock_cli_config(tmp_path)
  resp = _ok_response({"status": "accepted"})

  with patch("sys.argv", ["session", "send", "target-id", "--message", "relay this", "--session", "caller-id"]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp) as post_mock:
    session_cli_main()

  assert post_mock.call_count == 1
  url = post_mock.call_args[0][0]
  assert url.endswith("/api/internal/session-message")
  assert post_mock.call_args[1]["json"] == {
      "session_id": "caller-id",
      "target_session_id": "target-id",
      "content": "relay this",
  }


def test_cli_session_send_reads_message_file(tmp_path: Path) -> None:
  cfg = _mock_cli_config(tmp_path)
  msg_file = tmp_path / "msg.txt"
  msg_file.write_text("file content relay")
  resp = _ok_response({"status": "accepted"})

  with patch("sys.argv", ["session", "send", "target-id", "--file", str(msg_file), "--session", "caller-id"]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp) as post_mock:
    session_cli_main()

  assert post_mock.call_args[1]["json"]["content"] == "file content relay"


def test_cli_session_send_rejects_message_and_file_together() -> None:
  with patch("sys.argv", ["session", "send", "t", "--message", "m", "--file", "f"]), \
       pytest.raises(SystemExit) as exc_info:
    session_cli_main()
  assert exc_info.value.code == 2


def test_cli_session_send_requires_a_message_source() -> None:
  with patch("sys.argv", ["session", "send", "t", "--session", "caller"]), \
       pytest.raises(SystemExit) as exc_info:
    session_cli_main()
  assert exc_info.value.code == 2


def test_cli_session_send_missing_file_is_usage_error(tmp_path: Path) -> None:
  cfg = _mock_cli_config(tmp_path)
  with patch("sys.argv", ["session", "send", "target-id", "--file", str(tmp_path / "nope.txt"), "--session", "caller"]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       pytest.raises(SystemExit) as exc_info:
    session_cli_main()
  assert exc_info.value.code == 2


# ---------------------------------------------------------------------------
# W1: aggregator + JS whitelist (agent_message surfaces as a visible message)
# ---------------------------------------------------------------------------


def test_agent_message_aggregates_to_message_delta() -> None:
  agg = MessageAggregator()
  deltas = list(agg.feed({
      "type": ET.AGENT_MESSAGE,
      "content": "status please",
      "from_session": "caller-id",
      "from_session_name": "Caller PM",
      "timestamp": "2026-08-11T08:00:00Z",
  }))

  assert deltas == [
      {
          "type": "message",
          "message": {
              "role": "agent_message",
              "content": "status please",
              "from_session": "caller-id",
              "from_session_name": "Caller PM",
              "timestamp": "2026-08-11T08:00:00Z",
              "event_index": 0,
              "id": "legacy:0",
          },
      }
  ]


def test_agent_message_is_whitelisted_in_chat_renderers() -> None:
  """The chat renderers must know the agent_message role, or the turns vanish from the UI."""
  rendering = (ROOT / "web" / "static" / "js" / "chat" / "rendering.js").read_text(encoding="utf-8")
  turn_engine = (ROOT / "web" / "static" / "js" / "chat" / "turn-engine.js").read_text(encoding="utf-8")
  for name, source in (("rendering.js", rendering), ("turn-engine.js", turn_engine)):
    assert "'agent_message'" in source, f"{name}: STIMULUS_ROLES lost the agent_message entry"
  assert "agent_message: 'Agent'" in rendering, "rendering.js: TURN_TYPE_LABELS lost the Agent label"
  assert 'msg.role === "agent_message"' in rendering, "rendering.js lost the agent_message render branch"
