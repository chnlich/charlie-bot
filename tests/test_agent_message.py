"""Tests for the agent_message cross-session relay (A1 event, A2 route, A3 CLI).

The contract: an ``agent_message`` event carries a message from another agent
session into a session's event log and wakes its master — but it is NOT a real
user message, so the takeoff gate excludes it purely by event type. The
spawner gate code itself stays untouched (the exclusion is by type, like
``scheduled_trigger``).
"""

import asyncio
from collections.abc import Coroutine
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from conftest import (
    BROADCAST_PATCH_TARGET,
    CLI_COMMON_GET_CONFIG_PATCH_TARGET,
    CLI_COMMON_REQUESTS_POST_PATCH_TARGET,
    FakeSessionManager,
    _noop,
    make_home_config,
    make_internal_router_client,
    make_json_response,
    make_task_spawner,
    user_event,
)

from src.api import internal
from src.api.message_utils import build_agent_message_event
from src.cli.session import main as session_cli_main
from src.core import event_types as ET
from src.core.message_aggregator import MessageAggregator
from src.core.models import (
    CreateSessionRequest,
    SessionMessageRequest,
    SessionMetadata,
    SessionStatus,
)
from src.core.sessions import SessionManager
from src.core.takeoff_gate import DelegationBlockedError, check_takeoff_gate

ROOT = Path(__file__).resolve().parents[1]


def _agent_message_event(content: str, timestamp: str | None = None) -> dict[str, Any]:
  event: dict[str, Any] = {
      "type": ET.AGENT_MESSAGE,
      "content": content,
      "from_session": "caller-id",
      "from_session_name": "Caller",
  }
  if timestamp is not None:
    event["timestamp"] = timestamp
  return event


# ---------------------------------------------------------------------------
# Gate: agent_message can neither mint nor revoke a takeoff window
# ---------------------------------------------------------------------------


