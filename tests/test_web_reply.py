"""Acceptance tests for the web reply channel (src.features.web_reply): the reply record and its refusals, the language
check, the answers binding, the round-end audit, the For-you layer, the rules segment, the live broadcast of a reply,
and the session status: its set and read, their refusals, the bootstrap object, the rendering and the ancestor notice."""

from __future__ import annotations

import asyncio
import contextlib
import datetime
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
from fastapi import testclient

from src.features.chat_threads import thread_sessions
from src.features.discord import metadata as discord_metadata
from src.features.slack import metadata as slack_metadata
from src.features.web_reply import audit, cli, metadata, reply, status
from src.features.web_reply import turn_contribution as web_turn_contribution
from src.infra import event_types as ET
from src.infra import models
from src.runtime import backend_pool, control_events, file_urls, master_cc_state, run_token, streaming, task_prompts
from src.runtime.api import deps, message_utils
from src.runtime.cli import common
from src.runtime.hooks import backend_types

_SLACK_ORIGIN = slack_metadata.SlackOrigin(team_id="T1", channel_id="C1", thread_ts="1700000000.000100")
_DISCORD_ORIGIN = discord_metadata.DiscordOrigin(guild_id="G1", parent_channel_id="C1", thread_id="1234567890")


def _rig(tmp_path: pathlib.Path) -> conftest.SessionBlocks:
  return conftest.build_session_blocks(conftest.build_master_cc_cfg(tmp_path))


def _scan_context(blocks: conftest.SessionBlocks, session_id: str) -> file_urls.ScanContext:
  return file_urls.ScanContext(
      server_port=blocks.cfg.server.port,
      charliebot_home=blocks.cfg.charliebot_home,
      session_dir=blocks.store.session_dir(session_id),
      user_home=blocks.cfg.charliebot_home.parent)


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


def _nudges(events: list[dict]) -> list[dict]:
  return [ev for ev in conftest.events_of_type(events, ET.AGENT_MESSAGE) if reply.nudge_of(ev) is not None]


def _status_reminders(events: list[dict]) -> list[dict]:
  return [ev for ev in events if audit.is_status_reminder(ev)]


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
      mock.patch(conftest.MASTER_TRIGGER_PATCH_TARGET, trigger),
      mock.patch(conftest.TASKS_CREATE_LOGGED_TASK_PATCH_TARGET, side_effect=conftest.make_task_spawner(tasks)),
  ):
    yield trigger


async def _reply_step_audit(blocks: conftest.SessionBlocks, sid: str, done: dict) -> None:
  """The audit's reply step alone: an archived session skips the status reminder."""
  assert blocks.tree is not None, "a session-creating rig builds the task tree"
  await audit.audit_turn(
      sid, done, blocks.store, blocks.events, blocks.lifecycle, blocks.successor, blocks.tree, archived=True)


async def _post_nudge(blocks: conftest.SessionBlocks, sid: str, user_event_id: str) -> dict:
  """Audit a turn that answered *user_event_id* without a reply; returns the nudge it persisted."""
  tasks: list[asyncio.Task] = []
  with _wake_seams(tasks):
    await _reply_step_audit(blocks, sid, _done([user_event_id]))
    await asyncio.gather(*tasks)
  return _nudges(_events(blocks, sid))[-1]


async def _after_turn(monkeypatch: pytest.MonkeyPatch, blocks: conftest.SessionBlocks, sid: str, done: dict) -> None:
  """The web contribution's after_turn for one finished turn, over the process singletons bound to *blocks*."""
  assert blocks.tree is not None, "a session-creating rig builds the task tree"
  conftest.bind_deps_blocks(monkeypatch, blocks.tree, blocks)
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
  replies = conftest.events_of_type(_events(blocks, sid), ET.WEB_REPLY)
  assert [(ev["content"], ev[ET.WEB_REPLY]) for ev in replies] == [("the answer", {"answers": None, "chars": 10})]
  assert (await blocks.store.get_session(sid)).has_unread


@pytest.mark.asyncio
async def test_concurrent_web_reply_replay_returns_one_stored_receipt(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  route_client = conftest.make_internal_router_client(blocks.cfg, blocks, blocks.tree)
  claims = run_token.RunTokenClaims(session_id=sid, run_id="web-run", agent="manager")
  route_client.app.dependency_overrides[deps.require_caller] = lambda: run_token.CallerIdentity(
      kind="agent", claims=claims)

  with conftest.prime_barrier(blocks.events, sid):
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=route_client.app), base_url="http://test") as client:
      responses = await asyncio.gather(
          *
          [client.post("/api/internal/web/reply", json={
              "session_id": sid,
              "text": "same response"
          }) for _ in range(2)])

  assert [response.status_code for response in responses] == [200, 200]
  assert responses[0].json() == responses[1].json() == {
      "posted": True,
      "chars": len("same response"),
      "over_budget": False,
      "answers": None,
  }
  replies = conftest.events_of_type(_events(blocks, sid), ET.WEB_REPLY)
  assert len(replies) == 1
  assert replies[0]["request_id"] == control_events.derived_agent_send_request_id(
      "web-run", "web reply", sid, "same response")


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
@pytest.mark.parametrize("form", ["retired", "local", "canonical"])
async def test_a_dead_file_link_refuses_a_web_reply_with_a_correct_link_and_records_nothing(
    tmp_path: pathlib.Path, form: str) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = conftest.make_internal_router_client(blocks.cfg, blocks)
  context = _scan_context(blocks, sid)
  if form == "retired":
    target = context.charliebot_home / "sessions" / tmp_path.name / "plan.html"
    target.parent.mkdir(parents=True)
    target.write_text("page", encoding="utf-8")
    link = f"https://charliebot.example:{context.server_port}/files/sessions/{tmp_path.name}/plan.html"
    correct = f"https://charliebot.example:{context.server_port}/absolute_filepath{target}"
    detail = f"Dead file links; nothing was recorded.\n- {link}: /files/ is a retired prefix. Correct link: {correct}"
  elif form == "local":
    link = "[plan](artifacts/plan.html)"
    correct = _host_file_link(context.session_dir / "artifacts" / "plan.html")
    detail = (
        f"Dead file links; nothing was recorded.\n- artifacts/plan.html: a local path opens nothing on the web page. "
        f"Correct link: {correct}")
  else:
    target = tmp_path / "missing.html"
    link = f"http://localhost:{context.server_port}/absolute_filepath{target}"
    detail = f"Dead file links; nothing was recorded.\n- {link}: no file exists at {target}."
  events_before = _events(blocks, sid)

  response = client.post("/api/internal/web/reply", json={"session_id": sid, "text": f"See {link}"})

  assert response.status_code == 422
  assert response.json()["detail"] == detail
  assert _events(blocks, sid) == events_before
  assert conftest.events_of_type(_events(blocks, sid), ET.WEB_REPLY) == []


