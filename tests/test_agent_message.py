"""Tests for the agent_message cross-session relay (A1 event, A2 route, A3 CLI).

The contract: an ``agent_message`` event carries a message from another agent
session into a session's event log and wakes its master — but it is NOT a real
user message, so the takeoff gate excludes it purely by event type. The
spawner gate code itself stays untouched (the exclusion is by type, like
``scheduled_trigger``).
"""

import pathlib
from unittest import mock

import conftest
import pytest

from src.infra import event_types as ET
from src.infra import models
from src.runtime.cli import session

# ---------------------------------------------------------------------------
# A2 route boundaries
# ---------------------------------------------------------------------------


class RouteSessionBlocks:
  """Session-blocks double for the session-message route tests; its store is itself."""

  def __init__(self, by_id: dict[str, models.SessionMetadata]) -> None:
    self.sessions = by_id
    self.store = self

  async def get_session(self, session_id: str) -> models.SessionMetadata | None:
    return self.sessions.get(session_id)


def _payload() -> dict[str, str]:
  return {"session_id": "caller", "target_session_id": "target", "content": "status please"}


@pytest.mark.asyncio
async def test_session_message_refuses_an_archived_task_node(tmp_path: pathlib.Path) -> None:
  """Agent relays do not restore archived task nodes."""
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  caller = await conftest.create_task(tree, parent=None, request_id="caller", name="Caller")
  target = await conftest.create_task(tree, parent=None, request_id="target", name="Target")
  await tree.archive_subtree(target.id, caller=conftest.OPERATOR)

  with conftest.make_internal_router_client(cfg, session_blocks, tree) as client:
    response = client.post(
        "/api/internal/session-message",
        json={
            "session_id": caller.id,
            "target_session_id": target.id,
            "content": "status please"
        },
    )

  assert response.status_code == 409
  assert response.json()["detail"] == f"task {target.id} is archived"
  assert tree.task_state(target.id) == "archived"


def test_session_message_relay_admits_and_dispatches_a_task_input() -> None:
  session_blocks = RouteSessionBlocks(
      {
          "caller": models.SessionMetadata(profile="manager", id="caller", name="Caller PM"),
          "target": models.SessionMetadata(profile="manager", id="target", name="Target Task"),
      })
  task_mgr = mock.MagicMock()
  task_mgr.dispatch.admit_input = mock.AsyncMock()
  task_mgr.dispatch.dispatch_pending = mock.AsyncMock()

  with conftest.make_internal_router_client(mock.MagicMock(), session_blocks, task_mgr) as client:
    resp = client.post("/api/internal/session-message", json=_payload())

  assert resp.status_code == 200
  assert resp.json() == {"status": "accepted"}
  task_mgr.dispatch.admit_input.assert_awaited_once_with(
      "target",
      event_type=ET.AGENT_MESSAGE,
      content="status please",
      actor="agent",
      from_session="caller",
      from_session_name="Caller PM",
  )
  task_mgr.dispatch.dispatch_pending.assert_awaited_once_with("target")


def test_session_message_request_rejects_extra_fields() -> None:
  session_blocks = RouteSessionBlocks({})
  with conftest.make_internal_router_client(mock.MagicMock(), session_blocks, mock.MagicMock()) as client:
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
