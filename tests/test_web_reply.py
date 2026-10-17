"""Acceptance tests for the web reply channel (src.features.web_reply): the reply record and its refusals, the answers
binding, the round-end audit, the For-you layer, the rules segment, the live broadcast of a reply, and the session
status: its set and read, their refusals, the bootstrap object, the rendering and the ancestor notice."""

from __future__ import annotations

import asyncio
import contextlib
import importlib
import io
import json
import pathlib
import sys
import types
from collections.abc import Iterator
from unittest import mock

import conftest
import httpx
import pytest
from fastapi.testclient import TestClient

from src.features.chat_threads import thread_sessions
from src.features.discord import metadata as discord_metadata
from src.features.slack import metadata as slack_metadata
from src.features.web_reply import audit, reply
from src.features.web_reply import turn_contribution as web_turn_contribution
from src.infra import event_types as ET
from src.infra import models
from src.runtime import master_cc_state, streaming, task_prompts
from src.runtime.api import deps, message_utils
from src.runtime.cli import common

_SLACK_ORIGIN = slack_metadata.SlackOrigin(team_id="T1", channel_id="C1", thread_ts="1700000000.000100")
_DISCORD_ORIGIN = discord_metadata.DiscordOrigin(guild_id="G1", parent_channel_id="C1", thread_id="1234567890")


def _rig(tmp_path: pathlib.Path) -> conftest.SessionBlocks:
  return conftest.build_session_blocks(conftest.build_master_cc_cfg(tmp_path))


async def _session(blocks: conftest.SessionBlocks, *, slack_origin: slack_metadata.SlackOrigin | None = None) -> str:
  """A root manager session: a web session, or a Slack thread session when *slack_origin* is given."""
  origin = {} if slack_origin is None else {"slack_origin": slack_origin}
  meta = await conftest.create_root_session(blocks, models.CreateSessionRequest(name="session", **origin))
  return meta.id


async def _append(blocks: conftest.SessionBlocks, sid: str, event: dict) -> dict:
  """Append one event to the session log (ids and timestamps are stamped in place) and return it."""
  await blocks.events.save_chat_event(sid, event)
  return event


def _events(blocks: conftest.SessionBlocks, sid: str) -> list[dict]:
  return blocks.events.load_chat_events_sync(sid)


def _of_type(events: list[dict], event_type: str) -> list[dict]:
  return [ev for ev in events if ev.get("type") == event_type]


def _nudges(events: list[dict]) -> list[dict]:
  return [ev for ev in _of_type(events, ET.AGENT_MESSAGE) if reply.nudge_of(ev) is not None]


def _done(input_event_ids: list[str], exit_code: int = 0) -> dict:
  return {"type": ET.MASTER_DONE, "exit_code": exit_code, ET.STILL_THINKING: False, ET.INPUT_EVENT_IDS: input_event_ids}


@contextlib.contextmanager
def _running_turn(blocks: conftest.SessionBlocks, sid: str, input_event_ids: list[str]) -> Iterator[None]:
  """Expose a work item as the session's running turn, answering *input_event_ids*."""
  item = conftest.make_work_item(
      blocks.cfg, models.SessionMetadata(id=sid, name="session", profile="manager"), None, callbacks=blocks.callbacks())
  item.user_event_ids = list(input_event_ids)
  master_cc_state._current_items[sid] = item
  try:
    yield
  finally:
    master_cc_state._current_items.pop(sid, None)


@contextlib.contextmanager
def _wake_seams(tasks: list[asyncio.Task]) -> Iterator[mock.AsyncMock]:
  """Replace the wake with a mock and collect the audit's spawned tasks; yields the wake mock."""
  trigger = mock.AsyncMock()
  with (
      mock.patch(conftest.THREAD_ENTRY_TRIGGER_MASTER_PATCH_TARGET, trigger),
      mock.patch(conftest.THREAD_ENTRY_CREATE_LOGGED_TASK_PATCH_TARGET, side_effect=conftest.make_task_spawner(tasks)),
  ):
    yield trigger


