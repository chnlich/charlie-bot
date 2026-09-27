"""Acceptance tests for the Slack summon listener (src.core.slack_listener)."""

from __future__ import annotations

import asyncio
import contextlib
from collections.abc import Iterator
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
    ROOT,
    SLACK_LISTENER_CREATE_LOGGED_TASK_PATCH_TARGET,
    SLACK_LISTENER_TRIGGER_MASTER_PATCH_TARGET,
    FakeSlackClient,
    build_slack_cfg,
    make_task_spawner,
)

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager
from src.core.slack_listener import (
    CITATION_BOUNDARY,
    handle_app_mention,
    summon_session_id,
)

_TS = "1700000000.000100"

# The approved red-line and reply-format texts, read from the same prompts docs
# the builder reads and stripped exactly like the builder, so the tail
# assertions pin exact bytes.
_RED_LINE_PATH = ROOT / "prompts" / "slack_reply_redline.md"
_RED_LINE = _RED_LINE_PATH.read_text(encoding="utf-8").strip()
_FORMAT_PATH = ROOT / "prompts" / "slack_reply_format.md"
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


def _rig(tmp_path: Path) -> tuple[CharlieBotConfig, SessionManager, FakeSlackClient]:
  """Summon rig: cfg and session manager rooted at tmp_path, plus the recording fake client."""
  cfg = build_slack_cfg(tmp_path)
  return cfg, SessionManager(cfg), FakeSlackClient()


@contextlib.contextmanager
def _mention_seam(tasks: list[asyncio.Task] | None = None) -> Iterator[AsyncMock]:
  """Patch the seams an accepted mention fires through; yields the trigger mock.

  The yielded mock replaces ``trigger_master`` (an accepted mention wakes the
  master exactly once), and *tasks*, when given, collects the round the
  mention spawns through ``create_logged_task`` for the test to drain. Any
  further patch a test needs stays visible at the call site as a sibling
  context.
  """
  with contextlib.ExitStack() as stack:
    trigger = stack.enter_context(patch(SLACK_LISTENER_TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()))
    if tasks is not None:
      stack.enter_context(patch(SLACK_LISTENER_CREATE_LOGGED_TASK_PATCH_TARGET, side_effect=make_task_spawner(tasks)))
    yield trigger


@pytest.mark.asyncio
async def test_allowed_user_creates_session_and_persists_agent_message(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  event = _make_event()
  tasks = _spawn_round_tasks()

  with _mention_seam(tasks) as trigger:
    sid = await handle_app_mention(event, cfg, session_mgr, client)
    await asyncio.gather(*tasks)

  assert sid == _sid(event)

  meta = await session_mgr.get_session(sid)
  assert meta is not None
  assert meta.slack_origin is not None
  assert meta.slack_origin.team_id == "T_TEST"
  assert meta.slack_origin.channel_id == "C_TEST"
  assert meta.slack_origin.thread_ts == _TS

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

  events = session_mgr.load_chat_events_sync(sid)
  agent_messages = [ev for ev in events if ev.get("type") == ET.AGENT_MESSAGE]
  assert len(agent_messages) == 1
  assert agent_messages[0]["slack"] == {
      "channel_id": "C_TEST",
      "thread_ts": _TS,
      "mention_ts": _TS,
  }
  assert expected_url in agent_messages[0]["content"]
  assert agent_messages[0]["content"].endswith(f"{CITATION_BOUNDARY}\n{_RED_LINE}\n{_REPLY_FORMAT}")

  trigger.assert_awaited_once()
  assert trigger.await_args.kwargs["user_event_id"] == agent_messages[0]["id"]
  assert expected_url in trigger.await_args.args[1]


@pytest.mark.asyncio
async def test_same_thread_twice_reuses_the_session(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  event = _make_event()

  with _mention_seam():
    first = await handle_app_mention(event, cfg, session_mgr, client)
    second = await handle_app_mention(event, cfg, session_mgr, client)

  assert first == second
  sessions = await session_mgr.list_sessions()
  assert len(sessions) == 1
  assert sessions[0].id == first


_DROP_ROWS = [
    pytest.param({"user": "U_OTHER"}, id="disallowed-user"),
    pytest.param({"type": "message"}, id="non-app-mention"),
]


@pytest.mark.asyncio
@pytest.mark.parametrize(("event_overrides",), _DROP_ROWS)
async def test_unhandled_event_drops_with_no_side_effects(tmp_path: Path, event_overrides: dict) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  event = _make_event(**event_overrides)

  with _mention_seam():
    result = await handle_app_mention(event, cfg, session_mgr, client)

  assert result is None
  assert not client.calls
  assert await session_mgr.get_session(_sid(event)) is None


@pytest.mark.asyncio
async def test_trigger_master_forwards_user_event_id(tmp_path: Path) -> None:
  from src.core import master_trigger

  cfg, session_mgr, _ = _rig(tmp_path)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"))

  with patch.object(master_trigger, "run_message", new=AsyncMock(return_value=None)) as run_mock:
    await master_trigger.trigger_master(meta.id, "s", cfg, session_mgr, ET.AGENT_MESSAGE, user_event_id="evt-1")
    await master_trigger.trigger_master(meta.id, "s", cfg, session_mgr, ET.CHILD_REPORT)

  assert run_mock.call_count == 2
  assert run_mock.await_args_list[0].kwargs["user_event_id"] == "evt-1"
  assert run_mock.await_args_list[1].kwargs["user_event_id"] is None
