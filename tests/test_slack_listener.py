"""Acceptance tests for the Slack summon listener (src.features.slack.slack_listener)."""

from __future__ import annotations

import asyncio
import contextlib
import pathlib
import time
from unittest import mock

import conftest
import httpx
import pytest

from src.features.slack import metadata, slack_listener
from src.infra import config, metadata_slots, models
from src.infra import event_types as ET

_TS = "1700000000.000100"


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
  return slack_listener.summon_session_id(event["team"], event["channel"], _thread_ts(event))


def _rig(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> tuple[config.CharlieBotConfig, conftest.SessionBlocks, conftest.FakeSlackClient]:
  """Summon rig: cfg and session blocks rooted at tmp_path, plus the recording fake client.

  The summon creates its manager root through the deps task-tree singleton, so the rig binds a tree over the
  same session blocks.
  """
  cfg = conftest.build_slack_cfg(tmp_path)
  session_blocks = conftest.build_session_blocks(cfg)
  conftest.bind_deps_blocks(monkeypatch, conftest.build_task_tree(cfg, session_blocks), session_blocks)
  return cfg, session_blocks, conftest.FakeSlackClient()


@pytest.mark.asyncio
async def test_allowed_user_creates_session_and_persists_agent_message(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, client = _rig(tmp_path, monkeypatch)
  event = _make_event()
  tasks = _spawn_round_tasks()

  with conftest.mention_seam(tasks) as trigger:
    sid = await slack_listener.handle_app_mention(event, cfg, *conftest.thread_blocks(session_blocks), client)
    await asyncio.gather(*tasks)

  assert sid == _sid(event)

  meta = await session_blocks.store.get_session(sid)
  assert meta is not None
  origin = metadata_slots.fields_of(meta, "slack").slack_origin
  assert origin is not None
  assert origin.team_id == "T_TEST"
  assert origin.channel_id == "C_TEST"
  assert origin.thread_ts == _TS

  # The Slack traffic is exactly one eyes reaction on the mention plus one
  # channel-name lookup for the auto-grouping — no permalink lookup (the
  # summon input carries no link), no thread-content read of any kind, and no
  # posted acceptance message. The completeness assertion below fails on any
  # other client call, so a summon path that started reading thread content or
  # removing reactions fails here.
  posts = [c for name, c in client.calls if name == "post_message"]
  assert not posts
  reactions = [c for name, c in client.calls if name == "add_reaction"]
  assert reactions == [{"channel": "C_TEST", "name": "eyes", "ts": _TS}]
  name_lookups = [c for name, c in client.calls if name == "get_channel_name"]
  assert name_lookups == [{"channel": "C_TEST"}]
  assert len(client.calls) == len(reactions) + len(name_lookups)

  events = session_blocks.events.load_chat_events_sync(sid)
  agent_messages = [ev for ev in events if ev.get("type") == ET.AGENT_MESSAGE]
  assert len(agent_messages) == 1
  assert agent_messages[0]["slack"] == {
      "channel_id": "C_TEST",
      "thread_ts": _TS,
      "mention_ts": _TS,
  }
  # The summon input is the one fixed line: the rules ride the session's
  # managed instructions (the thread reply contract), not the input.
  assert agent_messages[0]["content"] == slack_listener.SLACK.summon_line
  assert agent_messages[0]["content"] == "Slack 线程召唤"

  trigger.assert_awaited_once()
  assert trigger.await_args.kwargs["input_id"] == agent_messages[0]["id"]
  assert trigger.await_args.args[1] == slack_listener.SLACK.summon_line


def test_contract_part_carries_the_platform_line_the_address_and_the_rules() -> None:
  """The Slack contract part states the platform line, this thread's address, the
  summon rule, and the wake's read and ack rules."""
  part = slack_listener.thread_contract_part(metadata.SlackOrigin(team_id="T_TEST", channel_id="C_TEST", thread_ts=_TS))
  # The platform line carries the facts the shared reply-format contract defers
  # to it: the reply command and the per-message limit.
  expected_platform_line = (
      f"Platform: Slack. Reply command: `{slack_listener._REPLY_COMMAND} --file <path>`. "
      "Per-message limit: 40000 characters. "
      "Linked pages: publish each page with `charliebot publish <path>` and write the URL it prints "
      "into the reply; a CharlieBot file-server link refuses the reply.")
  assert expected_platform_line in part
  # The thread address replaces the old summon link: the skill reads this thread
  # by its ids.
  assert f"channel `C_TEST`、thread_ts `{_TS}`" in part
  assert "conversations.replies" in part
  # The summon is the newest bot mention in the thread.
  assert "本次召唤是线程里最新一条 @ 机器人的消息" in part
  # The wake reads from the floor and acks what it read, silence included.
  assert "slack-thread-follow floor=<ts>" in part
  assert "从 floor 标注的消息读起" in part
  assert "`charliebot slack ack --message-id <ts> [...]` 确认，本轮沉默也要 ack" in part


@pytest.mark.asyncio
async def test_read_eligible_over_the_real_client_keeps_only_allowed_human_messages(tmp_path: pathlib.Path) -> None:
  """The eligible readback through a real SlackClient: the thread is read with conversations.replies
  on its channel and ts, and only the allowed human's message comes back."""
  reply = {"type": "message", "user": "U_ALLOWED", "ts": "1700000000.000200", "text": "hi"}
  bot = {"type": "message", "subtype": "bot_message", "bot_id": "B_TEST", "ts": "1700000000.000300", "text": "bot"}
  requests: list[httpx.Request] = []

  def handler(request: httpx.Request) -> httpx.Response:
    requests.append(request)
    return httpx.Response(200, json={"ok": True, "messages": [reply, bot]})

  async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as http:
    client = slack_listener.SlackClient(http, bot_token="test-bot-token", app_token="test-app-token")
    origin = metadata.SlackOrigin(team_id="T_TEST", channel_id="C_TEST", thread_ts=_TS)
    messages = await slack_listener.SlackThreadAdapter(client).read_eligible(origin, conftest.build_slack_cfg(tmp_path))

  [request] = requests
  assert request.url.path == "/api/conversations.replies"
  assert dict(request.url.params) == {"channel": "C_TEST", "ts": _TS}
  assert [(m.id, m.user, m.text) for m in messages] == [("1700000000.000200", "U_ALLOWED", "hi")]


@pytest.mark.asyncio
async def test_same_thread_twice_reuses_the_session(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, client = _rig(tmp_path, monkeypatch)
  event = _make_event()

  with conftest.mention_seam():
    first = await slack_listener.handle_app_mention(event, cfg, *conftest.thread_blocks(session_blocks), client)
    second = await slack_listener.handle_app_mention(event, cfg, *conftest.thread_blocks(session_blocks), client)

  assert first == second
  sessions = await session_blocks.listing.list_sessions()
  assert len(sessions) == 1
  assert sessions[0].id == first


_DROP_ROWS = [
    pytest.param({"user": "U_OTHER"}, id="disallowed-user"),
    pytest.param({"type": "message"}, id="non-app-mention"),
]


@pytest.mark.asyncio
@pytest.mark.parametrize(("event_overrides",), _DROP_ROWS)
async def test_unhandled_event_drops_with_no_side_effects(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, event_overrides: dict) -> None:
  cfg, session_blocks, client = _rig(tmp_path, monkeypatch)
  event = _make_event(**event_overrides)

  with conftest.mention_seam():
    result = await slack_listener.handle_app_mention(event, cfg, *conftest.thread_blocks(session_blocks), client)

  assert result is None
  assert not client.calls
  assert await session_blocks.store.get_session(_sid(event)) is None


@pytest.mark.asyncio
async def test_trigger_master_forwards_input_id_to_the_task_wake(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.runtime import master_trigger

  _, session_blocks, _ = _rig(tmp_path, monkeypatch)
  meta = await conftest.create_root_session(session_blocks, models.CreateSessionRequest(name="t"))

  with mock.patch.object(master_trigger, "_wake_task_node", new=mock.AsyncMock()) as wake_mock:
    await master_trigger.trigger_master(
        meta.id,
        "s",
        session_blocks.store,
        session_blocks.successor,
        session_blocks.lifecycle,
        event_type=ET.AGENT_MESSAGE,
        input_id="evt-1")
    await master_trigger.trigger_master(
        meta.id,
        "s",
        session_blocks.store,
        session_blocks.successor,
        session_blocks.lifecycle,
        event_type=ET.CHILD_REPORT)

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
    stand_in: conftest.WsServerNeverAnswersClose, tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch) -> asyncio.Task:
  """Start the stand-in and run the real run_listener with its Slack Web API side
  pointed at it; the caller stops the stand-in when the listener task is done."""
  cfg, session_blocks, _ = _rig(tmp_path, monkeypatch)
  url = await stand_in.start()

  async def open_stand_in(self: object) -> str:
    return url

  async def no_backfill(*args: object, **kwargs: object) -> int:
    return 0

  monkeypatch.setattr(slack_listener.SlackClient, "open_connection", open_stand_in)
  monkeypatch.setattr(slack_listener, "_backfill_followed_threads", no_backfill)
  # The listener never issues an HTTP request once open_connection is the seam
  # under stub; keep it off the shared httpx client other tests may have faked.
  monkeypatch.setattr("src.infra.http.get_http_client", lambda: object())
  return asyncio.create_task(
      slack_listener.run_listener(cfg, session_blocks.listing, *conftest.thread_blocks(session_blocks)),
      name="slack-listener-under-test")


async def _await_listener_cancel(task: asyncio.Task) -> float:
  """Cancel the listener and return the seconds its cancellation took."""
  started = time.perf_counter()
  task.cancel()
  with contextlib.suppress(asyncio.CancelledError):
    await task
  return time.perf_counter() - started


@pytest.mark.asyncio
async def test_listener_cancel_skips_the_close_wait(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Cancelling the listener aborts the transport instead of waiting out
  WS_CLIENT_CLOSE_TIMEOUT for the close frame the stand-in never answers."""
  from src.infra import timeouts

  monkeypatch.setattr(timeouts, "WS_CLIENT_CLOSE_TIMEOUT", 0.2)
  stand_in = conftest.WsServerNeverAnswersClose(_WS_HELLO)
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
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A Socket Mode refresh (the server-sent disconnect) has the listener's next
  connection on the wire within 3 s: the session exit aborts the transport, so
  the bound is the reconnect backoff alone."""
  stand_in = conftest.WsServerNeverAnswersClose([*_WS_HELLO, {"type": "disconnect"}])
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