def test_takeoff_gate_agent_message_does_not_mint_takeoff() -> None:
  session_mgr = FakeSessionManager([_agent_message_event("take off")])

  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_agent_message_does_not_revoke_takeoff() -> None:
  session_mgr = FakeSessionManager(
      [
          user_event("take off"),
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

  async def get_session(self, session_id: str) -> SessionMetadata | None:
    return self.sessions.get(session_id)

  async def persist_and_broadcast(self, session_id: str, event: dict[str, Any]) -> None:
    self.persisted.append((session_id, event))


def _payload(session_id: str = "caller", target: str = "target") -> dict[str, str]:
  return {"session_id": session_id, "target_session_id": target, "content": "status please"}


def test_session_message_404_when_caller_missing() -> None:
  session_mgr = RouteSessionManager({})
  with make_internal_router_client(MagicMock(), session_mgr) as client:
    resp = client.post("/api/internal/session-message", json=_payload())
  assert resp.status_code == 404
  assert resp.json()["detail"] == "Session not found"
  assert not session_mgr.persisted


def test_session_message_404_when_target_missing() -> None:
  session_mgr = RouteSessionManager({"caller": SessionMetadata(id="caller", name="Caller")})
  with make_internal_router_client(MagicMock(), session_mgr) as client:
    resp = client.post("/api/internal/session-message", json=_payload())
  assert resp.status_code == 404
  assert resp.json()["detail"] == "Target session not found"
  assert not session_mgr.persisted


@pytest.mark.asyncio
async def test_session_message_to_archived_target_relays_and_pulls_back(tmp_path: Path) -> None:
  """No 409: the relay returns success, persists the event, and the wake that
  follows (default pull_back) leaves the archived target ACTIVE."""
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  caller = await session_mgr.create_session(CreateSessionRequest(name="Caller"))
  target = await session_mgr.create_session(CreateSessionRequest(name="Target"))
  await session_mgr.archive_session(target.id)

  spawned: list[asyncio.Task] = []

  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch("src.core.master_trigger.run_message_with_resume_recovery", new=AsyncMock()) as mock_run,
      patch.object(internal, "create_logged_task", make_task_spawner(spawned)),
  ):
    resp = await internal.session_message(
        SessionMessageRequest(session_id=caller.id, target_session_id=target.id, content="status please"),
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
  assert fresh.status == SessionStatus.ACTIVE


def test_session_message_relay_persists_event_and_wakes_master(monkeypatch: pytest.MonkeyPatch,) -> None:
  session_mgr = RouteSessionManager(
      {
          "caller": SessionMetadata(id="caller", name="Caller PM"),
          "target": SessionMetadata(id="target", name="Target Task"),
      })
  triggered: list[tuple[str, str]] = []

  def fake_trigger_master(session_id: str, summary: str, *args: Any, **kwargs: Any) -> Coroutine[Any, Any, None]:
    triggered.append((session_id, summary))
    return _noop()

  created: list[str] = []

  def fake_create_logged_task(coro: Coroutine[Any, Any, Any], name: str | None = None) -> None:
    created.append(name or "")
    coro.close()

  monkeypatch.setattr(internal, "trigger_master", fake_trigger_master)
  monkeypatch.setattr(internal, "create_logged_task", fake_create_logged_task)

  with make_internal_router_client(MagicMock(), session_mgr) as client:
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
  with make_internal_router_client(MagicMock(), session_mgr) as client:
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


def _mock_cli_config(tmp_path: Path) -> MagicMock:
  cfg = MagicMock()
  cfg.server_port = 9443
  cfg.server_base_url = "http://localhost:9443"
  cfg.charliebot_access_key = ""
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  return cfg


def test_cli_session_create_posts_metadata_only_payload(tmp_path: Path) -> None:
  cfg = _mock_cli_config(tmp_path)
  resp = make_json_response({"id": "new-id", "name": "task-a"})

  with patch("sys.argv", ["session", "create", "--name", "task-a", "--backend", "codex-o3", "--role", "project"]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET, return_value=resp) as post_mock:
    session_cli_main()

  assert post_mock.call_count == 1
  url = post_mock.call_args[0][0]
  assert url.endswith("/api/sessions/")
  assert post_mock.call_args[1]["json"] == {"name": "task-a", "backend": "codex-o3", "role": "project"}


def test_cli_session_create_group_triggers_second_group_call(tmp_path: Path) -> None:
  cfg = _mock_cli_config(tmp_path)
  create_resp = make_json_response({"id": "new-id", "name": "task-a"})
  group_resp = make_json_response({"id": "new-id", "name": "task-a", "group": "bp-eval"})

  with patch("sys.argv", ["session", "create", "--name", "task-a", "--group", "bp-eval"]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET, side_effect=[create_resp, group_resp]) as post_mock:
    session_cli_main()

  assert post_mock.call_count == 2
  first_url = post_mock.call_args_list[0][0][0]
  second = post_mock.call_args_list[1]
  assert first_url.endswith("/api/sessions/")
  assert second[0][0].endswith("/api/sessions/new-id/group")
  assert second[1]["json"] == {"group": "bp-eval"}


def test_cli_session_send_relays_message(tmp_path: Path) -> None:
  cfg = _mock_cli_config(tmp_path)
  resp = make_json_response({"status": "accepted"})

  with patch("sys.argv", ["session", "send", "target-id", "--message", "relay this", "--session", "caller-id"]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET, return_value=resp) as post_mock:
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
  resp = make_json_response({"status": "accepted"})

  with patch("sys.argv", ["session", "send", "target-id", "--file", str(msg_file), "--session", "caller-id"]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET, return_value=resp) as post_mock:
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
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       pytest.raises(SystemExit) as exc_info:
    session_cli_main()
  assert exc_info.value.code == 2


# ---------------------------------------------------------------------------
# W1: aggregator + JS whitelist (agent_message surfaces as a visible message)
# ---------------------------------------------------------------------------


def test_agent_message_aggregates_to_message_delta() -> None:
  agg = MessageAggregator()
  deltas = list(
      agg.feed(
          {
              "type": ET.AGENT_MESSAGE,
              "content": "status please",
              "from_session": "caller-id",
              "from_session_name": "Caller PM",
              "timestamp": "2026-08-11T08:00:00Z",
          }))

  assert deltas == [
      {
          "type": "message",
          "message":
              {
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
  """The chat renderers must know the agent_message role, or the turns vanish from the UI.

  STIMULUS_ROLES lives once in chat/shared.js; rendering.js's DOM matcher and
  turn-engine.js's fold derive both read that shared list.
  """
  shared = (ROOT / "web" / "static" / "js" / "chat" / "shared.js").read_text(encoding="utf-8")
  rendering = (ROOT / "web" / "static" / "js" / "chat" / "rendering.js").read_text(encoding="utf-8")
  turn_engine = (ROOT / "web" / "static" / "js" / "chat" / "turn-engine.js").read_text(encoding="utf-8")
  assert "'agent_message'" in shared, "shared.js: STIMULUS_ROLES lost the agent_message entry"
  for name, source in (("rendering.js", rendering), ("turn-engine.js", turn_engine)):
    assert "STIMULUS_ROLES" in source, f"{name}: no longer reads the shared STIMULUS_ROLES list"
    assert "const STIMULUS_ROLES" not in source, f"{name}: re-split STIMULUS_ROLES into a local copy"
  assert "agent_message: 'Agent'" in rendering, "rendering.js: TURN_TYPE_LABELS lost the Agent label"
  assert 'msg.role === "agent_message"' in rendering, "rendering.js lost the agent_message render branch"
