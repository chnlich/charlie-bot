"""Acceptance tests for the Slack summon listener (src.features.slack.slack_listener)."""

from __future__ import annotations

import asyncio
import contextlib
import time
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
    ROOT,
    FakeSlackClient,
    WsServerNeverAnswersClose,
    bind_deps_managers,
    build_session_manager,
    build_slack_cfg,
    create_root_session,
    mention_seam,
)

from src.features.slack.slack_listener import (
    _REPLY_COMMAND,
    _build_follow_wake_message,
    handle_app_mention,
    summon_session_id,
)
from src.infra import event_types as ET
from src.infra import metadata_slots
from src.infra.config import CharlieBotConfig
from src.infra.models import CreateSessionRequest
from src.runtime.sessions import SessionManager
from src.runtime.task_sessions import TaskTreeManager

_TS = "1700000000.000100"

# The approved scope, red-line, and reply-format texts, read from the same
# prompts docs the builder reads and stripped exactly like the builder, so the
# tail assertions pin exact bytes.
_SCOPE_PATH = ROOT / "prompts" / "slack_reply_scope.md"
_SLACK_SCOPE = _SCOPE_PATH.read_text(encoding="utf-8").strip()
_RED_LINE_PATH = ROOT / "prompts" / "thread_reply_redline.md"
_RED_LINE = _RED_LINE_PATH.read_text(encoding="utf-8").strip()
_FORMAT_PATH = ROOT / "prompts" / "thread_reply_format.md"
_REPLY_FORMAT = _FORMAT_PATH.read_text(encoding="utf-8").strip()


def _spawn_round_tasks() -> list[asyncio.Task]:
  """Reset the task sink used by the patched create_logged_task."""
  _spawn_round_tasks.tasks = []
  return _spawn_round_tasks.tasks


_spawn_round_tasks.tasks: list[asyncio.Task] = []


def _make_event(**overrides: object) -> dict:
  """Build an allowed app_mention event, merging in per-test overrides."""
  base: dict = {
      "type": "app_mention",
      "user": "U_ALLOWED",
      "team": "T_TEST",
      "channel": "C_TEST",
      "ts": _TS,
      "text": "hey",
      "channel_type": "channel",
  }
  base.update(overrides)
  return base


def _thread_ts(event: dict) -> str:
  return event.get("thread_ts") or event["ts"]


def _sid(event: dict) -> str:
  return summon_session_id(event["team"], event["channel"], _thread_ts(event))