@pytest.mark.asyncio
async def test_a_live_file_link_posts_as_a_web_reply(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  page = tmp_path / "live.html"
  page.write_text("page", encoding="utf-8")
  link = _host_file_link(page)
  client = conftest.make_internal_router_client(blocks.cfg, blocks)

  response = client.post("/api/internal/web/reply", json={"session_id": sid, "text": f"See {link}"})

  assert response.status_code == 200
  assert [event["content"] for event in conftest.events_of_type(_events(blocks, sid), ET.WEB_REPLY)] == [f"See {link}"]


@pytest.mark.asyncio
@pytest.mark.parametrize("chars, over_budget", [(500, False), (501, True)])
async def test_the_budget_flags_an_over_budget_reply_and_the_reply_still_posts(
    tmp_path: pathlib.Path, chars: int, over_budget: bool) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  text = "x" * chars

  readback = await reply.post_web_reply(
      sid,
      text,
      blocks.store,
      blocks.events,
      blocks.lifecycle,
      tree=blocks.tree,
      scan_context=_scan_context(blocks, sid))

  assert readback == {"posted": True, "chars": chars, "over_budget": over_budget, "answers": None}
  assert [ev["content"] for ev in conftest.events_of_type(_events(blocks, sid), ET.WEB_REPLY)] == [text]


@pytest.mark.asyncio
async def test_markdown_link_labels_set_the_reply_count_and_replay_readback(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  diff_prefix = "https://example.com/"
  pr_prefix = "https://example.com/"
  diff_url = diff_prefix + "d" * (150 - len(diff_prefix))
  pr_url = pr_prefix + "p" * (183 - len(pr_prefix))
  text = "x" * 200 + f"[diff]({diff_url})[PR 页]({pr_url})"
  expected = {"posted": True, "chars": 208, "over_budget": False, "answers": None}

  readback = await reply.post_web_reply(
      sid,
      text,
      blocks.store,
      blocks.events,
      blocks.lifecycle,
      request_id="same-reply",
      tree=blocks.tree,
      scan_context=_scan_context(blocks, sid))
  replay = await reply.post_web_reply(
      sid,
      text,
      blocks.store,
      blocks.events,
      blocks.lifecycle,
      request_id="same-reply",
      tree=blocks.tree,
      scan_context=_scan_context(blocks, sid))

  assert len(diff_url) == 150
  assert len(pr_url) == 183
  assert readback == replay == expected
  replies = conftest.events_of_type(_events(blocks, sid), ET.WEB_REPLY)
  assert len(replies) == 1
  assert replies[0][ET.WEB_REPLY]["chars"] == 208


@pytest.mark.asyncio
async def test_a_bare_url_counts_in_full_in_a_web_reply(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  text = "See https://example.com/" + "u" * 120

  readback = await reply.post_web_reply(
      sid,
      text,
      blocks.store,
      blocks.events,
      blocks.lifecycle,
      tree=blocks.tree,
      scan_context=_scan_context(blocks, sid))

  assert readback == {"posted": True, "chars": len(text), "over_budget": False, "answers": None}
  stored = conftest.events_of_type(_events(blocks, sid), ET.WEB_REPLY)[0][ET.WEB_REPLY]
  assert stored["chars"] == len(text)


# ---------------------------------------------------------------------------
# The language check: an English reply to a Chinese message is held unless the agent gives a reason
# ---------------------------------------------------------------------------

_CHINESE_MESSAGE = "这个改动先别合入，我想再看一下状态面板的折叠逻辑。"
_MANY_TERMS_MESSAGE = "这个 PR 的 CI 挂了，pytest 的 log 里有 Kubernetes pod 的 timeout，你看一下 rebase 之后还在不在。"
_ARTIFACT_COMMENT_MESSAGE = (
    "[Artifact comments · plan_32.html] (1)\n\n"
    "1. ▸ \"Keep the phone status toggle sticky while scrolling\"\n"
    "   ↳ 这一条为什么要改？先说明理由再动手。")
_ENGLISH_REPLY = "I will hold the merge until the review is done."
_CODE_HEAVY_REPLY = "改好了，入口在下面：\n\n```\n" + " ".join(["ChatPanelStatusToggle"] * 20) + "\n```\n测试全部通过。"


def _user_message(text: str) -> dict:
  """A user message as the chat API writes it: the actor is the user."""
  return {**conftest.user_event(text), "actor": "user"}


async def _post_reply(blocks: conftest.SessionBlocks, sid: str, text: str, keep_language: str | None = None) -> dict:
  return await reply.post_web_reply(
      sid,
      text,
      blocks.store,
      blocks.events,
      blocks.lifecycle,
      keep_language=keep_language,
      tree=blocks.tree,
      scan_context=_scan_context(blocks, sid))


@pytest.mark.asyncio
async def test_an_english_reply_after_a_chinese_message_is_held_and_records_nothing(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  await _append(blocks, sid, _user_message(_CHINESE_MESSAGE))
  events_before = len(_events(blocks, sid))

  with pytest.raises(reply.WebReplyError) as refusal:
    await _post_reply(blocks, sid, _ENGLISH_REPLY)

  assert (refusal.value.status, refusal.value.detail) == (409, reply.LANGUAGE_HOLD_DETAIL)
  assert len(_events(blocks, sid)) == events_before
  assert not (await blocks.store.get_session(sid)).has_unread


@pytest.mark.asyncio
async def test_a_reply_with_a_keep_language_reason_posts_as_written_and_records_the_reason(
    tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  await _append(blocks, sid, _user_message(_CHINESE_MESSAGE))
  client = conftest.make_internal_router_client(blocks.cfg, blocks)
  reason = "The reply quotes an English term."

  resp = client.post(
      "/api/internal/web/reply", json={
          "session_id": sid,
          "text": _ENGLISH_REPLY,
          "keep_language": reason
      })

  assert resp.status_code == 200
  replies = conftest.events_of_type(_events(blocks, sid), ET.WEB_REPLY)
  assert [(ev["content"], ev[ET.WEB_REPLY]) for ev in replies] == [
      (_ENGLISH_REPLY, {
          "answers": None,
          "chars": len(_ENGLISH_REPLY),
          "keep_language": reason
      })
  ]


@pytest.mark.asyncio
@pytest.mark.parametrize("reason", ["", "  "])
async def test_an_empty_keep_language_reason_is_refused_and_records_nothing(
    tmp_path: pathlib.Path, reason: str) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  await _append(blocks, sid, _user_message(_CHINESE_MESSAGE))
  client = conftest.make_internal_router_client(blocks.cfg, blocks)
  events_before = len(_events(blocks, sid))

  resp = client.post(
      "/api/internal/web/reply", json={
          "session_id": sid,
          "text": _ENGLISH_REPLY,
          "keep_language": reason
      })

  assert resp.status_code == 422
  assert len(_events(blocks, sid)) == events_before


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "user_message", [_MANY_TERMS_MESSAGE, _ARTIFACT_COMMENT_MESSAGE],
    ids=["many-english-terms", "artifact-comment-with-english-quote"])
async def test_an_english_reply_after_a_chinese_message_with_english_inside_it_is_held(
    tmp_path: pathlib.Path, user_message: str) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  await _append(blocks, sid, _user_message(user_message))

  with pytest.raises(reply.WebReplyError) as refusal:
    await _post_reply(blocks, sid, _ENGLISH_REPLY)

  assert refusal.value.detail == reply.LANGUAGE_HOLD_DETAIL


@pytest.mark.asyncio
async def test_an_english_reply_after_an_english_message_posts_even_after_a_chinese_one(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  await _append(blocks, sid, _user_message(_CHINESE_MESSAGE))
  await _append(blocks, sid, _user_message("Please rerun the suite and report the failures."))

  readback = await _post_reply(blocks, sid, _ENGLISH_REPLY)

  assert readback["posted"] is True


@pytest.mark.asyncio
async def test_a_chinese_reply_with_many_code_names_posts(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  await _append(blocks, sid, _user_message(_CHINESE_MESSAGE))

  readback = await _post_reply(blocks, sid, _CODE_HEAVY_REPLY)

  assert readback["posted"] is True


@pytest.mark.asyncio
async def test_a_reply_in_a_session_without_a_user_message_posts(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)

  readback = await _post_reply(blocks, sid, _ENGLISH_REPLY)

  assert readback["posted"] is True


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
    cli.main()

  assert post_mock.call_args.args[0].endswith("/api/internal/web/reply")
  assert post_mock.call_args.kwargs["json"] == {"session_id": "abc", "text": "the answer"}
  out = capsys.readouterr().out
  assert out.count("\n") == 1
  assert json.loads(out) == readback


@pytest.mark.parametrize("detail", ["Session is not a web session", reply.LANGUAGE_HOLD_DETAIL])
def test_the_cli_exits_non_zero_with_a_json_error_on_a_refusal(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], detail: str) -> None:
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  reply_file = tmp_path / "reply.md"
  reply_file.write_text("the answer", encoding="utf-8")
  refused = conftest.make_json_response({"detail": detail}, 409)

  with (
      conftest.patched_cli_post(cfg, ["web", "reply", "--file", str(reply_file)], return_value=refused),
      mock.patch(conftest.CLI_COMMON_MAYBE_VERSION_SKEW_HINT_PATCH_TARGET, return_value=None),
      pytest.raises(SystemExit) as exc_info,
  ):
    cli.main()

  assert exc_info.value.code == 1
  assert json.loads(capsys.readouterr().err) == {
      "error": detail,
      "code": "server_error",
      "effect": "none",
  }


def test_the_cli_sends_the_keep_language_reason_with_the_reply(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  reply_file = tmp_path / "reply.md"
  reply_file.write_text("the answer", encoding="utf-8")
  argv = ["web", "reply", "--file", str(reply_file), "--keep-language", "The reply quotes an English term."]
  readback = {"posted": True, "chars": 10, "over_budget": False, "answers": None}

  with conftest.patched_cli_post(cfg, argv, return_value=conftest.make_json_response(readback)) as post_mock:
    cli.main()

  assert post_mock.call_args.kwargs["json"] == {
      "session_id": "abc",
      "text": "the answer",
      "keep_language": "The reply quotes an English term.",
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

  cli.main()

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
    readback = await _post_reply(blocks, sid, "the answer")

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
    readback = await _post_reply(blocks, sid, "the answer")

  assert readback["answers"] == user["id"]


@pytest.mark.asyncio
async def test_a_reply_in_a_background_turn_answers_nothing(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  wake = await _append(blocks, sid, conftest.scheduled_trigger_event("wake"))

  with _running_turn(blocks, sid, [wake["id"]]):
    readback = await _post_reply(blocks, sid, "the answer")

  assert readback["answers"] is None


@pytest.mark.asyncio
async def test_a_reply_answers_the_newest_input_in_log_order_not_in_input_order(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  first = await _append(blocks, sid, conftest.user_event("first"))
  second = await _append(blocks, sid, conftest.user_event("second"))

  with _running_turn(blocks, sid, [second["id"], first["id"]]):
    readback = await _post_reply(blocks, sid, "the answer")

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
    await _reply_step_audit(blocks, sid, done)
    await _reply_step_audit(blocks, sid, done)
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
    await _reply_step_audit(blocks, sid, done)
    await _reply_step_audit(blocks, sid, done)
    await asyncio.gather(*tasks)

  notices = conftest.events_of_type(_events(blocks, sid), ET.WEB_REPLY_NOTICE)
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
    await _post_reply(blocks, sid, "the answer")
  await _reply_step_audit(blocks, sid, _done([nudge["id"]]))

  events = _events(blocks, sid)
  assert not conftest.events_of_type(events, ET.WEB_REPLY_NOTICE)
  assert [ev[ET.WEB_REPLY]["answers"] for ev in conftest.events_of_type(events, ET.WEB_REPLY)] == [user["id"]]


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
  assert not conftest.events_of_type(events, ET.WEB_REPLY_NOTICE)


# ---------------------------------------------------------------------------
# The status reminder: every finished turn, once, and no reminder from its own wake
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_a_turn_without_a_status_gets_one_reminder_and_its_reminder_turn_gets_none(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  done = {**_done([]), "id": "turn-1"}
  tasks: list[asyncio.Task] = []

  with _wake_seams(tasks) as trigger:
    await _after_turn(monkeypatch, blocks, sid, done)
    await _after_turn(monkeypatch, blocks, sid, done)
    await asyncio.gather(*tasks)

  reminders = _status_reminders(_events(blocks, sid))
  assert len(reminders) == 1
  assert reminders[0][ET.WEB_STATUS_REMINDER] == "turn-1"
  trigger.assert_awaited_once()

  reminder = reminders[0]
  reminder_tasks: list[asyncio.Task] = []
  with _wake_seams(reminder_tasks) as reminder_trigger:
    await _after_turn(monkeypatch, blocks, sid, {**_done([reminder["id"]]), "id": "turn-2"})
    await asyncio.gather(*reminder_tasks)

  assert len(_status_reminders(_events(blocks, sid))) == 1
  reminder_trigger.assert_not_awaited()


@pytest.mark.asyncio
async def test_a_turn_missing_reply_and_status_gets_one_combined_reminder_and_one_wake(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  user = await _append(blocks, sid, conftest.user_event("the question"))
  done = {**_done([user["id"]]), "id": "turn-1"}
  tasks: list[asyncio.Task] = []

  with _wake_seams(tasks) as trigger:
    await _after_turn(monkeypatch, blocks, sid, done)
    await asyncio.gather(*tasks)

  events = _events(blocks, sid)
  reminders = _status_reminders(events)
  assert len(reminders) == 1
  assert reminders[0][ET.WEB_REPLY]["nudge_of"] == user["id"]
  assert _nudges(events) == reminders
  trigger.assert_awaited_once()

  reminder_tasks: list[asyncio.Task] = []
  with _wake_seams(reminder_tasks) as reminder_trigger:
    await _after_turn(monkeypatch, blocks, sid, {**_done([reminders[0]["id"]]), "id": "turn-2"})
    await asyncio.gather(*reminder_tasks)

  events = _events(blocks, sid)
  assert len(_status_reminders(events)) == 1
  assert len(conftest.events_of_type(events, ET.WEB_REPLY_NOTICE)) == 1
  reminder_trigger.assert_not_awaited()


@pytest.mark.asyncio
@pytest.mark.parametrize("missing_reply", [False, True])
async def test_a_pending_close_request_skips_only_the_status_reminder(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, missing_reply: bool) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  user = await _append(blocks, sid, conftest.user_event("the question"))
  await _append(blocks, sid, {
      "type": ET.TASK_CLOSE_REQUESTED,
      "request_id": "close-1",
      "owner_run_id": "run-1",
  })
  tasks: list[asyncio.Task] = []

  with _wake_seams(tasks) as trigger:
    await _after_turn(monkeypatch, blocks, sid, {**_done([user["id"]] if missing_reply else []), "id": "turn-1"})
    await asyncio.gather(*tasks)

  assert _status_reminders(_events(blocks, sid)) == []
  assert len(_nudges(_events(blocks, sid))) == int(missing_reply)
  assert trigger.await_count == int(missing_reply)


@pytest.mark.asyncio
@pytest.mark.parametrize("event_type", [ET.WEB_STATUS, ET.WEB_STATUS_CONFIRMED])
async def test_a_turn_that_recorded_a_status_command_gets_no_status_reminder(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, event_type: str) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  await _append(blocks, sid, {"type": event_type, "content": "Status command"})
  done = await _append(blocks, sid, _done([]))
  tasks: list[asyncio.Task] = []

  with _wake_seams(tasks) as trigger:
    await _after_turn(monkeypatch, blocks, sid, done)
    await asyncio.gather(*tasks)

  assert _status_reminders(_events(blocks, sid)) == []
  trigger.assert_not_awaited()

  # A status command from the previous turn cannot satisfy the next background turn.
  next_done = await _append(blocks, sid, _done([]))
  with _wake_seams(tasks) as next_trigger:
    await _after_turn(monkeypatch, blocks, sid, next_done)
    await asyncio.gather(*tasks)
  assert len(_status_reminders(_events(blocks, sid))) == 1
  next_trigger.assert_awaited_once()


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


def _status_client(blocks: conftest.SessionBlocks) -> testclient.TestClient:
  return conftest.make_internal_router_client(blocks.cfg, blocks, blocks.tree)


def _set(client: testclient.TestClient, sid: str, text: str) -> httpx.Response:
  return client.post(_STATUS_URL, json={"session_id": sid, "text": text})


def _waiting_set(text: str) -> str:
  return json.dumps(
      {
          "waiting":
              [
                  {
                      "text": text,
                      "context": "The line is active now. Options are continue or stop; continue is recommended.",
                  }
              ]
      })


def _set_after_reads(client: testclient.TestClient, sid: str, text: str) -> httpx.Response:
  """Record current digests for the waiting items in *text*, then set the patch."""
  for item in json.loads(text).get("waiting", []):
    assert _ask_check(client, sid, item).status_code == 200
  return _set(client, sid, text)


@pytest.mark.asyncio
@pytest.mark.parametrize("endpoint", ["set", "unread"])
@pytest.mark.parametrize("field", ["goal", "now", "waiting_text", "waiting_context", "waiting_link"])
async def test_dead_links_in_every_status_field_refuse_before_any_cold_read_or_write(
    tmp_path: pathlib.Path, endpoint: str, field: str) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  context = _scan_context(blocks, sid)
  missing = tmp_path / "missing-status-page.html"
  canonical_link = f"http://localhost:{context.server_port}/absolute_filepath{missing}"
  local_target = "artifacts/missing-status-page.html"
  item = {
      "text": "Decide",
      "context": "The choice is ready. Options are keep or change; keep is recommended.",
      "link": None,
  }
  if field in ("goal", "now"):
    patch = {field: f"Review [the page]({local_target})"}
    expected = (
        f"Dead file links; nothing was recorded.\n- {local_target}: a local path opens nothing on the web page. "
        f"Correct link: {_host_file_link(context.session_dir / local_target)}")
  else:
    if field == "waiting_text":
      item["text"] = f"Review [the page]({local_target})"
      expected = (
          f"Dead file links; nothing was recorded.\n- {local_target}: a local path opens nothing on the web page. "
          f"Correct link: {_host_file_link(context.session_dir / local_target)}")
    elif field == "waiting_context":
      item["context"] = f"Review [the page]({local_target})"
      expected = (
          f"Dead file links; nothing was recorded.\n- {local_target}: a local path opens nothing on the web page. "
          f"Correct link: {_host_file_link(context.session_dir / local_target)}")
    else:
      item["link"] = canonical_link
      expected = f"Dead file links; nothing was recorded.\n- {canonical_link}: no file exists at {missing}."
    patch = {"waiting": [item]}
  text = json.dumps(patch)
  before = _events(blocks, sid)

  with mock.patch.object(status, "_read_item_digests", side_effect=AssertionError("cold read ran")) as digest_reads:
    response = _unread(client, sid, text) if endpoint == "unread" else _set(client, sid, text)

  assert response.status_code == 422
  assert response.json()["detail"] == expected
  digest_reads.assert_not_called()
  assert await _stored_status(blocks, sid) is None
  assert _events(blocks, sid) == before
  assert _ask_checks(blocks, sid) == []


def _without_budget(status: dict) -> dict:
  """The set's return without the two keys that the Now budget adds to it."""
  return {key: value for key, value in status.items() if key not in ("now_chars", "now_over_budget")}


def _without_item_times(items: list[dict]) -> list[dict]:
  """The waiting items without the start-time fields that the server adds."""
  return [{key: value for key, value in item.items() if key not in ("since", "since_exact")} for item in items]


async def _stored_status(blocks: conftest.SessionBlocks, sid: str) -> dict | None:
  return (await blocks.store.get_session(sid)).model_dump(mode="json")["web_status"]


def _status_events(blocks: conftest.SessionBlocks, sid: str) -> list[dict]:
  return conftest.events_of_type(_events(blocks, sid), ET.WEB_STATUS)


def _waiting_with_link(text: str, link: str) -> str:
  return json.dumps(
      {
          "waiting":
              [
                  {
                      "text": text,
                      "context": "The line is active now. Options are continue or stop; continue is recommended.",
                      "link": link,
                  }
              ]
      })


def _session_artifact(blocks: conftest.SessionBlocks, sid: str, name: str) -> pathlib.Path:
  path = blocks.cfg.sessions_dir / sid / "artifacts" / name
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(name, encoding="utf-8")
  return path


def _plan_version(v: int, relative: str) -> dict:
  return {
      "v": v,
      "file": relative,
      "created_at": "2026-10-24T00:00:00+00:00",
      "trigger": "initial",
      "base": None,
      "note": None,
  }


def _write_plans(blocks: conftest.SessionBlocks, sid: str, plans: list[dict]) -> None:
  path = blocks.cfg.sessions_dir / sid / "plans.json"
  path.write_text(json.dumps({"plans": plans}), encoding="utf-8")


def _plan(plan_id: int, versions: list[dict], closed: dict | None = None) -> dict:
  return {"id": plan_id, "title": f"Plan {plan_id}", "versions": versions, "takeoff": None, "closed": closed}


def _parse_time(value: str) -> datetime.datetime:
  return datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))


@pytest.mark.asyncio
async def test_a_status_set_of_now_alone_keeps_the_other_items_and_records_one_event(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  page = tmp_path / "status-page.html"
  page.write_text("status evidence", encoding="utf-8")

  first = _set_after_reads(
      client, sid,
      json.dumps(
          {
              "goal":
                  "Ship the panel",
              "waiting":
                  [
                      {
                          "text":
                              "Pick a layout",
                          "context":
                              "The panel is one row per item. Options: fold the line or show it. Fold is recommended.",
                          "link":
                              _host_file_link(page)
                      }
                  ]
          }))
  second = _set(client, sid, _VALID_SET)

  assert first.status_code == 200 and second.status_code == 200
  stored = second.json()
  assert stored["goal"] == "Ship the panel"
  assert stored["now"] == "Running"
  assert _without_item_times(stored["waiting"]) == [
      {
          "text": "Pick a layout",
          "context": "The panel is one row per item. Options: fold the line or show it. Fold is recommended.",
          "link": _host_file_link(page),
      }
  ]
  assert stored["updated_at"] != first.json()["updated_at"]
  assert await _stored_status(blocks, sid) == _without_budget(stored)
  events = _status_events(blocks, sid)
  assert [ev["web_status_changed"] for ev in events] == [["goal", "waiting"], ["now"]]
  assert events[-1]["content"] == "Status updated: Now"
  assert events[-1]["web_status"] == _without_budget(stored)


@pytest.mark.asyncio
@pytest.mark.parametrize("chars, over_budget", [(200, False), (201, True)])
async def test_a_status_set_reports_the_now_length_and_flags_a_now_over_the_budget(
    tmp_path: pathlib.Path, chars: int, over_budget: bool) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)

  resp = _set(client, sid, json.dumps({"now": "n" * chars}))

  assert resp.status_code == 200
  assert resp.json()["now_chars"] == chars
  assert resp.json()["now_over_budget"] is over_budget
  assert "now_chars" not in _status_events(blocks, sid)[-1]["web_status"]
  assert "now_over_budget" not in await _stored_status(blocks, sid)


@pytest.mark.asyncio
async def test_a_status_set_naming_no_now_on_a_session_without_one_reports_zero_chars(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)

  resp = _set_after_reads(client, sid, _waiting_set("Pick a layout"))

  assert resp.json()["now_chars"] == 0
  assert resp.json()["now_over_budget"] is False


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
    _set_after_reads(client, child.id, text)
  await tree.completion.archive_subtree(
      closed.id,
      request_id="archive-test",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR)
  await blocks.lifecycle.archive_session(archived.id)

  body = client.get(_STATUS_URL, params={"session_id": root_id}).json()

  assert body["web_status"] is None
  assert [row["session_id"] for row in body["descendants"]] == [deep.id, new.id, old.id]
  assert [_without_item_times(row["waiting"]) for row in body["descendants"]] == [
      [
          {
              "text": "Deep decision",
              "context": "The line is active now. Options are continue or stop; continue is recommended.",
              "link": None
          }
      ],
      [
          {
              "text": "New decision",
              "context": "The line is active now. Options are continue or stop; continue is recommended.",
              "link": None
          }
      ],
      [
          {
              "text": "Old decision",
              "context": "The line is active now. Options are continue or stop; continue is recommended.",
              "link": None
          }
      ],
  ]


@pytest.mark.asyncio
async def test_the_status_read_lists_the_open_direct_children_with_a_now_newest_first(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  root_id = await _session(blocks)
  tree = blocks.tree
  old = await conftest.create_task(tree, parent=root_id, request_id="old", name="Old")
  new = await conftest.create_task(tree, parent=root_id, request_id="new", name="New")
  deep = await conftest.create_task(tree, parent=old.id, request_id="deep", name="Deep")
  no_now = await conftest.create_task(tree, parent=root_id, request_id="no-now", name="No Now")
  closed = await conftest.create_task(tree, parent=root_id, request_id="closed", name="Closed")
  archived = await conftest.create_task(tree, parent=root_id, request_id="archived", name="Archived")
  client = _status_client(blocks)
  for child, text in [
      (old, json.dumps({"now": "Old step"})),
      (new, json.dumps({"now": "New step"})),
      (deep, json.dumps({"now": "Deep step"})),
      (no_now, _waiting_set("No Now decision")),
      (closed, json.dumps({"now": "Closed step"})),
      (archived, json.dumps({"now": "Archived step"})),
  ]:
    _set_after_reads(client, child.id, text)
  await tree.completion.archive_subtree(
      closed.id,
      request_id="archive-test",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR)
  await blocks.lifecycle.archive_session(archived.id)

  body = client.get(_STATUS_URL, params={"session_id": root_id}).json()

  assert [(row["session_id"], row["name"], row["now"]) for row in body["children"]] == [
      (new.id, "New", "New step"),
      (old.id, "Old", "Old step"),
  ]
  assert set(body["children"][0]) == {"session_id", "name", "now", "updated_at"}


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


# ---------------------------------------------------------------------------
# Stale plan links: an old version or a closed lineage is refused in set and confirm
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
@pytest.mark.parametrize("endpoint", [_STATUS_URL, "/api/internal/web/status/unread"])
@pytest.mark.parametrize("field", ["text", "context", "link"])
async def test_a_status_set_refuses_an_old_plan_link_before_the_cold_read(
    tmp_path: pathlib.Path, endpoint: str, field: str) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  plan_sid = await _session(blocks)
  client = _status_client(blocks)
  v9 = _session_artifact(blocks, plan_sid, "plan_01_v9.html")
  v11 = _session_artifact(blocks, plan_sid, "plan_01_v11.html")
  _write_plans(
      blocks, plan_sid,
      [_plan(1, [_plan_version(9, "artifacts/plan_01_v9.html"),
                 _plan_version(11, "artifacts/plan_01_v11.html")])])
  item = {
      "text": "Decide plan 1",
      "context": "The plan waits. Options are approve or revise; approve is recommended.",
      "link": None,
  }
  item[field] = _host_file_link(v9)

  with mock.patch.object(status, "_read_item_digests", side_effect=AssertionError("cold read ran")) as digest_reads:
    response = client.post(endpoint, json={"session_id": sid, "text": json.dumps({"waiting": [item]})})

  assert response.status_code == 409
  detail = response.json()["detail"]
  assert _host_file_link(v9) in detail
  assert str(v11) in detail
  digest_reads.assert_not_called()
  assert await _stored_status(blocks, sid) is None
  assert _status_events(blocks, sid) == []
  assert _ask_checks(blocks, sid) == []


@pytest.mark.asyncio
@pytest.mark.parametrize("local_path", [False, True])
async def test_status_confirm_refuses_a_stale_stored_plan_link(tmp_path: pathlib.Path, local_path: bool) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  v9 = _session_artifact(blocks, sid, "plan_01_v9.html")
  v11 = _session_artifact(blocks, sid, "plan_01_v11.html")
  _write_plans(
      blocks, sid,
      [_plan(1, [_plan_version(9, "artifacts/plan_01_v9.html"),
                 _plan_version(11, "artifacts/plan_01_v11.html")])])
  link = str(v9) if local_path else _host_file_link(v9)
  assert blocks.tree is not None
  await blocks.tree.update_slot_fields(
      sid,
      status.OWNER,
      web_status=metadata.WebStatus(
          waiting=[
              metadata.WaitingItem(
                  text="Decide plan 1",
                  context="The plan waits. Options are approve or revise; approve is recommended.",
                  link=link)
          ]))

  response = client.post("/api/internal/web/status/confirm", json={"session_id": sid})

  assert response.status_code == 409
  detail = response.json()["detail"]
  assert link in detail
  assert str(v11) in detail
  assert conftest.events_of_type(_events(blocks, sid), ET.WEB_STATUS_CONFIRMED) == []


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "endpoint", [_STATUS_URL, "/api/internal/web/status/unread", "/api/internal/web/status/confirm"])
async def test_status_refuses_a_link_to_a_closed_lineage(tmp_path: pathlib.Path, endpoint: str) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  page = _session_artifact(blocks, sid, "plan_02_v1.html")
  _write_plans(blocks, sid, [_plan(2, [_plan_version(1, "artifacts/plan_02_v1.html")], closed={"as": "completed"})])
  assert blocks.tree is not None
  await blocks.tree.update_slot_fields(
      sid,
      status.OWNER,
      web_status=metadata.WebStatus(
          waiting=[
              metadata.WaitingItem(
                  text="Decide plan 2",
                  context="The plan is closed. Options are reopen or stop; reopen is recommended.",
                  link=_host_file_link(page))
          ]))

  payload = {"session_id": sid}
  if endpoint != "/api/internal/web/status/confirm":
    payload["text"] = _waiting_with_link("Decide plan 2", _host_file_link(page))
  response = client.post(endpoint, json=payload)

  assert response.status_code == 409
  assert "closed" in response.json()["detail"]
  assert conftest.events_of_type(_events(blocks, sid), ET.WEB_STATUS_CONFIRMED) == []


@pytest.mark.asyncio
async def test_status_confirmation_surfaces_an_unreadable_plan_registry(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  page = _session_artifact(blocks, sid, "plan.html")
  (blocks.cfg.sessions_dir / sid / "plans.json").write_text("invalid JSON", encoding="utf-8")
  assert blocks.tree is not None
  await blocks.tree.update_slot_fields(
      sid,
      status.OWNER,
      web_status=metadata.WebStatus(waiting=[metadata.WaitingItem(text="Decide", link=_host_file_link(page))]))

  with pytest.raises(json.JSONDecodeError):
    client.post("/api/internal/web/status/confirm", json={"session_id": sid})

  assert conftest.events_of_type(_events(blocks, sid), ET.WEB_STATUS_CONFIRMED) == []


@pytest.mark.asyncio
async def test_status_confirm_returns_the_stored_status_with_start_times_and_records_one_event(
    tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  stored_at = models.utc_now() - datetime.timedelta(hours=5)
  assert blocks.tree is not None
  await blocks.tree.update_slot_fields(
      sid,
      status.OWNER,
      web_status=metadata.WebStatus(
          waiting=[
              metadata.WaitingItem(
                  text="Decide the deploy",
                  context="The release is ready. Options are deploy or wait; deploy is recommended.")
          ],
          updated_at=stored_at))

  with mock.patch.object(status, "_read_item_digests", side_effect=AssertionError("cold read ran")) as digest_reads:
    response = client.post("/api/internal/web/status/confirm", json={"session_id": sid})

  assert response.status_code == 200
  waited = response.json()["waiting"][0]
  assert _parse_time(waited["since"]) == stored_at
  assert waited["since_exact"] is False
  digest_reads.assert_not_called()
  assert len(conftest.events_of_type(_events(blocks, sid), ET.WEB_STATUS_CONFIRMED)) == 1


# ---------------------------------------------------------------------------
# since: a stored item keeps its start time across a rewording or a plan version
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_since_keeps_the_start_time_of_the_same_text_and_a_reworded_item(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)

  first = _set_after_reads(client, sid, _waiting_set("Decide A"))
  since = first.json()["waiting"][0]["since"]
  second = _set_after_reads(client, sid, _waiting_set("Decide A"))
  third = _set_after_reads(client, sid, _waiting_set("Decide B"))

  assert first.status_code == second.status_code == third.status_code == 200
  assert second.json()["waiting"][0]["since"] == since
  assert third.json()["waiting"][0]["since"] == since
  assert third.json()["waiting"][0]["since_exact"] is True


@pytest.mark.asyncio
async def test_since_keeps_the_start_time_of_a_reworded_item_with_the_same_link(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  page = tmp_path / "linked-page.html"
  page.write_text("page", encoding="utf-8")
  link = _host_file_link(page)

  first = _set_after_reads(client, sid, _waiting_with_link("Review A", link))
  second = _set_after_reads(client, sid, _waiting_with_link("Review B", link))

  assert first.status_code == second.status_code == 200
  assert second.json()["waiting"][0]["since"] == first.json()["waiting"][0]["since"]


@pytest.mark.asyncio
async def test_since_keeps_the_start_time_when_a_plan_link_moves_to_its_new_version(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  v2 = _session_artifact(blocks, sid, "plan_03_v2.html")
  v3 = _session_artifact(blocks, sid, "plan_03_v3.html")
  _write_plans(blocks, sid, [_plan(3, [_plan_version(2, "artifacts/plan_03_v2.html")])])

  first = _set_after_reads(client, sid, _waiting_with_link("Decide plan A", _host_file_link(v2)))
  since = first.json()["waiting"][0]["since"]
  _write_plans(
      blocks, sid,
      [_plan(3, [_plan_version(2, "artifacts/plan_03_v2.html"),
                 _plan_version(3, "artifacts/plan_03_v3.html")])])
  second = _set_after_reads(client, sid, _waiting_with_link("Decide plan B", _host_file_link(v3)))

  assert first.status_code == second.status_code == 200
  assert second.json()["waiting"][0]["since"] == since


@pytest.mark.asyncio
async def test_legacy_items_read_with_a_lower_bound_and_keep_it_after_a_set(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  stored_at = models.utc_now() - datetime.timedelta(hours=5)
  item = metadata.WaitingItem(
      text="Legacy decision", context="The line is active now. Options are continue or stop; continue is recommended.")
  assert blocks.tree is not None
  await blocks.tree.update_slot_fields(
      sid, status.OWNER, web_status=metadata.WebStatus(waiting=[item], updated_at=stored_at))

  read = client.get(_STATUS_URL, params={"session_id": sid}).json()["web_status"]["waiting"][0]
  assert _parse_time(read["since"]) == stored_at
  assert read["since_exact"] is False

  written = _set_after_reads(client, sid, _waiting_set("Legacy decision"))

  assert written.status_code == 200
  assert _parse_time(written.json()["waiting"][0]["since"]) == stored_at
  assert written.json()["waiting"][0]["since_exact"] is False


@pytest.mark.asyncio
async def test_a_set_file_that_carries_a_start_time_field_is_refused(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  item = {
      "text": "Decide",
      "context": "The line is active. Options are continue or stop; continue is recommended.",
      "since": models.utc_now_iso(),
      "since_exact": True,
  }

  response = _set(client, sid, json.dumps({"waiting": [item]}))

  assert response.status_code == 422
  assert await _stored_status(blocks, sid) is None
  assert _status_events(blocks, sid) == []


# ---------------------------------------------------------------------------
# The cold read: the item digest, the unread query and the ask-check record
# ---------------------------------------------------------------------------

_ASK_CHECK_URL = "/api/internal/web/ask-check"
_UNREAD_URL = "/api/internal/web/status/unread"
_CONTEXTED_DECIDE = json.dumps(
    {
        "waiting":
            [{
                "text": "Decide",
                "context": "The line is active. Choose continue or stop; continue is recommended.",
            }]
    })


def _ask_check(
    client: testclient.TestClient,
    session_id: str,
    item: dict,
    *,
    backend: str = "haiku",
    answer: str = "(1) Decide.\n(2) NOT STATED.") -> httpx.Response:
  return client.post(
      _ASK_CHECK_URL, json={
          "session_id": session_id,
          "item": item,
          "backend": backend,
          "answer": answer
      })


def _unread(client: testclient.TestClient, session_id: str, text: str) -> httpx.Response:
  return client.post(_UNREAD_URL, json={"session_id": session_id, "text": text})


def _ask_checks(blocks: conftest.SessionBlocks, sid: str) -> list[dict]:
  return conftest.events_of_type(_events(blocks, sid), ET.WEB_ASK_CHECK)


def _host_file_link(path: pathlib.Path) -> str:
  return file_urls.FILE_SERVER_MOUNTS[0] + str(path)


def test_file_link_scanner_classifies_retired_and_markdown_local_targets(tmp_path: pathlib.Path) -> None:
  home = tmp_path / "charliebot-home"
  session_dir = tmp_path / "sessions" / "synthetic-session"
  home.mkdir()
  session_dir.mkdir(parents=True)
  user_home = tmp_path / "user-home"
  user_home.mkdir()
  context = file_urls.ScanContext(18498, home, session_dir, user_home)
  session_artifact = session_dir / "artifacts" / "plan.html"
  session_artifact.parent.mkdir()
  session_artifact.write_text("page", encoding="utf-8")
  retired_session_file = home / "sessions" / tmp_path.name / "plan.html"
  retired_session_file.parent.mkdir(parents=True)
  retired_session_file.write_text("page", encoding="utf-8")
  retired_home_file = home / "home" / tmp_path.name / "source.rs"
  retired_home_file.parent.mkdir(parents=True)
  retired_home_file.write_text("source", encoding="utf-8")
  session_url = f"https://charliebot.example:18498/files/sessions/{tmp_path.name}/plan.html"
  home_url = f"https://charliebot.example:18498/files/home/{tmp_path.name}/source.rs"
  text = (
      f"{session_url} {home_url} [plan](artifacts/plan.html) [script](~/scripts/x.sh) "
      "[line](/home/u/x.rs:258)\n[artifact]: artifacts/plan.html")

  findings = file_urls.scan_file_links(text, context)

  assert [finding.form for finding in findings] == [
      file_urls.FileLinkForm.RETIRED_PREFIX,
      file_urls.FileLinkForm.RETIRED_PREFIX,
      file_urls.FileLinkForm.LOCAL_PATH,
      file_urls.FileLinkForm.LOCAL_PATH,
      file_urls.FileLinkForm.LOCAL_PATH,
      file_urls.FileLinkForm.LOCAL_PATH,
  ]
  assert [finding.link for finding in findings] == [
      session_url,
      home_url,
      "artifacts/plan.html",
      "~/scripts/x.sh",
      "/home/u/x.rs:258",
      "artifacts/plan.html",
  ]
  assert all(finding.dead for finding in findings)
  assert findings[0].correct_link == f"https://charliebot.example:18498/absolute_filepath{retired_session_file}"
  assert findings[1].correct_link == f"https://charliebot.example:18498/absolute_filepath{retired_home_file}"
  assert findings[2].correct_link == _host_file_link(session_artifact)
  assert findings[3].correct_link == _host_file_link(user_home / "scripts" / "x.sh")
  assert findings[4].correct_link == "/absolute_filepath/home/u/x.rs:258"
  assert findings[5].correct_link == _host_file_link(session_artifact)


def test_file_link_scanner_checks_local_canonical_paths_and_ignores_non_file_links(tmp_path: pathlib.Path) -> None:
  home = tmp_path / "charliebot-home"
  session_dir = tmp_path / "sessions" / "synthetic-session"
  home.mkdir()
  session_dir.mkdir(parents=True)
  context = file_urls.ScanContext(18498, home, session_dir, tmp_path / "user-home")
  live_file = tmp_path / "live.html"
  live_file.write_text("page", encoding="utf-8")
  live_dir = tmp_path / "directory"
  live_dir.mkdir()
  missing_file = tmp_path / "missing.html"
  missing_url = f"http://localhost:18498/absolute_filepath{missing_file}"
  live_url = f"http://localhost:18498/absolute_filepath{live_file}"
  directory_url = f"http://localhost:18498/absolute_filepath{live_dir}"
  text = (
      f"{live_url} {directory_url} {live_url}. {live_url}, {live_url}。 {missing_url} "
      "[session](/?session=synthetic-id) https://code.example/o/r/pull/1/files "
      "https://other.example/files/x https://charliebot.example:18499/files/x /diff?x "
      "/files/x https://other.example/absolute_filepath/tmp/not-on-this-host")

  findings = file_urls.scan_file_links(text, context)

  assert [finding.link for finding in findings] == [
      live_url, directory_url, live_url, live_url, live_url, missing_url,
      "https://other.example/absolute_filepath/tmp/not-on-this-host"
  ]
  assert [finding.live for finding in findings] == [True, True, True, True, True, False, None]
  assert [finding.dead for finding in findings] == [False, False, False, False, False, True, False]
  assert findings[5].path == missing_file


@pytest.mark.asyncio
async def test_a_status_set_stores_context_after_the_item_was_read(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  with_context = {
      "text": "Pick a layout",
      "context": "The panel is one row per item. Options: fold the line or show it. Fold is recommended.",
      "link": None,
  }
  _ask_check(client, sid, with_context)
  resp = _set(client, sid, json.dumps({"waiting": [with_context]}))

  assert resp.status_code == 200
  stored = (await _stored_status(blocks, sid))["waiting"]
  assert _without_item_times(stored) == [with_context]
  assert _status_events(blocks, sid)[-1]["web_status"]["waiting"] == stored


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "item",
    [
        pytest.param({"text": "Approve the deploy"}, id="missing"),
        pytest.param({
            "text": "Approve the deploy",
            "context": ""
        }, id="empty"),
        pytest.param({
            "text": "Approve the deploy",
            "context": "  \n "
        }, id="blank"),
    ],
)
async def test_a_status_set_refuses_an_item_without_nonblank_context(
    tmp_path: pathlib.Path, item: dict[str, str]) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)

  resp = _set(client, sid, json.dumps({"waiting": [item]}))

  assert resp.status_code == 422
  assert await _stored_status(blocks, sid) is None
  assert _status_events(blocks, sid) == []
  assert _ask_checks(blocks, sid) == []


@pytest.mark.asyncio
async def test_a_status_set_refuses_an_unread_item_without_writing_status_or_event(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  item = {
      "text": "Approve the deploy",
      "context": "The release is ready now. Options are deploy or wait; deploy is recommended.",
      "link": None,
  }

  resp = _set(client, sid, json.dumps({"waiting": [item]}))

  assert resp.status_code == 409
  assert item["text"] in resp.json()["detail"]
  assert await _stored_status(blocks, sid) is None
  assert _status_events(blocks, sid) == []
  assert _ask_checks(blocks, sid) == []


@pytest.mark.asyncio
async def test_a_status_set_stores_an_item_after_ask_check_with_the_same_digest(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  item = {
      "text": "Approve the deploy",
      "context": "The release is ready now. Options are deploy or wait; deploy is recommended.",
      "link": None,
  }
  assert _ask_check(client, sid, item).status_code == 200

  resp = _set(client, sid, json.dumps({"waiting": [item]}))

  assert resp.status_code == 200
  assert _without_item_times(resp.json()["waiting"]) == [item]
  assert await _stored_status(blocks, sid) == _without_budget(resp.json())
  assert len(_status_events(blocks, sid)) == 1


@pytest.mark.asyncio
async def test_a_changed_linked_page_makes_a_status_item_unread_again(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  page = tmp_path / "page.html"
  page.write_text("first page", encoding="utf-8")
  item = {
      "text": "Approve the deploy",
      "context": "The release is ready now. Options are deploy or wait; deploy is recommended.",
      "link": _host_file_link(page),
  }
  assert _ask_check(client, sid, item).status_code == 200
  page.write_text("changed page", encoding="utf-8")

  resp = _set(client, sid, json.dumps({"waiting": [item]}))

  assert resp.status_code == 409
  assert item["text"] in resp.json()["detail"]
  assert await _stored_status(blocks, sid) is None
  assert _status_events(blocks, sid) == []
  assert len(_ask_checks(blocks, sid)) == 1


def test_the_item_digest_names_the_item_and_every_part_and_the_linked_file_change_it(tmp_path: pathlib.Path) -> None:
  page = tmp_path / "page.html"
  page.write_text("first", encoding="utf-8")
  item = metadata.WaitingItem(
      text="Pick a layout", context="Why now: the panel ships today.", link=_host_file_link(page))

  digest = status.item_sha256(item)

  assert digest == status.item_sha256(item.model_copy())
  assert len(digest) == 64
  changed = {
      status.item_sha256(item.model_copy(update={"text": "Pick a colour"})),
      status.item_sha256(item.model_copy(update={"context": "Why now: the panel ships tomorrow."})),
      status.item_sha256(item.model_copy(update={"link": None})),
  }
  page.write_text("second", encoding="utf-8")
  changed.add(status.item_sha256(item))
  assert digest not in changed
  assert len(changed) == 4


def test_the_item_digest_reads_a_bare_absolute_path_and_ignores_a_remote_link(tmp_path: pathlib.Path) -> None:
  page = tmp_path / "page.html"
  page.write_text("first", encoding="utf-8")
  bare = metadata.WaitingItem(text="Pick a layout", link=str(page))
  served = metadata.WaitingItem(text="Pick a layout", link=_host_file_link(page))
  remote = metadata.WaitingItem(text="Pick a layout", link="https://code.example/org/repo/pull/1")
  route = metadata.WaitingItem(text="Pick a layout", link="/?session=abc")

  assert len(status.item_sha256(bare)) == 64
  assert status.item_sha256(bare) != status.item_sha256(served)
  assert len(status.item_sha256(remote)) == 64
  assert len(status.item_sha256(route)) == 64


def test_the_item_digest_does_not_read_a_missing_file_or_directory(tmp_path: pathlib.Path) -> None:
  missing = metadata.WaitingItem(text="Pick a layout", link=_host_file_link(tmp_path / "missing.html"))
  directory = tmp_path / "page-directory"
  directory.mkdir()
  directory_item = metadata.WaitingItem(text="Pick a layout", link=_host_file_link(directory))

  assert len(status.item_sha256(missing)) == 64
  assert len(status.item_sha256(directory_item)) == 64


@pytest.mark.asyncio
async def test_a_waiting_link_to_a_directory_can_be_cold_read_and_stored(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  directory = tmp_path / "linked-directory"
  directory.mkdir()
  item = {
      "text": "Review the directory",
      "context": "The directory lists the generated pages. Options are browse or wait; browse is recommended.",
      "link": _host_file_link(directory),
  }

  assert _ask_check(client, sid, item).status_code == 200
  response = _set(client, sid, json.dumps({"waiting": [item]}))

  assert response.status_code == 200
  assert _without_item_times(response.json()["waiting"]) == [item]
  assert _without_item_times((await _stored_status(blocks, sid))["waiting"]) == [item]


@pytest.mark.asyncio
async def test_ask_check_records_one_event_with_the_digest_and_the_for_you_layer_hides_it(
    tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  item = {"text": "Pick a layout", "context": "The panel folds context.", "link": None}
  answer = "(1) Pick a layout.\n(2) NOT STATED."

  resp = _ask_check(client, sid, item, backend="haiku", answer=answer)

  assert resp.status_code == 200
  assert resp.json() == {"item_sha256": status.item_sha256(metadata.WaitingItem.model_validate(item))}
  checks = _ask_checks(blocks, sid)
  assert len(checks) == 1
  assert {
      key: checks[0][key] for key in ("type", "content", "item_sha256", "backend", "answer")
  } == {
      "type": ET.WEB_ASK_CHECK,
      "content": "Cold read of a Waiting on you item",
      "item_sha256": resp.json()["item_sha256"],
      "backend": "haiku",
      "answer": answer,
  }
  assert reply.for_you_entries(_events(blocks, sid)) == []
  assert ET.WEB_ASK_CHECK in [message["role"] for message in message_utils.events_to_messages(_events(blocks, sid))]


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "case, item, status_code", [
        ("unknown", {
            "text": "Decide"
        }, 404),
        ("thread", {
            "text": "Decide"
        }, 409),
        ("worker", {
            "text": "Decide"
        }, 409),
        ("blank", {
            "text": " "
        }, 422),
        ("extra", {
            "text": "Decide",
            "due": "today"
        }, 422),
    ])
async def test_a_refused_ask_check_records_nothing(
    tmp_path: pathlib.Path, case: str, item: dict, status_code: int) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks, slack_origin=_SLACK_ORIGIN if case == "thread" else None)
  if case == "worker":
    await blocks.store.update_field(sid, "profile", "worker", "test_worker_profile")
  client = _status_client(blocks)

  resp = _ask_check(client, "no-such-session" if case == "unknown" else sid, item)

  assert resp.status_code == status_code
  assert _ask_checks(blocks, sid) == []


@pytest.mark.asyncio
async def test_the_unread_query_lists_an_item_until_ask_check_names_it_and_again_when_the_file_changes(
    tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  page = tmp_path / "page.html"
  page.write_text("first", encoding="utf-8")
  item = {
      "text": "Decide the sample size",
      "context": "The sweep costs $5 an hour. Options: 1k or 10k; 1k is recommended.",
      "link": _host_file_link(page),
  }
  text = json.dumps({"waiting": [item]})

  before = _unread(client, sid, text)
  assert before.status_code == 200 and before.json() == {"unread": [item]}

  digest = status.item_sha256(metadata.WaitingItem.model_validate(item))
  assert _ask_check(client, sid, item).json() == {"item_sha256": digest}
  assert _unread(client, sid, text).json() == {"unread": []}

  page.write_text("second", encoding="utf-8")
  assert _unread(client, sid, text).json() == {"unread": [item]}

  assert await _stored_status(blocks, sid) is None
  assert _status_events(blocks, sid) == []
  assert len(_ask_checks(blocks, sid)) == 1


@pytest.mark.asyncio
async def test_the_unread_query_of_a_file_without_waiting_lists_nothing(tmp_path: pathlib.Path) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)

  resp = _unread(_status_client(blocks), sid, json.dumps({"now": "Running"}))

  assert resp.status_code == 200
  assert resp.json() == {"unread": []}


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "case, text, status_code", [
        ("unknown", _CONTEXTED_DECIDE, 404),
        ("thread", _CONTEXTED_DECIDE, 409),
        ("worker", _CONTEXTED_DECIDE, 409),
        ("bad json", "not json", 422),
        ("names no item", "{}", 422),
        ("invalid item", json.dumps({"waiting": [{
            "text": " "
        }]}), 422),
        ("no context", json.dumps({"waiting": [{
            "text": "Decide"
        }]}), 422),
    ])
async def test_a_refused_unread_query_records_nothing(
    tmp_path: pathlib.Path, case: str, text: str, status_code: int) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks, slack_origin=_SLACK_ORIGIN if case == "thread" else None)
  if case == "worker":
    await blocks.store.update_field(sid, "profile", "worker", "test_worker_profile")
  client = _status_client(blocks)
  before = _events(blocks, sid)

  resp = _unread(client, "no-such-session" if case == "unknown" else sid, text)

  assert resp.status_code == status_code
  assert _events(blocks, sid) == before


@pytest.mark.parametrize("stdin", [False, True])
def test_the_cli_status_set_posts_the_file_and_prints_the_stored_status_as_one_json_line(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], stdin: bool) -> None:
  conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  stored = {"goal": "g", "now": "n", "waiting": [], "updated_at": "2026-10-16T21:00:00+00:00"}
  argv = ["web", "status", "set", "--file", "-"]
  if stdin:
    monkeypatch.setattr("sys.stdin", io.StringIO('{"now": "n"}'))
  else:
    status_file = tmp_path / "status.json"
    status_file.write_text('{"now": "n"}', encoding="utf-8")
    argv = ["web", "status", "set", "--file", str(status_file)]

  calls: list[tuple[str, dict]] = []

  def fake_post(endpoint: str, payload: dict) -> dict:
    calls.append((endpoint, payload))
    if endpoint.endswith("/status/unread"):
      return {"unread": []}
    return stored

  monkeypatch.setattr(common, "post_internal_api", fake_post)
  monkeypatch.setattr(sys, "argv", argv)

  cli.main()

  assert calls == [
      ("/api/internal/web/status/unread", {
          "session_id": "abc",
          "text": '{"now": "n"}'
      }),
      ("/api/internal/web/status", {
          "session_id": "abc",
          "text": '{"now": "n"}'
      }),
  ]
  out = capsys.readouterr().out
  assert out.count("\n") == 1
  assert json.loads(out) == stored


def test_the_cli_status_set_does_not_start_a_cold_read_after_a_link_rejection(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  status_file = tmp_path / "status.json"
  status_file.write_text(
      json.dumps(
          {
              "waiting":
                  [
                      {
                          "text": "Review the page",
                          "context": "The page is missing. Options are retry or stop; retry is recommended.",
                          "link": "http://localhost:9443/absolute_filepath/tmp/missing-status-page.html",
                      }
                  ]
          }),
      encoding="utf-8")
  detail = (
      "Dead file links; nothing was recorded.\n- http://localhost:9443/absolute_filepath/tmp/missing-status-page.html: "
      "no file exists at /tmp/missing-status-page.html.")
  response = conftest.make_json_response({"detail": detail}, 422)

  with (
      conftest.patched_cli_post(cfg, ["web", "status", "set", "--file", str(status_file)], return_value=response) as
      post,
      mock.patch(conftest.CLI_COMMON_MAYBE_VERSION_SKEW_HINT_PATCH_TARGET, return_value=None),
      mock.patch("src.runtime.cold_read.load_ask_texts") as load_ask_texts,
      mock.patch("src.runtime.cold_read.run_light_probe") as run_light_probe,
      pytest.raises(SystemExit) as exit_info,
  ):
    cli.main()

  assert exit_info.value.code == 1
  assert post.call_count == 1
  assert post.call_args.args[0].endswith("/api/internal/web/status/unread")
  load_ask_texts.assert_not_called()
  run_light_probe.assert_not_called()
  assert json.loads(capsys.readouterr().err)["error"] == detail


@pytest.mark.asyncio
async def test_the_cli_status_set_stops_on_missing_context_before_calling_a_light_backend(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  blocks = _rig(tmp_path)
  sid = await _session(blocks)
  client = _status_client(blocks)
  cli_cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, sid)
  option = types.SimpleNamespace(id="fake-light")
  cli_cfg.backends = types.SimpleNamespace(pool=[option.id])
  cli_cfg.charlie_bot_repo = conftest.ROOT
  monkeypatch.setattr(common, "get_config", lambda: cli_cfg)
  item = {"text": "Decide", "link": None}
  status_file = tmp_path / "status.json"
  status_file.write_text(json.dumps({"waiting": [item]}), encoding="utf-8")
  monkeypatch.setattr(sys, "argv", ["web", "status", "set", "--file", str(status_file)])
  api_calls: list[str] = []

  def fake_transport_post(
      url: str, *, json: dict | None, params: dict | None, headers: dict[str, str], timeout: float) -> httpx.Response:
    del params, headers, timeout
    endpoint = url[url.index("/api/internal"):]
    api_calls.append(endpoint)
    assert json is not None
    return client.post(endpoint, json=json)

  monkeypatch.setattr(common, "_request_post", fake_transport_post)
  monkeypatch.setattr(common, "_maybe_version_skew_hint", lambda _base_url: None)
  backend_builds: list[str] = []
  backend_calls: list[str] = []

  class FakeBackend:

    async def one_shot_text(self, prompt: str, system_prompt: str, *, timeout: None) -> str:
      backend_calls.append(prompt)
      return "(1) Decide."

  def build_backend(_option: object, _cfg: object) -> FakeBackend:
    backend_builds.append(option.id)
    return FakeBackend()

  monkeypatch.setattr(
      backend_pool, "select", lambda _cfg, *, caller, exclude: option if option.id not in exclude else None)
  monkeypatch.setattr(backend_types, "build_backend", build_backend)

  with pytest.raises(SystemExit) as exit_info:
    await asyncio.to_thread(cli.main)

  assert exit_info.value.code == 1
  assert api_calls == [_UNREAD_URL]
  assert backend_builds == []
  assert backend_calls == []
  assert json.loads(capsys.readouterr().err) == {
      "error": "Waiting item needs context: Decide",
      "code": "server_error",
      "effect": "none",
  }


def test_the_cli_status_set_reads_and_records_each_unread_item_then_exits_three_before_storing(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  page = tmp_path / "linked-page.html"
  page.write_text("HOST-SERVED-PAGE-CONTENT", encoding="utf-8")
  file_link = _host_file_link(page)
  remote_link = "https://code.example/example/project/pull/12"
  items = [
      {
          "text": "Should we use the file page?",
          "context": "The line is ready now. Options are keep or change; keep is recommended.",
          "link": file_link,
      },
      {
          "text": "Should we use the remote page?",
          "context": "The review is open now. Options are merge or wait; wait is recommended.",
          "link": remote_link,
      },
  ]
  status_file = tmp_path / "status.json"
  status_file.write_text(json.dumps({"waiting": items}), encoding="utf-8")
  monkeypatch.setattr(sys, "argv", ["web", "status", "set", "--file", str(status_file)])
  options = [types.SimpleNamespace(id="fake-a"), types.SimpleNamespace(id="fake-b")]
  cfg = types.SimpleNamespace(
      backends=types.SimpleNamespace(pool=[option.id for option in options]), charlie_bot_repo=conftest.ROOT)
  monkeypatch.setattr(common, "get_config", lambda: cfg)
  monkeypatch.setattr(
      backend_pool, "select", lambda _cfg, *, caller, exclude: next(
          (option for option in options if option.id not in exclude), None))
  answers = "(1) First answer.\n(2) Second answer.\n(3) Third answer.\n(4) Fourth answer.\n(5) Fifth answer."
  probes: list[tuple[str, str, str | None]] = []

  class FakeBackend:

    def __init__(self, backend_id: str) -> None:
      self.backend_id = backend_id

    async def one_shot_text(self, prompt: str, system_prompt: str, *, timeout: None) -> str:
      probes.append((self.backend_id, prompt, system_prompt))
      return answers

  monkeypatch.setattr(backend_types, "build_backend", lambda option, _cfg: FakeBackend(option.id))
  recorded: list[dict] = []
  status_calls: list[dict] = []

  def fake_post(endpoint: str, payload: dict) -> dict:
    if endpoint.endswith("/status/unread"):
      read_items = {json.dumps(call["item"], sort_keys=True) for call in recorded}
      waiting = json.loads(payload["text"]).get("waiting", [])
      return {"unread": [item for item in waiting if json.dumps(item, sort_keys=True) not in read_items]}
    if endpoint.endswith("/ask-check"):
      recorded.append(payload)
      return {"item_sha256": "digest"}
    if endpoint.endswith("/web/status"):
      status_calls.append(payload)
      return {"goal": None, "now": None, "waiting": items, "updated_at": "now"}
    raise AssertionError(f"unexpected endpoint: {endpoint}")

  monkeypatch.setattr(common, "post_internal_api", fake_post)

  with pytest.raises(SystemExit) as first_exit:
    cli.main()

  assert first_exit.value.code == 3
  assert len(probes) == len(items) == len(recorded)
  assert [call["item"] for call in recorded] == items
  assert [call["backend"] for call in recorded] == ["fake-a", "fake-a"]
  assert [call["answer"] for call in recorded] == [answers, answers]
  assert status_calls == []
  out = capsys.readouterr().out
  assert out.count(answers) == 2
  assert out.count("Backend: fake-a") == 2
  assert out.endswith("Nothing was stored.\n")
  assert "HOST-SERVED-PAGE-CONTENT" in probes[0][1]
  assert probes[0][1].index(file_link) < probes[0][1].index("HOST-SERVED-PAGE-CONTENT")
  assert "HOST-SERVED-PAGE-CONTENT" not in probes[1][1]
  assert remote_link in probes[1][1]

  cli.main()

  assert status_calls == [{"session_id": "abc", "text": json.dumps({"waiting": items})}]
  assert len(recorded) == len(items)
  assert len(probes) == len(items)
  assert json.loads(capsys.readouterr().out)["waiting"] == items


def test_the_cli_status_set_prints_each_backend_error_and_records_no_failed_item(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  item = {
      "text": "Approve the deploy",
      "context": "The release is ready now. Options are deploy or wait; deploy is recommended.",
      "link": None,
  }
  status_file = tmp_path / "status.json"
  status_file.write_text(json.dumps({"waiting": [item]}), encoding="utf-8")
  monkeypatch.setattr(sys, "argv", ["web", "status", "set", "--file", str(status_file)])
  options = [types.SimpleNamespace(id="fake-a"), types.SimpleNamespace(id="fake-b")]
  cfg = types.SimpleNamespace(
      backends=types.SimpleNamespace(pool=[option.id for option in options]), charlie_bot_repo=conftest.ROOT)
  monkeypatch.setattr(common, "get_config", lambda: cfg)
  monkeypatch.setattr(
      backend_pool, "select", lambda _cfg, *, caller, exclude: next(
          (option for option in options if option.id not in exclude), None))

  class FailingBackend:

    def __init__(self, backend_id: str) -> None:
      self.backend_id = backend_id

    async def one_shot_text(self, prompt: str, system_prompt: str, *, timeout: None) -> str:
      raise RuntimeError(f"{self.backend_id} failed")

  monkeypatch.setattr(backend_types, "build_backend", lambda option, _cfg: FailingBackend(option.id))
  posts: list[str] = []

  def fake_post(endpoint: str, _payload: dict) -> dict:
    posts.append(endpoint)
    if endpoint.endswith("/status/unread"):
      return {"unread": [item]}
    raise AssertionError(f"unexpected endpoint: {endpoint}")

  monkeypatch.setattr(common, "post_internal_api", fake_post)

  with pytest.raises(SystemExit) as exit_info:
    cli.main()

  assert exit_info.value.code == 1
  assert posts == ["/api/internal/web/status/unread"]
  output = capsys.readouterr()
  assert "attempt fake-a failed: RuntimeError: fake-a failed" in output.err
  assert "attempt fake-b failed: RuntimeError: fake-b failed" in output.err
  assert output.out == ""


def test_the_cli_status_confirm_posts_the_session_and_prints_the_stored_status(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  stored = {
      "goal": "g",
      "now": "n",
      "waiting":
          [
              {
                  "text": "Decide",
                  "context": "The line is active.",
                  "link": None,
                  "since": "2026-10-24T00:00:00+00:00",
                  "since_exact": False,
              }
          ],
      "updated_at": "2026-10-24T05:00:00+00:00",
  }
  calls: list[tuple[str, dict]] = []

  def fake_post(endpoint: str, payload: dict) -> dict:
    calls.append((endpoint, payload))
    return stored

  monkeypatch.setattr(common, "post_internal_api", fake_post)
  monkeypatch.setattr(sys, "argv", ["web", "status", "confirm"])

  cli.main()

  assert calls == [("/api/internal/web/status/confirm", {"session_id": "abc"})]
  assert json.loads(capsys.readouterr().out) == stored


@pytest.mark.parametrize("has_status", [False, True])
def test_the_cli_status_show_prints_the_status_and_one_block_per_child_with_waiting_items(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str],
    has_status: bool) -> None:
  conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  web_status = {
      "goal": "Ship",
      "now": "Testing",
      "waiting":
          [
              {
                  "text": "Pick a layout",
                  "context": "The line is ready. Options are fold or show; fold is recommended.",
                  "link": None,
                  "since": "2026-10-16T18:00:00Z",
                  "since_exact": True,
              }
          ],
      "updated_at": "2026-10-16T21:00:00+00:00",
  } if has_status else None
  child = {
      "session_id":
          "d7645e8f-0000-4000-8000-000000000000",
      "name":
          "Panel child",
      "updated_at":
          "2026-10-16T21:05:00+00:00",
      "waiting":
          [
              {
                  "text": "Answer plan 5",
                  "context": "The plan needs review. Options are approve or revise; revise is recommended.",
                  "link": "/x",
                  "since": "2026-10-16T18:00:00Z",
                  "since_exact": False,
              }
          ],
  }
  calls: list[tuple[str, dict]] = []

  def fake_get(endpoint: str, params: dict | None = None) -> dict:
    calls.append((endpoint, params or {}))
    return {"session_id": "abc", "web_status": web_status, "descendants": [child], "children": []}

  monkeypatch.setattr(common, "get_api", fake_get)
  monkeypatch.setattr(sys, "argv", ["web", "status", "show"])

  cli.main()

  assert calls == [("/api/internal/web/status", {"session_id": "abc"})]
  out = capsys.readouterr().out
  assert ("No status yet" in out) is (not has_status)
  assert "Panel child (d7645e8f)" in out
  assert out.index("Panel child (d7645e8f)") < out.index("Answer plan 5 (/x)")
  if has_status:
    assert "  context: The line is ready. Options are fold or show; fold is recommended." in out
    assert "  since_exact: true" in out
  assert "  since: 2026-10-16T18:00:00Z" in out
  assert "  since_exact: false" in out
  assert "  context: The plan needs review. Options are approve or revise; revise is recommended." in out


@pytest.mark.parametrize("has_children", [False, True])
def test_the_cli_status_show_prints_the_now_of_each_open_child_in_one_block_and_no_block_without_children(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str],
    has_children: bool) -> None:
  conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  children = [
      {
          "session_id": "d7645e8f-0000-4000-8000-000000000000",
          "name": "Now child",
          "now": "Running the sweep",
          "updated_at": "2026-10-16T21:05:00+00:00",
      }
  ] if has_children else []
  monkeypatch.setattr(
      common,
      "get_api",
      lambda endpoint, params=None: {
          "session_id": "abc",
          "web_status": None,
          "descendants": [],
          "children": children
      })
  monkeypatch.setattr(sys, "argv", ["web", "status", "show"])

  cli.main()

  out = capsys.readouterr().out
  if has_children:
    assert out == (
        "No status yet\n\nChild sessions' Now:\n"
        "Now child (d7645e8f): Running the sweep (updated 2026-10-16T21:05:00+00:00)\n")
  else:
    assert out == "No status yet\n"


def test_the_reply_rules_add_the_now_sentences_and_drop_the_progress_sentence() -> None:
  rules = (conftest.ROOT / "prompts" / reply.RULES_FILE).read_text(encoding="utf-8")
  added = (
      "A child session keeps its own progress in its own **Now**.",
      "When a child session reports progress that changes this session's next milestone, set the new **Now**.",
      "Write **Now** in one or two sentences.",
      "Keep **Now** under 200 characters.",
      "Each set replaces the whole **Now** text. Write only the open state of the next milestone into it.",
      "The status panel shows the **Now** of each open child session in a folded block below your items.",
      "The printout also holds `now_chars` and `now_over_budget`.",
      "When `now_over_budget` is true, shorten **Now** and set it again.",
      "The command also prints the **Now** of each open child session.",
  )
  for sentence in added:
    assert sentence in rules
  assert "put the progress in **Now**" not in rules