async def _post_nudge(blocks: conftest.SessionBlocks, sid: str, user_event_id: str) -> dict:
  """Audit a turn that answered *user_event_id* without a reply; returns the nudge it persisted."""
  tasks: list[asyncio.Task] = []
  with _wake_seams(tasks):
    await audit.audit_turn(sid, _done([user_event_id]), blocks.store, blocks.events, blocks.lifecycle, blocks.successor)
    await asyncio.gather(*tasks)
  return _nudges(_events(blocks, sid))[-1]


async def _after_turn(monkeypatch: pytest.MonkeyPatch, blocks: conftest.SessionBlocks, sid: str, done: dict) -> None:
  """The web contribution's after_turn for one finished turn, over the process singletons bound to *blocks*."""
  conftest.bind_session_blocks(monkeypatch, blocks)
  meta = await blocks.store.get_session(sid)
  await web_turn_contribution.CONTRIBUTION.after_turn(meta, done, cfg=blocks.cfg)


# ---------------------------------------------------------------------------
# The reply record: readback, refusals, budget
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_a_web_reply_records_the_text_and_reads_back_the_outcome(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = conftest.make_internal_router_client(blocks.cfg, blocks)

  resp = client.post("/api/internal/web/reply", json={"session_id": sid, "text": "the answer"})

  assert resp.status_code == 200
  assert resp.json() == {"posted": True, "chars": 10, "over_budget": False, "answers": None}
  replies = _of_type(_events(blocks, sid), ET.WEB_REPLY)
  assert [(ev["content"], ev[ET.WEB_REPLY]) for ev in replies] == [("the answer", {"answers": None, "chars": 10})]
  assert (await blocks.store.get_session(sid)).has_unread


@pytest.mark.asyncio
@pytest.mark.parametrize("case, status", [("unknown", 404), ("thread", 409), ("worker", 409), ("blank", 422)])
async def test_a_refused_web_reply_records_nothing_and_marks_nothing(
    tmp_path: pathlib.Path, case: str, status: int) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks, slack_origin=_SLACK_ORIGIN if case == "thread" else None)
  if case == "worker":
    await blocks.store.update_field(sid, "profile", "worker", "test_worker_profile")
  client = conftest.make_internal_router_client(blocks.cfg, blocks)
  events_before = len(_events(blocks, sid))

  resp = client.post(
      "/api/internal/web/reply",
      json={
          "session_id": "no-such-session" if case == "unknown" else sid,
          "text": " \n\t" if case == "blank" else "the answer",
      })

  assert resp.status_code == status
  assert len(_events(blocks, sid)) == events_before
  assert not (await blocks.store.get_session(sid)).has_unread


@pytest.mark.asyncio
@pytest.mark.parametrize("chars, over_budget", [(500, False), (501, True)])
async def test_the_budget_flags_an_over_budget_reply_and_the_reply_still_posts(
    tmp_path: pathlib.Path, chars: int, over_budget: bool) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  text = "x" * chars

  readback = await reply.post_web_reply(sid, text, blocks.store, blocks.events, blocks.lifecycle)

  assert readback == {"posted": True, "chars": chars, "over_budget": over_budget, "answers": None}
  assert [ev["content"] for ev in _of_type(_events(blocks, sid), ET.WEB_REPLY)] == [text]


# ---------------------------------------------------------------------------
# The CLI: readback and refusal
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("stdin", [False, True])
def test_the_cli_posts_the_reply_text_and_prints_the_readback_as_one_json_line(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], stdin: bool) -> None:
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  readback = {"posted": True, "chars": 10, "over_budget": False, "answers": "u1"}
  argv = ["web", "reply", "--file", "-"]
  if stdin:
    monkeypatch.setattr("sys.stdin", io.StringIO("the answer"))
  else:
    reply_file = tmp_path / "reply.md"
    reply_file.write_text("the answer", encoding="utf-8")
    argv = ["web", "reply", "--file", str(reply_file)]

  with conftest.patched_cli_post(cfg, argv, return_value=conftest.make_json_response(readback)) as post_mock:
    importlib.import_module("src.features.web_reply.cli").main()

  assert post_mock.call_args.args[0].endswith("/api/internal/web/reply")
  assert post_mock.call_args.kwargs["json"] == {"session_id": "abc", "text": "the answer"}
  out = capsys.readouterr().out
  assert out.count("\n") == 1
  assert json.loads(out) == readback


