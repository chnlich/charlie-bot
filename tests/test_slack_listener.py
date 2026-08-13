"""Acceptance tests for the Slack summon listener (src.core.slack_listener)."""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Optional
from unittest.mock import AsyncMock, patch

import pytest

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, SlackOrigin
from src.core.sessions import SessionManager
from src.core.slack_listener import (
  CITATION_BOUNDARY,
  handle_app_mention,
  summon_session_id,
)

_TS = "1700000000.000100"


def _spawn_round_tasks() -> list[asyncio.Task]:
  """Reset the task sink used by the patched create_logged_task."""
  _spawn_round_tasks.tasks = []
  return _spawn_round_tasks.tasks


_spawn_round_tasks.tasks: list[asyncio.Task] = []


class _FakeSlackClient:
  """Records calls and returns canned payloads; never touches the network."""

  def __init__(self, messages: Optional[list[dict]] = None) -> None:
    self.calls: list[tuple[str, dict]] = []
    self.messages: list[dict] = messages or [
        {"user": "U_ALLOWED", "text": "please summarize"},
        {"user": "U_ALLOWED", "text": "and check git status"},
    ]

  async def open_connection(self) -> str:
    self.calls.append(("open_connection", {"channel": None}))
    return "wss://fake.example/socket"

  async def post_message(self, channel: str, text: str, thread_ts: Optional[str] = None) -> dict:
    self.calls.append(("post_message", {"channel": channel, "text": text, "thread_ts": thread_ts}))
    return {"ok": True}

  async def thread_text(self, channel: str, ts: str) -> list[dict]:
    self.calls.append(("thread_text", {"channel": channel, "ts": ts}))
    return self.messages


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


def _cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      slack_bot_token="test-bot-token",
      slack_app_token="test-app-token",
      slack_allowed_user_ids=["U_ALLOWED"],
  )


def _thread_ts(event: dict) -> str:
  return event.get("thread_ts") or event["ts"]


def _sid(event: dict) -> str:
  return summon_session_id(event["team"], event["channel"], _thread_ts(event))


def _origin(event: dict) -> SlackOrigin:
  return SlackOrigin(
      team_id=event["team"], channel_id=event["channel"], thread_ts=_thread_ts(event))


@pytest.mark.asyncio
async def test_allowed_user_creates_session_and_persists_agent_message(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  event = _make_event()

  def _spawn(coro, *, name=None):
    task = asyncio.get_running_loop().create_task(coro)
    _spawn_round_tasks.tasks.append(task)
    return task

  with (
      patch("src.core.slack_listener.trigger_master", new=AsyncMock()) as trigger,
      patch("src.core.slack_listener.create_logged_task", side_effect=_spawn),
  ):
    sid = await handle_app_mention(event, cfg, session_mgr, client)
    await asyncio.gather(*_spawn_round_tasks.tasks)

  assert sid == _sid(event)

  meta = await session_mgr.get_session(sid)
  assert meta is not None
  assert meta.slack_origin is not None
  assert meta.slack_origin.team_id == "T_TEST"
  assert meta.slack_origin.channel_id == "C_TEST"
  assert meta.slack_origin.thread_ts == _TS

  posts = [c for name, c in client.calls if name == "post_message"]
  assert len(posts) == 1
  thread_reads = [c for name, c in client.calls if name == "thread_text"]
  assert len(thread_reads) == 1

  events = session_mgr.load_chat_events_sync(sid)
  agent_messages = [ev for ev in events if ev.get("type") == ET.AGENT_MESSAGE]
  assert len(agent_messages) == 1
  assert agent_messages[0]["slack"] == {
      "channel_id": "C_TEST",
      "thread_ts": _TS,
      "mention_ts": _TS,
  }
  assert agent_messages[0]["content"].endswith(CITATION_BOUNDARY)

  trigger.assert_awaited_once()
  assert trigger.await_args.kwargs["user_event_id"] == agent_messages[0]["id"]


@pytest.mark.asyncio
async def test_same_thread_twice_reuses_the_session(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  event = _make_event()

  with patch("src.core.slack_listener.trigger_master", new=AsyncMock()):
    first = await handle_app_mention(event, cfg, session_mgr, client)
    second = await handle_app_mention(event, cfg, session_mgr, client)

  assert first == second
  sessions = await session_mgr.list_sessions()
  assert len(sessions) == 1
  assert sessions[0].id == first


@pytest.mark.asyncio
async def test_disallowed_user_drops_with_no_side_effects(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  event = _make_event(user="U_OTHER")

  with patch("src.core.slack_listener.trigger_master", new=AsyncMock()):
    result = await handle_app_mention(event, cfg, session_mgr, client)

  assert result is None
  assert client.calls == []
  assert await session_mgr.get_session(_sid(event)) is None


@pytest.mark.asyncio
async def test_non_app_mention_message_drops_with_no_side_effects(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  event = _make_event(type="message")

  with patch("src.core.slack_listener.trigger_master", new=AsyncMock()):
    result = await handle_app_mention(event, cfg, session_mgr, client)

  assert result is None
  assert client.calls == []
  assert await session_mgr.get_session(_sid(event)) is None


@pytest.mark.asyncio
async def test_top_level_mention_uses_own_ts(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  event = _make_event()  # no thread_ts, so the mention's own ts is the thread

  with patch("src.core.slack_listener.trigger_master", new=AsyncMock()):
    sid = await handle_app_mention(event, cfg, session_mgr, client)

  assert sid == summon_session_id("T_TEST", "C_TEST", _TS)
  posts = [c for name, c in client.calls if name == "post_message"]
  assert len(posts) == 1
  assert posts[0]["thread_ts"] == _TS


@pytest.mark.asyncio
async def test_archived_session_is_unarchived_not_duplicated(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  event = _make_event()
  sid = _sid(event)
  await session_mgr.create_session(
      CreateSessionRequest(session_id=sid, name="slack-archived", slack_origin=_origin(event)))
  await session_mgr.archive_session(sid)
  assert (await session_mgr.get_session(sid)).status == "archived"

  client = _FakeSlackClient()
  with patch("src.core.slack_listener.trigger_master", new=AsyncMock()):
    result = await handle_app_mention(event, cfg, session_mgr, client)

  assert result == sid
  assert (await session_mgr.get_session(sid)).status == "active"
  sessions = await session_mgr.list_sessions()
  assert len(sessions) == 1
  assert sessions[0].id == sid


@pytest.mark.asyncio
async def test_trigger_master_forwards_user_event_id(tmp_path: Path) -> None:
  from src.core import master_trigger

  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"))

  with patch.object(master_trigger, "run_message", new=AsyncMock(return_value=None)) as run_mock:
    await master_trigger.trigger_master(meta.id, "s", cfg, session_mgr, user_event_id="evt-1")
    await master_trigger.trigger_master(meta.id, "s", cfg, session_mgr)

  assert run_mock.call_count == 2
  assert run_mock.await_args_list[0].kwargs["user_event_id"] == "evt-1"
  assert run_mock.await_args_list[1].kwargs["user_event_id"] is None