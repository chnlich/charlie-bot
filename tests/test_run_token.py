"""Run-token identity: signing, fail-closed middleware rules, and CLI token priority."""

from __future__ import annotations

import json

import conftest
import fastapi
import pytest
from fastapi import testclient

from src.features.discord import discord_listener
from src.features.slack import slack_listener
from src.infra import event_types as ET
from src.runtime import run_token, takeoff_gate
from src.runtime.api import auth, deps


def _scope(headers: dict[str, str] | None = None, cookies: dict[str, str] | None = None) -> dict:
  raw_headers: list[tuple[bytes, bytes]] = []
  for name, value in (headers or {}).items():
    raw_headers.append((name.lower().encode(), value.encode()))
  if cookies:
    raw_headers.append((b"cookie", "; ".join(f"{k}={v}" for k, v in cookies.items()).encode()))
  return {"type": "http", "method": "GET", "path": "/api/sessions", "headers": raw_headers, "query_string": b""}


def _status(sent: list[dict]) -> int:
  return next(m for m in sent if m["type"] == "http.response.start")["status"]


CLAIMS = run_token.RunTokenClaims(session_id="s-1", run_id="r-1", agent="worker-alpha")

# ---------------------------------------------------------------------------
# Token mechanics
# ---------------------------------------------------------------------------


def test_sign_and_verify_round_trip() -> None:
  token = run_token.sign_run_token(CLAIMS, "op-secret")
  assert run_token.verify_run_token(token, "op-secret") == CLAIMS
  # Every claim is bound: another session/run/agent cannot reuse it.
  with pytest.raises(run_token.RunTokenError):
    run_token.verify_run_token(token, "other-key")
  payload_b64, sig = token.split(".", 1)
  import base64
  payload = json.loads(base64.urlsafe_b64decode(payload_b64 + "=" * (-len(payload_b64) % 4)))
  payload["run_id"] = "r-2"
  forged_payload = base64.urlsafe_b64encode(json.dumps(payload).encode()).rstrip(b"=").decode()
  with pytest.raises(run_token.RunTokenError):
    run_token.verify_run_token(f"{forged_payload}.{sig}", "op-secret")


def test_missing_signing_key_is_an_explicit_error() -> None:
  with pytest.raises(run_token.RunTokenError, match="without a configured"):
    run_token.sign_run_token(CLAIMS, "")
  with pytest.raises(run_token.RunTokenError, match="no signing key"):
    run_token.verify_run_token("a.b", "")


def test_a_run_bearer_with_no_configured_key_is_refused_with_401() -> None:
  """A presented bearer with no access key configured fails closed at the caller dependency."""
  conftest.stub_credentials({"charliebot": {"access_key": ""}})
  app = fastapi.FastAPI()
  app.dependency_overrides[deps.get_run_store] = lambda: None

  @app.get("/structural")
  async def structural(caller: run_token.CallerIdentity = fastapi.Depends(deps.require_caller)) -> dict:
    return {"kind": caller.kind}

  response = testclient.TestClient(app).get("/structural", headers={"Authorization": "Bearer not-the-key"})
  assert response.status_code == 401


# ---------------------------------------------------------------------------
# Middleware: a presented bearer is verified or rejected, never cookie-fallback
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_invalid_run_bearer_never_falls_back_to_operator_cookie() -> None:
  conftest.stub_credentials({"charliebot": {"access_key": "op-secret"}})
  mw = auth.AuthMiddleware(app=conftest._ok_asgi_downstream)
  sent = await conftest.run_through_asgi_middleware(
      mw, _scope(headers={"Authorization": "Bearer forged"}, cookies={"charliebot_access_key": "op-secret"}))
  assert _status(sent) == 401
  assert not conftest.asgi_downstream_called()


# ---------------------------------------------------------------------------
# The v2 task gate: an open manager chain that a real user message or a platform summon started
# ---------------------------------------------------------------------------

_PARENTS = {"root": None, "child": "root", "grandchild": "child"}
_MANAGERS = {"root": "manager", "child": "manager", "grandchild": "manager"}


def _gate(start: str, events_by_session: dict[str, list[dict]], states: dict[str, str] | None = None) -> str:
  """Run the gate from *start* over in-memory chat logs on the root -> child -> grandchild manager chain.

  A node missing from *states* is open.
  """
  return takeoff_gate.check_takeoff_gate_for_task(
      start,
      load_events=lambda session_id: events_by_session.get(session_id, []),
      task_meta_of=lambda session_id: (_PARENTS.get(session_id), _MANAGERS.get(session_id)),
      task_state_of=lambda session_id: (states or {}).get(session_id, "open"))


def _user(content: str) -> dict:
  return {"id": content, "type": ET.USER, "timestamp": "2026-10-15T00:00:00+00:00", "actor": "user", "content": content}


def _event(event_type: str, content: str = "take off") -> dict:
  return {"type": event_type, "content": content}


def test_task_gate_admits_a_user_question_anywhere_on_the_chain() -> None:
  """A question with no take off starts the line: the root and a child that holds its own question
  delegate, and a grandchild with only agent chatter rides the root's request."""
  events = {
      "root": [_user("Can you look at the flaky test?")],
      "child": [_user("Which file holds it?"), _event(ET.AGENT_MESSAGE, "progress")],
  }
  assert _gate("root", events) == "root"
  assert _gate("child", events) == "child"
  assert _gate("grandchild", {"root": events["root"], "child": [_event(ET.AGENT_MESSAGE)]}) == "root"


