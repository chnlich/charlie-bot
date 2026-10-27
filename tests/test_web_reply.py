"""Acceptance tests for the web reply channel (src.features.web_reply): the reply record and its refusals, the language
check, the answers binding, the round-end audit, the For-you layer, the rules segment, the live broadcast of a reply,
and the session status CLI: set, confirm and show."""

from __future__ import annotations

import asyncio
import contextlib
import io
import json
import pathlib
import sys
import types
from collections.abc import Iterator
from unittest import mock

import conftest
import pytest

from src.features.chat_threads import thread_sessions
from src.features.discord import metadata as discord_metadata
from src.features.slack import metadata as slack_metadata
from src.features.web_reply import audit, cli, reply
from src.features.web_reply import turn_contribution as web_turn_contribution
from src.infra import event_types as ET
from src.infra import models
from src.runtime import backend_pool, file_urls, master_cc_state, task_prompts
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