def test_the_cli_exits_non_zero_with_a_json_error_on_a_refusal(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  reply_file = tmp_path / "reply.md"
  reply_file.write_text("the answer", encoding="utf-8")
  refused = conftest.make_json_response({"detail": "Session is not a web session"}, 409)

  with (
      conftest.patched_cli_post(cfg, ["web", "reply", "--file", str(reply_file)], return_value=refused),
      mock.patch(conftest.CLI_COMMON_MAYBE_VERSION_SKEW_HINT_PATCH_TARGET, return_value=None),
      pytest.raises(SystemExit) as exc_info,
  ):
    importlib.import_module("src.features.web_reply.cli").main()

  assert exc_info.value.code == 1
  assert json.loads(capsys.readouterr().err) == {
      "error": "Session is not a web session",
      "code": "server_error",
      "effect": "none",
  }


def test_the_cli_dialog_prints_the_for_you_layer_in_log_order(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  calls: list[tuple[str, dict]] = []

  def fake_get(endpoint: str, params: dict | None = None) -> dict:
    calls.append((endpoint, params or {}))
    return {"session_id": "abc", "entries": [{"role": "user", "content": "hi"}, {"role": "web_reply", "content": "ok"}]}

  monkeypatch.setattr(common, "get_api", fake_get)
  monkeypatch.setattr(sys, "argv", ["web", "dialog"])

  importlib.import_module("src.features.web_reply.cli").main()

  assert calls == [("/api/internal/web/dialog", {"session_id": "abc"})]
  out = capsys.readouterr().out
  assert out.count("\n") == 3 and out.index("hi") < out.index("ok")


@pytest.mark.parametrize(
    "profile, origin", [
        ("manager", {}),
        ("manager", {
            "slack_origin": _SLACK_ORIGIN
        }),
        ("manager", {
            "discord_origin": _DISCORD_ORIGIN
        }),
        ("worker", {}),
    ])
def test_the_web_session_predicate_agrees_with_the_thread_session_test(profile: str, origin: dict) -> None:
  """The package may not import chat_threads, so its predicate restates the thread test; this pins the two together."""
  meta = models.SessionMetadata(id="s", name="s", profile=profile, **origin)

  assert reply.is_web_session(meta) is (profile == "manager" and not thread_sessions.is_thread_session(meta))


def test_the_for_you_layer_keeps_user_messages_replies_and_notices_in_log_order() -> None:
  events = [
      {
          "id": "u1",
          "type": ET.USER,
          "content": "first"
      },
      {
          "id": "t1",
          "type": ET.SCHEDULED_TRIGGER,
          "content": "wake"
      },
      {
          "id": "r1",
          "type": ET.WEB_REPLY,
          "content": "answer",
          ET.WEB_REPLY: {
              "answers": "u1",
              "chars": 6
          }
      },
      {
          "id": "n1",
          "type": ET.AGENT_MESSAGE,
          "content": reply.NUDGE_TEXT,
          ET.WEB_REPLY: {
              "nudge_of": "u2"
          }
      },
      {
          "id": "u2",
          "type": ET.USER,
          "content": "second"
      },
      {
          "id": "w1",
          "type": ET.WEB_REPLY_NOTICE,
          "content": "notice",
          ET.WEB_REPLY_NOTICE: {
              ET.INPUT_EVENT_ID: "u2"
          }
      },
  ]

  assert reply.for_you_entries(events) == [
      {
          "role": "user",
          "content": "first"
      },
      {
          "role": "web_reply",
          "content": "answer"
      },
      {
          "role": "user",
          "content": "second"
      },
      {
          "role": "web_reply_notice",
          "content": "notice"
      },
  ]


# ---------------------------------------------------------------------------
# answers: the user message or nudge the running turn answers
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_a_reply_answers_the_user_message_that_woke_its_turn(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  user = await _append(blocks, sid, conftest.user_event("the question"))

  with _running_turn(blocks, sid, [user["id"]]):
    readback = await reply.post_web_reply(sid, "the answer", blocks.store, blocks.events, blocks.lifecycle)

  assert readback["answers"] == user["id"]


@pytest.mark.asyncio
async def test_a_reply_in_a_nudge_turn_answers_the_user_message_the_nudge_asks_about(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  user = await _append(blocks, sid, conftest.user_event("the question"))
  nudge = await _append(
      blocks, sid, {
          "type": ET.AGENT_MESSAGE,
          "content": reply.NUDGE_TEXT,
          ET.WEB_REPLY: {
              "nudge_of": user["id"]
          },
      })

  with _running_turn(blocks, sid, [nudge["id"]]):
    readback = await reply.post_web_reply(sid, "the answer", blocks.store, blocks.events, blocks.lifecycle)

  assert readback["answers"] == user["id"]


@pytest.mark.asyncio
async def test_a_reply_in_a_background_turn_answers_nothing(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  wake = await _append(blocks, sid, conftest.scheduled_trigger_event("wake"))

  with _running_turn(blocks, sid, [wake["id"]]):
    readback = await reply.post_web_reply(sid, "the answer", blocks.store, blocks.events, blocks.lifecycle)

  assert readback["answers"] is None


@pytest.mark.asyncio
async def test_a_reply_answers_the_newest_input_in_log_order_not_in_input_order(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  first = await _append(blocks, sid, conftest.user_event("first"))
  second = await _append(blocks, sid, conftest.user_event("second"))

  with _running_turn(blocks, sid, [second["id"], first["id"]]):
    readback = await reply.post_web_reply(sid, "the answer", blocks.store, blocks.events, blocks.lifecycle)

  assert readback["answers"] == second["id"]


# ---------------------------------------------------------------------------
# The round-end audit: one nudge, one notice, none after an error
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_a_user_turn_without_a_reply_gets_one_nudge_that_wakes_a_turn(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  user = await _append(blocks, sid, conftest.user_event("the question"))
  done = _done([user["id"]])
  tasks: list[asyncio.Task] = []

  with _wake_seams(tasks) as trigger:
    await audit.audit_turn(sid, done, blocks.store, blocks.events, blocks.lifecycle, blocks.successor)
    await audit.audit_turn(sid, done, blocks.store, blocks.events, blocks.lifecycle, blocks.successor)
    await asyncio.gather(*tasks)

  nudges = _nudges(_events(blocks, sid))
  assert [(ev["content"], ev[ET.WEB_REPLY]) for ev in nudges] == [(reply.NUDGE_TEXT, {"nudge_of": user["id"]})]
  trigger.assert_awaited_once()
  assert trigger.await_args.args[1] == reply.NUDGE_TEXT
  assert trigger.await_args.kwargs["input_id"] == nudges[0]["id"]
  assert trigger.await_args.kwargs["event_type"] == ET.AGENT_MESSAGE


@pytest.mark.asyncio
async def test_a_nudge_turn_without_a_reply_posts_one_notice_and_marks_the_session_unread(
    tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  user = await _append(blocks, sid, conftest.user_event("the question"))
  nudge = await _post_nudge(blocks, sid, user["id"])
  done = _done([nudge["id"]])
  tasks: list[asyncio.Task] = []

  with _wake_seams(tasks) as trigger:
    await audit.audit_turn(sid, done, blocks.store, blocks.events, blocks.lifecycle, blocks.successor)
    await audit.audit_turn(sid, done, blocks.store, blocks.events, blocks.lifecycle, blocks.successor)
    await asyncio.gather(*tasks)

  notices = _of_type(_events(blocks, sid), ET.WEB_REPLY_NOTICE)
  assert [(ev["content"], ev[ET.WEB_REPLY_NOTICE]) for ev in notices] == [
      (reply.NOTICE_TEXT, {
          ET.INPUT_EVENT_ID: user["id"]
      })
  ]
  trigger.assert_not_awaited()
  assert (await blocks.store.get_session(sid)).has_unread


@pytest.mark.asyncio
async def test_a_reply_in_the_nudge_turn_ends_the_audit_with_no_notice(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  user = await _append(blocks, sid, conftest.user_event("the question"))
  nudge = await _post_nudge(blocks, sid, user["id"])

  with _running_turn(blocks, sid, [nudge["id"]]):
    await reply.post_web_reply(sid, "the answer", blocks.store, blocks.events, blocks.lifecycle)
  await audit.audit_turn(sid, _done([nudge["id"]]), blocks.store, blocks.events, blocks.lifecycle, blocks.successor)

  events = _events(blocks, sid)
  assert not _of_type(events, ET.WEB_REPLY_NOTICE)
  assert [ev[ET.WEB_REPLY]["answers"] for ev in _of_type(events, ET.WEB_REPLY)] == [user["id"]]


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "session_kind, input_kind, expect_nudge", [
        ("web", "user", True),
        ("web", "background", False),
        ("thread", "user", False),
    ])
async def test_the_after_turn_hook_audits_only_a_web_turn_that_answers_a_user(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, session_kind: str, input_kind: str,
    expect_nudge: bool) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks, slack_origin=_SLACK_ORIGIN if session_kind == "thread" else None)
  if input_kind == "user":
    input_id = (await _append(blocks, sid, conftest.user_event("the question")))["id"]
  else:
    input_id = (await _append(blocks, sid, conftest.scheduled_trigger_event("wake")))["id"]
  tasks: list[asyncio.Task] = []

  with _wake_seams(tasks):
    await _after_turn(monkeypatch, blocks, sid, _done([input_id]))
    await asyncio.gather(*tasks)

  assert bool(_nudges(_events(blocks, sid))) is expect_nudge


@pytest.mark.asyncio
@pytest.mark.parametrize("errored_input", ["user", "nudge"])
async def test_an_errored_turn_gets_no_audit(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, errored_input: str) -> None:
  """An errored user turn gets no nudge; an errored nudge turn gets no notice."""
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  user = await _append(blocks, sid, conftest.user_event("the question"))
  input_id = user["id"]
  if errored_input == "nudge":
    input_id = (await _post_nudge(blocks, sid, user["id"]))["id"]
  tasks: list[asyncio.Task] = []

  with _wake_seams(tasks) as trigger:
    await _after_turn(monkeypatch, blocks, sid, _done([input_id], exit_code=1))
    await asyncio.gather(*tasks)

  events = _events(blocks, sid)
  trigger.assert_not_awaited()
  assert len(_nudges(events)) == (1 if errored_input == "nudge" else 0)
  assert not _of_type(events, ET.WEB_REPLY_NOTICE)


# ---------------------------------------------------------------------------
# The instruction segment and the live broadcast
# ---------------------------------------------------------------------------


def test_a_web_manager_run_carries_the_reply_rules_and_thread_and_worker_runs_do_not(tmp_path: pathlib.Path) -> None:
  cfg = types.SimpleNamespace(
      charlie_bot_repo=conftest.ROOT, claude_md_file=tmp_path / "MASTER_AGENT_PROMPT.md", charliebot_home=tmp_path)
  web = models.SessionMetadata(id="web", name="web", profile="manager")
  thread = models.SessionMetadata(id="thread", name="thread", profile="manager", slack_origin=_SLACK_ORIGIN)
  rules_text = (conftest.ROOT / "prompts" / reply.RULES_FILE).read_text(encoding="utf-8").strip()

  def manager_sources(meta: models.SessionMetadata) -> list[tuple[str, ...]]:
    segments, _ = task_prompts.build_segments(cfg, meta, "manager_turn", overlay=None, chain=(), node_ref=None)
    return [
        tuple(source.source_ref for source in segment.sources) for segment in segments if segment.text == rules_text
    ]

  assert manager_sources(web) == [(f"prompts/{reply.RULES_FILE}",)]
  assert manager_sources(thread) == []
  assert web_turn_contribution.CONTRIBUTION.instruction_segments(web, "work", cfg) == []


@pytest.mark.asyncio
async def test_a_live_web_reply_and_notice_reach_a_connected_client_once_each(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  reply_event = {"type": ET.WEB_REPLY, "content": "the answer", ET.WEB_REPLY: {"answers": None, "chars": 10}}
  notice_event = {"type": ET.WEB_REPLY_NOTICE, "content": "the notice", ET.WEB_REPLY_NOTICE: {ET.INPUT_EVENT_ID: "u1"}}

  with mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()) as broadcast:
    await blocks.events.persist_and_broadcast(sid, reply_event)
    await blocks.events.persist_and_broadcast(sid, notice_event)

  frames = [call.args[1] for call in broadcast.await_args_list]
  rendered = [frame["message"] for frame in frames if frame.get("type") == "message"]
  assert [(m["role"], m["content"]) for m in rendered if m["role"] in ("web_reply", "web_reply_notice")] == [
      ("web_reply", "the answer"),
      ("web_reply_notice", "the notice"),
  ]
  assert [frame for frame in frames if frame.get("type") in (ET.WEB_REPLY, ET.WEB_REPLY_NOTICE)] == []


# ---------------------------------------------------------------------------
# The session status: the set, its refusals, the read, the bootstrap, the rendering and the ancestor notice
# ---------------------------------------------------------------------------

_STATUS_URL = "/api/internal/web/status"
_VALID_SET = json.dumps({"now": "Running"})


def _status_client(blocks: conftest.SessionBlocks) -> TestClient:
  return conftest.make_internal_router_client(blocks.cfg, blocks, blocks.tree)


def _set(client: TestClient, sid: str, text: str) -> httpx.Response:
  return client.post(_STATUS_URL, json={"session_id": sid, "text": text})


def _waiting_set(text: str) -> str:
  return json.dumps({"waiting": [{"text": text}]})


async def _stored_status(blocks: conftest.SessionBlocks, sid: str) -> dict | None:
  return (await blocks.store.get_session(sid)).model_dump(mode="json")["web_status"]


def _status_events(blocks: conftest.SessionBlocks, sid: str) -> list[dict]:
  return _of_type(_events(blocks, sid), ET.WEB_STATUS)


@pytest.mark.asyncio
async def test_a_status_set_of_now_alone_keeps_the_other_items_and_records_one_event(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)

  first = _set(
      client, sid, json.dumps({
          "goal": "Ship the panel",
          "waiting": [{
              "text": "Pick a layout",
              "link": "/p"
          }]
      }))
  second = _set(client, sid, _VALID_SET)

  assert first.status_code == 200 and second.status_code == 200
  stored = second.json()
  assert stored["goal"] == "Ship the panel"
  assert stored["now"] == "Running"
  assert stored["waiting"] == [{"text": "Pick a layout", "link": "/p"}]
  assert stored["updated_at"] != first.json()["updated_at"]
  assert await _stored_status(blocks, sid) == stored
  events = _status_events(blocks, sid)
  assert [ev["web_status_changed"] for ev in events] == [["goal", "waiting"], ["now"]]
  assert events[-1]["content"] == "Status updated: Now"
  assert events[-1]["web_status"] == stored


@pytest.mark.asyncio
async def test_a_null_goal_clears_the_goal_and_keeps_the_other_items(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  _set(client, sid, json.dumps({"goal": "Ship the panel", "now": "Running"}))

  resp = _set(client, sid, json.dumps({"goal": None}))

  assert resp.status_code == 200
  assert (resp.json()["goal"], resp.json()["now"]) == (None, "Running")
  assert _status_events(blocks, sid)[-1]["web_status_changed"] == ["goal"]


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "case, text, status", [
        ("unknown", _VALID_SET, 404),
        ("thread", _VALID_SET, 409),
        ("worker", _VALID_SET, 409),
        ("own", "not json", 422),
        ("own", "[1, 2]", 422),
        ("own", json.dumps({"colour": "red"}), 422),
        ("own", "{}", 422),
        ("own", json.dumps({"goal": " "}), 422),
        ("own", json.dumps({"waiting": [{
            "text": " "
        }]}), 422),
        ("own", json.dumps({"waiting": [{
            "text": "Decide",
            "due": "today"
        }]}), 422),
    ])
async def test_a_refused_status_set_stores_nothing_and_records_no_event(
    tmp_path: pathlib.Path, case: str, text: str, status: int) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks, slack_origin=_SLACK_ORIGIN if case == "thread" else None)
  if case == "worker":
    await blocks.store.update_field(sid, "profile", "worker", "test_worker_profile")
  client = _status_client(blocks)

  resp = _set(client, "no-such-session" if case == "unknown" else sid, text)

  assert resp.status_code == status
  assert await _stored_status(blocks, sid) is None
  assert _status_events(blocks, sid) == []


@pytest.mark.asyncio
async def test_the_bootstrap_session_object_carries_the_session_status(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = conftest.make_sessions_client(blocks.cfg, blocks)
  client.app.dependency_overrides[deps.get_task_manager] = lambda: blocks.tree
  assert client.get(f"/api/sessions/{sid}/bootstrap").json()["session"]["web_status"] is None

  _set(_status_client(blocks), sid, _VALID_SET)

  assert client.get(f"/api/sessions/{sid}/bootstrap").json()["session"]["web_status"]["now"] == "Running"


@pytest.mark.asyncio
async def test_the_history_renders_a_status_set_as_a_web_status_message(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  _set(_status_client(blocks), sid, _VALID_SET)

  rows = [m for m in message_utils.events_to_messages(_events(blocks, sid)) if m["role"] == ET.WEB_STATUS]

  assert [(m["content"], m["web_status"]["now"]) for m in rows] == [("Status updated: Now", "Running")]
  assert rows[0]["id"] and rows[0]["timestamp"]


@pytest.mark.asyncio
async def test_a_live_status_set_reaches_a_connected_client_once(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)

  with mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()) as broadcast:
    _set(client, sid, _VALID_SET)

  frames = [call.args[1] for call in broadcast.await_args_list]
  rendered = [frame["message"] for frame in frames if frame.get("type") == "message"]
  assert [(m["role"], m["content"]) for m in rendered if m["role"] == ET.WEB_STATUS] == [
      (ET.WEB_STATUS, "Status updated: Now")
  ]
  assert [frame for frame in frames if frame.get("type") == ET.WEB_STATUS] == []


@pytest.mark.asyncio
async def test_a_child_status_set_notifies_its_parent_and_grandparent_pages_and_a_root_set_notifies_none(
    tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  root_id = await _session(blocks)
  child = await conftest.create_task(blocks.tree, parent=root_id, request_id="child", name="Child")
  grandchild = await conftest.create_task(blocks.tree, parent=child.id, request_id="grandchild", name="Grandchild")
  client = _status_client(blocks)

  with mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.AsyncMock()) as broadcast:
    _set(client, grandchild.id, _VALID_SET)
    _set(client, root_id, _VALID_SET)

  notices = [
      (call.args[0], call.args[1])
      for call in broadcast.await_args_list
      if call.args[1].get("type") == "web_status_subtree"
  ]
  assert notices == [
      (streaming.session_channel(child.id), {
          "type": "web_status_subtree",
          "source_session_id": grandchild.id
      }),
      (streaming.session_channel(root_id), {
          "type": "web_status_subtree",
          "source_session_id": grandchild.id
      }),
  ]


@pytest.mark.asyncio
async def test_the_status_read_lists_open_waiting_descendants_newest_first(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  root_id = await _session(blocks)
  tree = blocks.tree
  old = await conftest.create_task(tree, parent=root_id, request_id="old", name="Old")
  new = await conftest.create_task(tree, parent=root_id, request_id="new", name="New")
  deep = await conftest.create_task(tree, parent=old.id, request_id="deep", name="Deep")
  empty = await conftest.create_task(tree, parent=root_id, request_id="empty", name="Empty")
  closed = await conftest.create_task(tree, parent=root_id, request_id="closed", name="Closed")
  archived = await conftest.create_task(tree, parent=root_id, request_id="archived", name="Archived")
  client = _status_client(blocks)
  for child, text in [
      (old, _waiting_set("Old decision")),
      (new, _waiting_set("New decision")),
      (deep, _waiting_set("Deep decision")),
      (empty, json.dumps({"waiting": []})),
      (closed, _waiting_set("Closed decision")),
      (archived, _waiting_set("Archived decision")),
  ]:
    _set(client, child.id, text)
  await tree.archive_subtree(closed.id, caller=conftest.OPERATOR)
  await blocks.lifecycle.archive_session(archived.id)

  body = client.get(_STATUS_URL, params={"session_id": root_id}).json()

  assert body["web_status"] is None
  assert [row["session_id"] for row in body["descendants"]] == [deep.id, new.id, old.id]
  assert [row["waiting"] for row in body["descendants"]] == [
      [{
          "text": "Deep decision",
          "link": None
      }],
      [{
          "text": "New decision",
          "link": None
      }],
      [{
          "text": "Old decision",
          "link": None
      }],
  ]


@pytest.mark.asyncio
@pytest.mark.parametrize("case, status", [("unknown", 404), ("thread", 409), ("worker", 409)])
async def test_the_status_read_refuses_an_unknown_thread_or_worker_session(
    tmp_path: pathlib.Path, case: str, status: int) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks, slack_origin=_SLACK_ORIGIN if case == "thread" else None)
  if case == "worker":
    await blocks.store.update_field(sid, "profile", "worker", "test_worker_profile")
  client = _status_client(blocks)

  resp = client.get(_STATUS_URL, params={"session_id": "no-such-session" if case == "unknown" else sid})

  assert resp.status_code == status


@pytest.mark.asyncio
async def test_a_status_event_leaves_the_unread_mark_unchanged(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  assert not (await blocks.store.get_session(sid)).has_unread

  _set(client, sid, _VALID_SET)
  assert not (await blocks.store.get_session(sid)).has_unread

  await blocks.lifecycle.mark_unread(sid)
  _set(client, sid, json.dumps({"now": "Done"}))
  assert (await blocks.store.get_session(sid)).has_unread


@pytest.mark.parametrize("stdin", [False, True])
def test_the_cli_status_set_posts_the_file_and_prints_the_stored_status_as_one_json_line(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], stdin: bool) -> None:
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  stored = {"goal": "g", "now": "n", "waiting": [], "updated_at": "2026-10-16T21:00:00+00:00"}
  argv = ["web", "status", "set", "--file", "-"]
  if stdin:
    monkeypatch.setattr("sys.stdin", io.StringIO('{"now": "n"}'))
  else:
    status_file = tmp_path / "status.json"
    status_file.write_text('{"now": "n"}', encoding="utf-8")
    argv = ["web", "status", "set", "--file", str(status_file)]

  with conftest.patched_cli_post(cfg, argv, return_value=conftest.make_json_response(stored)) as post_mock:
    importlib.import_module("src.features.web_reply.cli").main()

  assert post_mock.call_args.args[0].endswith("/api/internal/web/status")
  assert post_mock.call_args.kwargs["json"] == {"session_id": "abc", "text": '{"now": "n"}'}
  out = capsys.readouterr().out
  assert out.count("\n") == 1
  assert json.loads(out) == stored


@pytest.mark.parametrize("has_status", [False, True])
def test_the_cli_status_show_prints_the_status_and_one_block_per_child_with_waiting_items(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str],
    has_status: bool) -> None:
  conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  web_status = {
      "goal": "Ship",
      "now": "Testing",
      "waiting": [{
          "text": "Pick a layout",
          "link": None
      }],
      "updated_at": "2026-10-16T21:00:00+00:00",
  } if has_status else None
  child = {
      "session_id": "d7645e8f-0000-4000-8000-000000000000",
      "name": "Panel child",
      "updated_at": "2026-10-16T21:05:00+00:00",
      "waiting": [{
          "text": "Answer plan 5",
          "link": "/x"
      }],
  }
  calls: list[tuple[str, dict]] = []

  def fake_get(endpoint: str, params: dict | None = None) -> dict:
    calls.append((endpoint, params or {}))
    return {"session_id": "abc", "web_status": web_status, "descendants": [child]}

  monkeypatch.setattr(common, "get_api", fake_get)
  monkeypatch.setattr(sys, "argv", ["web", "status", "show"])

  importlib.import_module("src.features.web_reply.cli").main()

  assert calls == [("/api/internal/web/status", {"session_id": "abc"})]
  out = capsys.readouterr().out
  assert ("No status yet" in out) is (not has_status)
  assert "Panel child (d7645e8f)" in out
  assert out.index("Panel child (d7645e8f)") < out.index("Answer plan 5 (/x)")