def _rig(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> tuple[CharlieBotConfig, SessionManager, FakeSlackClient]:
  """Summon rig: cfg and session manager rooted at tmp_path, plus the recording fake client.

  The summon creates its manager root through the deps task-tree singleton, so the rig binds a tree over the
  same session manager.
  """
  cfg = build_slack_cfg(tmp_path)
  session_mgr = build_session_manager(cfg)
  bind_deps_managers(monkeypatch, TaskTreeManager(cfg, session_mgr), session_mgr)
  return cfg, session_mgr, FakeSlackClient()


@pytest.mark.asyncio
async def test_allowed_user_creates_session_and_persists_agent_message(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, client = _rig(tmp_path, monkeypatch)
  event = _make_event()
  tasks = _spawn_round_tasks()

  with mention_seam(tasks) as trigger:
    sid = await handle_app_mention(event, cfg, session_mgr, client)
    await asyncio.gather(*tasks)

  assert sid == _sid(event)

  meta = await session_mgr.store.get_session(sid)
  assert meta is not None
  origin = metadata_slots.fields_of(meta, "slack").slack_origin
  assert origin is not None
  assert origin.team_id == "T_TEST"
  assert origin.channel_id == "C_TEST"
  assert origin.thread_ts == _TS

  # The Slack traffic is exactly one eyes reaction on the mention, one
  # permalink lookup for the mention's own ts, plus one channel-name lookup
  # for the auto-grouping — no thread-content read of any kind, and no posted
  # acceptance message. The completeness assertion below fails on any other
  # client call, so a summon path that started reading thread content or
  # removing reactions fails here.
  posts = [c for name, c in client.calls if name == "post_message"]
  assert not posts
  reactions = [c for name, c in client.calls if name == "add_reaction"]
  assert reactions == [{"channel": "C_TEST", "name": "eyes", "ts": _TS}]
  permalinks = [c for name, c in client.calls if name == "get_permalink"]
  assert permalinks == [{"channel": "C_TEST", "ts": _TS}]
  name_lookups = [c for name, c in client.calls if name == "get_channel_name"]
  assert name_lookups == [{"channel": "C_TEST"}]
  assert len(client.calls) == len(reactions) + len(permalinks) + len(name_lookups)

  # The persisted content carries exactly the URL the client produced for this
  # mention, and nothing else Slack-derived beyond the citation boundary.
  expected_url = f"https://fake.slack.test/archives/C_TEST/p{_TS}"

  events = session_mgr.events.load_chat_events_sync(sid)
  agent_messages = [ev for ev in events if ev.get("type") == ET.AGENT_MESSAGE]
  assert len(agent_messages) == 1
  assert agent_messages[0]["slack"] == {
      "channel_id": "C_TEST",
      "thread_ts": _TS,
      "mention_ts": _TS,
  }
  assert expected_url in agent_messages[0]["content"]
  assert agent_messages[0]["content"].endswith(f"{_SLACK_SCOPE}\n{_RED_LINE}\n{_REPLY_FORMAT}")

  trigger.assert_awaited_once()
  assert trigger.await_args.kwargs["input_id"] == agent_messages[0]["id"]
  assert expected_url in trigger.await_args.args[1]


@pytest.mark.asyncio
async def test_summon_prompt_carries_the_platform_line(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, client = _rig(tmp_path, monkeypatch)
  event = _make_event()
  tasks = _spawn_round_tasks()

  with mention_seam(tasks):
    await handle_app_mention(event, cfg, session_mgr, client)
    await asyncio.gather(*tasks)

  events = session_mgr.events.load_chat_events_sync(_sid(event))
  agent_messages = [ev for ev in events if ev.get("type") == ET.AGENT_MESSAGE]
  content = agent_messages[0]["content"]
  # The platform line sits between the fetch hint and the citation boundary
  # and carries the facts the shared reply-format contract defers to it: the
  # reply command the round-end audit gate reads off the summon, and the
  # per-message limit.
  expected_platform_line = (
      f"Platform: Slack. Reply command: `{_REPLY_COMMAND} --file <path>`. "
      "Per-message limit: 40000 characters. "
      "Linked pages: publish each page with `charliebot publish <path>` and write the URL it prints "
      "into the reply; a CharlieBot file-server link refuses the reply.")
  assert f"\n{expected_platform_line}\n" in content


@pytest.mark.asyncio
async def test_summon_prompt_keeps_the_slack_scope_sentences_verbatim(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Slack's citation-boundary and summoner-PII sentences survive verbatim in the scope-doc slot."""
  cfg, session_mgr, client = _rig(tmp_path, monkeypatch)
  event = _make_event()
  tasks = _spawn_round_tasks()

  with mention_seam(tasks):
    await handle_app_mention(event, cfg, session_mgr, client)
    await asyncio.gather(*tasks)

  events = session_mgr.events.load_chat_events_sync(_sid(event))
  content = [ev for ev in events if ev.get("type") == ET.AGENT_MESSAGE][0]["content"]
  assert ("引用边界：只引用这条频道／线程本身、公开仓库、公开频道；"
          "现场只读命令取得的运行状态可引用并附取数命令；已成文的私有内容不引用。") in content
  assert "Keep the summoner's PII out of everything this session posts to the thread." in content


def test_follow_wake_message_names_thread_reply_docs_and_reply_command() -> None:
  msg = _build_follow_wake_message(_TS, "https://fake.slack.test/archives/C_TEST/p1700000000000100")
  assert msg.startswith("slack-thread-follow floor=1700000000.000100\n")
  assert "prompts/slack_reply_scope.md" in msg
  assert "prompts/thread_reply_redline.md" in msg
  assert "prompts/thread_reply_format.md" in msg
  assert msg.endswith(f"只在值得时用 `{_REPLY_COMMAND} --file <path>` 回复。")


@pytest.mark.asyncio
async def test_same_thread_twice_reuses_the_session(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_mgr, client = _rig(tmp_path, monkeypatch)
  event = _make_event()

  with mention_seam():
    first = await handle_app_mention(event, cfg, session_mgr, client)
    second = await handle_app_mention(event, cfg, session_mgr, client)

  assert first == second
  sessions = await session_mgr.listing.list_sessions()
  assert len(sessions) == 1
  assert sessions[0].id == first


_DROP_ROWS = [
    pytest.param({"user": "U_OTHER"}, id="disallowed-user"),
    pytest.param({"type": "message"}, id="non-app-mention"),
]


@pytest.mark.asyncio
@pytest.mark.parametrize(("event_overrides",), _DROP_ROWS)
async def test_unhandled_event_drops_with_no_side_effects(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, event_overrides: dict) -> None:
  cfg, session_mgr, client = _rig(tmp_path, monkeypatch)
  event = _make_event(**event_overrides)

  with mention_seam():
    result = await handle_app_mention(event, cfg, session_mgr, client)

  assert result is None
  assert not client.calls
  assert await session_mgr.store.get_session(_sid(event)) is None


@pytest.mark.asyncio
async def test_trigger_master_forwards_input_id_to_the_task_wake(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.runtime import master_trigger

  cfg, session_mgr, _ = _rig(tmp_path, monkeypatch)
  meta = await create_root_session(session_mgr, CreateSessionRequest(name="t"))

  with patch.object(master_trigger, "_wake_task_node", new=AsyncMock()) as wake_mock:
    await master_trigger.trigger_master(meta.id, "s", session_mgr, event_type=ET.AGENT_MESSAGE, input_id="evt-1")
    await master_trigger.trigger_master(meta.id, "s", session_mgr, event_type=ET.CHILD_REPORT)

  assert wake_mock.await_count == 2
  assert wake_mock.await_args_list[0].kwargs["input_id"] == "evt-1"
  assert wake_mock.await_args_list[1].kwargs["input_id"] is None


# --- Socket Mode close wait ---------------------------------------------------
#
# Slack's Socket Mode endpoint never answers a client close frame, so the
# close handshake `websockets.connect(...)` exits through can never complete;
# the listener's session exit aborts the transport instead of waiting out
# close_timeout. Both tests here drive the real run_listener against the
# conftest stand-in (WsServerNeverAnswersClose) with the
# SlackClient.open_connection seam pointed at it and the thread backfill
# stubbed.

_WS_HELLO = [{"type": "hello"}]


async def _run_listener_against(
    stand_in: WsServerNeverAnswersClose, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> asyncio.Task:
  """Start the stand-in and run the real run_listener with its Slack Web API side
  pointed at it; the caller stops the stand-in when the listener task is done."""
  from src.features.slack import slack_listener

  cfg, session_mgr, _ = _rig(tmp_path, monkeypatch)
  url = await stand_in.start()

  async def open_stand_in(self: object) -> str:
    return url

  async def no_backfill(*args: object, **kwargs: object) -> int:
    return 0

  monkeypatch.setattr(slack_listener.SlackClient, "open_connection", open_stand_in)
  monkeypatch.setattr(slack_listener, "_backfill_followed_threads", no_backfill)
  # The listener never issues an HTTP request once open_connection is the seam
  # under stub; keep it off the shared httpx client other tests may have faked.
  monkeypatch.setattr(slack_listener, "get_http_client", lambda: object())
  return asyncio.create_task(slack_listener.run_listener(cfg, session_mgr), name="slack-listener-under-test")


async def _await_listener_cancel(task: asyncio.Task) -> float:
  """Cancel the listener and return the seconds its cancellation took."""
  started = time.perf_counter()
  task.cancel()
  with contextlib.suppress(asyncio.CancelledError):
    await task
  return time.perf_counter() - started


@pytest.mark.asyncio
async def test_listener_cancel_skips_the_close_wait(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Cancelling the listener aborts the transport instead of waiting out
  WS_CLIENT_CLOSE_TIMEOUT for the close frame the stand-in never answers."""
  from src.infra import timeouts

  monkeypatch.setattr(timeouts, "WS_CLIENT_CLOSE_TIMEOUT", 0.2)
  stand_in = WsServerNeverAnswersClose(_WS_HELLO)
  try:
    task = await _run_listener_against(stand_in, tmp_path, monkeypatch)
    async with asyncio.timeout(5):
      while not stand_in.upgrade_times:
        await asyncio.sleep(0.02)
    await asyncio.sleep(0.05)  # hello round trip: the listener parks in its receive await
    elapsed = await _await_listener_cancel(task)
    assert elapsed < timeouts.WS_CLIENT_CLOSE_TIMEOUT, (
        f"listener cancel took {elapsed:.3f}s; the session exit paid the "
        f"WS_CLIENT_CLOSE_TIMEOUT={timeouts.WS_CLIENT_CLOSE_TIMEOUT} close wait")
  finally:
    await stand_in.stop()


@pytest.mark.asyncio
@pytest.mark.integration
async def test_listener_reconnects_within_three_seconds_after_disconnect(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A Socket Mode refresh (the server-sent disconnect) has the listener's next
  connection on the wire within 3 s: the session exit aborts the transport, so
  the bound is the reconnect backoff alone."""
  stand_in = WsServerNeverAnswersClose([*_WS_HELLO, {"type": "disconnect"}])
  try:
    task = await _run_listener_against(stand_in, tmp_path, monkeypatch)
    async with asyncio.timeout(9):
      while len(stand_in.upgrade_times) < 2:
        await asyncio.sleep(0.02)
    gap = stand_in.upgrade_times[1] - stand_in.upgrade_times[0]
    assert gap < 3.0, f"reconnect after disconnect took {gap:.2f}s"
  finally:
    task.cancel()
    with contextlib.suppress(asyncio.CancelledError):
      await task
    await stand_in.stop()