@pytest.mark.parametrize("state", ["completed", "cancelled", "archived"])
def test_task_gate_refuses_under_a_closed_task(state: str) -> None:
  """Every node up to the root must be open, even past a node that holds the user's message."""
  events = {"root": [_user("Ship it?")], "grandchild": [_user("go ahead")]}
  with pytest.raises(takeoff_gate.DelegationBlockedError, match=f"ancestor task root is {state}"):
    _gate("grandchild", events, states={"root": state})
  with pytest.raises(takeoff_gate.DelegationBlockedError, match=f"task grandchild is {state}"):
    _gate("grandchild", events, states={"grandchild": state})


def test_task_gate_refuses_a_chain_with_machine_input_only() -> None:
  """Agent messages, scheduled triggers, child reports and tool-result echoes never count, whatever their text."""
  machine = [
      _event(ET.AGENT_MESSAGE),
      _event(ET.SCHEDULED_TRIGGER),
      _event(ET.CHILD_REPORT),
      {
          "type": ET.USER,
          "content": [{
              "type": "tool_result",
              "content": "take off"
          }]
      },
  ]
  with pytest.raises(takeoff_gate.DelegationBlockedError, match="no user request on the task chain"):
    _gate("grandchild", {"root": machine, "child": machine, "grandchild": machine})


def test_task_gate_counts_user_messages_after_the_newest_clone_start_only() -> None:
  """A fork's copied user messages belong to its source; the node's own later message counts, and a later
  marker voids it again. The log grows in place, so the calls after the first fold only the appended suffix."""
  child_log = [_user("the source's request"), _event(ET.CLONE_START, "")]
  events = {"root": [_event(ET.AGENT_MESSAGE)], "child": child_log}
  with pytest.raises(takeoff_gate.DelegationBlockedError, match="no user request"):
    _gate("child", events)
  child_log.append(_user("the child's own request"))
  assert _gate("child", events) == "child"
  child_log.append(_event(ET.CLONE_START, ""))
  with pytest.raises(takeoff_gate.DelegationBlockedError, match="no user request"):
    _gate("child", events)


_SLACK_SUMMON = {
    "type": ET.AGENT_MESSAGE,
    "content": "Slack 线程召唤",
    "slack": {
        "channel_id": "C123",
        "thread_ts": "1700000000.000100",
        "mention_ts": "1700000000.000200",
    },
}
_LEGACY_SLACK_SUMMON = {
    "type": ET.AGENT_MESSAGE,
    "content": "Slack 线程召唤：帮我看下这个 flaky 的测试",
    "slack": {
        "channel_id": "C123",
        "thread_ts": "1700000000.000100",
        "mention_ts": "1700000000.000200",
    },
}
_DISCORD_SUMMON = {
    "type": ET.AGENT_MESSAGE,
    "content": "Discord thread summon",
    "discord": {
        "guild_id": "G123",
        "channel_id": "C123",
        "thread_id": "T123",
        "mention_id": "M123",
    },
}


@pytest.mark.parametrize(
    "summon", [_SLACK_SUMMON, _LEGACY_SLACK_SUMMON, _DISCORD_SUMMON], ids=["slack-current", "slack-legacy", "discord"])
def test_task_gate_admits_a_thread_summon_anywhere_on_the_chain(summon: dict) -> None:
  """A recorded Slack or Discord summon is a user request, so a grandchild delegates on the root's summon."""
  assert _gate("grandchild", {"root": [summon]}) == "root"


@pytest.mark.parametrize(
    ("node", "summon"), [("child", _LEGACY_SLACK_SUMMON), ("grandchild", _DISCORD_SUMMON)], ids=["child", "grandchild"])
def test_task_gate_admits_a_summon_on_a_descendant_node(node: str, summon: dict) -> None:
  """A grandchild can delegate from a summon recorded on its own node or its child ancestor."""
  assert _gate("grandchild", {node: [summon]}) == node


@pytest.mark.parametrize(
    "event", [
        pytest.param(
            {
                "type": ET.AGENT_MESSAGE,
                "content": "Slack 线程召唤 nudge",
                "slack": {
                    "channel_id": "C123",
                    "thread_ts": "1700000000.000100",
                    "nudge_of": "1700000000.000200",
                },
            },
            id="slack-nudge"),
        pytest.param(_event(ET.AGENT_MESSAGE), id="plain-agent-message"),
        pytest.param(
            {
                "type": ET.AGENT_MESSAGE,
                "content": "web reply nudge",
                "web_reply": {
                    "nudge_of": "1700000000.000200"
                },
            },
            id="web-reply-nudge"),
        pytest.param(_event(ET.SCHEDULED_TRIGGER), id="scheduled-trigger"),
        pytest.param(_event(ET.CHILD_REPORT), id="child-report"),
    ])
def test_task_gate_refuses_a_chain_whose_only_input_is_not_a_user_request(event: dict) -> None:
  """A thread nudge, a plain agent message, a web-reply nudge, a scheduled trigger and a child report
  carry no request: each leaves the chain without a user request."""
  with pytest.raises(takeoff_gate.DelegationBlockedError, match="no user request"):
    _gate("grandchild", {"root": [event]})


def test_task_gate_counts_a_summon_after_the_newest_clone_start_only() -> None:
  """A summon before the node's newest marker belongs to the source; a summon appended after it counts."""
  child_log = [_SLACK_SUMMON, _event(ET.CLONE_START, "")]
  events = {"root": [_event(ET.AGENT_MESSAGE)], "child": child_log}
  with pytest.raises(takeoff_gate.DelegationBlockedError, match="no user request"):
    _gate("child", events)
  child_log.append(_SLACK_SUMMON)
  assert _gate("child", events) == "child"


def test_thread_platform_names_are_the_shared_platform_keys() -> None:
  """Each listener writes its summon block under the key the gate's predicate reads, so the two names
  and the shared tuple cannot drift apart."""
  assert (slack_listener.SLACK.name, discord_listener.DISCORD.name) == ET.THREAD_PLATFORM_KEYS
