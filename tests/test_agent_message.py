"""Tests for the agent_message cross-session relay (A1 event, A2 route, A3 CLI).

The contract: an ``agent_message`` event carries a message from another agent
session into a session's event log and wakes its master — but it is NOT a real
user message, so the takeoff gate excludes it purely by event type. The
spawner gate code itself stays untouched (the exclusion is by type, like
``scheduled_trigger``).
"""

import asyncio
import hashlib
import json
import pathlib
from unittest import mock

import conftest
import httpx
import pytest

from src.infra import constants, models, text_length
from src.infra import event_types as ET
from src.runtime import control_events, run_token
from src.runtime.api import deps
from src.runtime.cli import common
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
  await tree.completion.archive_subtree(
      target.id,
      request_id="archive-test",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR)

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
      request_id=None,
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


def test_derived_agent_send_request_id_uses_run_verb_target_and_text() -> None:
  digest = hashlib.sha256(b"run-1\x00session send\x00target\x00status please").hexdigest()[:24]
  assert control_events.derived_agent_send_request_id("run-1", "session send", "target",
                                                      "status please") == (f"agent-send-{digest}")


@pytest.mark.asyncio
async def test_session_message_replays_for_a_run_and_newly_admits_for_an_operator(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  caller = await conftest.create_task(tree, parent=None, request_id="caller", name="Caller")
  target = await conftest.create_task(tree, parent=None, request_id="target", name="Target")
  route_client = conftest.make_internal_router_client(cfg, session_blocks, tree)
  claims = run_token.RunTokenClaims(session_id=caller.id, run_id="run-1", agent="caller")
  route_client.app.dependency_overrides[deps.require_caller] = lambda: run_token.CallerIdentity(
      kind="agent", claims=claims)

  async with httpx.AsyncClient(transport=httpx.ASGITransport(app=route_client.app), base_url="http://test") as client:
    first = await client.post("/api/internal/session-message", json=_relay_payload(caller.id, target.id, "same send"))
    second = await client.post("/api/internal/session-message", json=_relay_payload(caller.id, target.id, "same send"))

    with conftest.prime_barrier(session_blocks.events, target.id):
      concurrent = await asyncio.gather(
          *[
              client.post("/api/internal/session-message", json=_relay_payload(caller.id, target.id, "same send"))
              for _ in range(2)
          ])

    assert [response.status_code for response in (first, second, *concurrent)] == [200, 200, 200, 200]
    assert [response.json() for response in (first, second, *concurrent)] == [{"status": "accepted"}] * 4

  agent_messages = [ev for ev in tree.fact_history(target.id) if ev.get("type") == ET.AGENT_MESSAGE]
  expected_id = control_events.derived_agent_send_request_id("run-1", "session send", target.id, "same send")
  assert len(agent_messages) == 1
  assert agent_messages[0]["request_id"] == expected_id
  assert tree.runs.list_run_records_sync(target.id) == []

  route_client.app.dependency_overrides[deps.require_caller] = lambda: run_token.CallerIdentity(kind="operator")
  async with httpx.AsyncClient(transport=httpx.ASGITransport(app=route_client.app), base_url="http://test") as client:
    for _ in range(2):
      response = await client.post(
          "/api/internal/session-message", json=_relay_payload(caller.id, target.id, "operator send"))
      assert response.status_code == 200

  operator_messages = [
      ev for ev in tree.fact_history(target.id)
      if ev.get("type") == ET.AGENT_MESSAGE and ev.get("content") == "operator send"
  ]
  assert len(operator_messages) == 2
  assert all("request_id" not in event for event in operator_messages)


# ---------------------------------------------------------------------------
# A3 CLI
# ---------------------------------------------------------------------------


def test_cli_session_send_relays_message(tmp_path: pathlib.Path) -> None:
  cfg = conftest.make_cli_config(tmp_path)
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


# ---------------------------------------------------------------------------
# Weighted length (the limit's counting rule)
# ---------------------------------------------------------------------------


def test_weighted_length_counts_cjk_punctuation_and_fullwidth_double() -> None:
  """ASCII counts 1; CJK ideographs, CJK punctuation, and fullwidth forms count 2."""
  assert text_length.weighted_length("plain ascii") == 11
  assert text_length.weighted_length("一" * 1000) == 2000
  assert text_length.weighted_length("。") == 2
  assert text_length.weighted_length("Ａ") == 2
  assert text_length.weighted_length("") == 0


# ---------------------------------------------------------------------------
# The weighted-character limit on the relay route
# ---------------------------------------------------------------------------


def _relay_payload(caller_id: str, target_id: str, content: str) -> dict[str, str]:
  return {"session_id": caller_id, "target_session_id": target_id, "content": content}


@pytest.mark.asyncio
async def test_session_message_accepts_a_message_at_the_weighted_limit(tmp_path: pathlib.Path) -> None:
  """1000 CJK ideographs weigh exactly 2000: accepted, one agent_message event."""
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  caller = await conftest.create_task(tree, parent=None, request_id="caller", name="Caller")
  target = await conftest.create_task(tree, parent=None, request_id="target", name="Target")

  with conftest.make_internal_router_client(cfg, session_blocks, tree) as client:
    response = client.post("/api/internal/session-message", json=_relay_payload(caller.id, target.id, "一" * 1000))

  assert response.status_code == 200
  assert response.json() == {"status": "accepted"}
  events = [e for e in tree.events.load_events(target.id) if e["type"] == ET.AGENT_MESSAGE]
  assert len(events) == 1
  assert events[0]["content"] == "一" * 1000


@pytest.mark.asyncio
async def test_session_message_refuses_a_message_over_the_weighted_limit(tmp_path: pathlib.Path) -> None:
  """1001 CJK ideographs weigh 2002: 422 with the measured length and the limit,
  and the target session gains no event and no queued input."""
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  caller = await conftest.create_task(tree, parent=None, request_id="caller", name="Caller")
  target = await conftest.create_task(tree, parent=None, request_id="target", name="Target")

  with conftest.make_internal_router_client(cfg, session_blocks, tree) as client:
    response = client.post("/api/internal/session-message", json=_relay_payload(caller.id, target.id, "一" * 1001))

  assert response.status_code == 422
  assert response.json()["detail"] == (
      f"Message is 2002 weighted characters; the limit is {constants.MAX_AGENT_MESSAGE_WEIGHTED_CHARS}.\n"
      "CJK characters, CJK punctuation, and fullwidth characters count 2.\n"
      "Write the long content to a file. Send a short message with the conclusion,\n"
      "the request, and the file path.")
  assert not [e for e in tree.events.load_events(target.id) if e["type"] == ET.AGENT_MESSAGE]
  assert tree.dispatch.pending_inputs(target.id) == []


@pytest.mark.asyncio
async def test_session_message_refuses_an_over_limit_ascii_message(tmp_path: pathlib.Path) -> None:
  """2001 ASCII characters weigh 2001: refused, nothing written to the target."""
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  caller = await conftest.create_task(tree, parent=None, request_id="caller", name="Caller")
  target = await conftest.create_task(tree, parent=None, request_id="target", name="Target")

  with conftest.make_internal_router_client(cfg, session_blocks, tree) as client:
    response = client.post("/api/internal/session-message", json=_relay_payload(caller.id, target.id, "a" * 2001))

  assert response.status_code == 422
  assert "2001" in response.json()["detail"]
  assert not [e for e in tree.events.load_events(target.id) if e["type"] == ET.AGENT_MESSAGE]


# ---------------------------------------------------------------------------
# A3 CLI: the refusal exits 2 through the rejection printer
# ---------------------------------------------------------------------------


def test_cli_session_send_maps_a_422_refusal_to_exit_2(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """The over-limit refusal is a rejection, not a server fault: exit 2, effect none."""
  monkeypatch.setattr(common, "_maybe_version_skew_hint", lambda _base_url: None)
  detail = (
      f"Message is 2002 weighted characters; the limit is {constants.MAX_AGENT_MESSAGE_WEIGHTED_CHARS}.\n"
      "CJK characters, CJK punctuation, and fullwidth characters count 2.\n"
      "Write the long content to a file. Send a short message with the conclusion,\n"
      "the request, and the file path.")
  cfg = conftest.make_cli_config(tmp_path)
  resp = conftest.make_json_response({"detail": detail}, status_code=422)

  with pytest.raises(SystemExit) as excinfo, conftest.patched_cli_post(
      cfg, ["session", "send", "target-id", "--message", "a" * 2001, "--session", "caller-id"], return_value=resp):
    session.main()

  assert excinfo.value.code == session.EXIT_SEND_REFUSED
  payload = json.loads(capsys.readouterr().err)
  assert payload["error"] == detail
  assert payload["code"] == "server_error"
  assert payload["effect"] == "none"
