"""Acceptance tests for Slack reply delivery and the boot backfill (src.core.slack_listener)."""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Optional
from unittest.mock import patch

import pytest

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, MasterRunRecord, SlackOrigin, utc_now
from src.core.sessions import SessionManager
from src.core.slack_listener import backfill_lost_summons, deliver_done

_CHANNEL = "C_TEST"
_THREAD = "1700000000.000100"
_TEAM = "T_TEST"


class _FakeSlackClient:
  """Records posts; never touches the network."""

  def __init__(self) -> None:
    self.posts: list[dict] = []

  async def post_message(self, channel: str, text: str, thread_ts: Optional[str] = None) -> dict:
    self.posts.append({"channel": channel, "text": text, "thread_ts": thread_ts})
    return {"ok": True}


def _cfg(tmp_path: Path, public_base_url: Optional[str] = None) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      public_base_url=public_base_url,
      slack_bot_token="test-bot-token",
      slack_app_token="test-app-token",
      slack_allowed_user_ids=["U_ALLOWED"],
  )


async def _slack_session(session_mgr: SessionManager, *, thread_ts: str = _THREAD) -> str:
  """Create a Slack-born session and return its id."""
  meta = await session_mgr.create_session(
      CreateSessionRequest(
          name="slack session",
          slack_origin=SlackOrigin(team_id=_TEAM, channel_id=_CHANNEL, thread_ts=thread_ts)))
  return meta.id


async def _append(session_mgr: SessionManager, sid: str, event: dict) -> dict:
  """Append one event to the session log (no aggregator, no delivery hook) and return it."""
  await session_mgr.save_chat_event(sid, event)
  return event


def _summon(thread_ts: str = _THREAD) -> dict:
  return {
      "type": ET.AGENT_MESSAGE,
      "content": "please summarize",
      "from_session": "src",
      "from_session_name": "Slack",
      "slack": {"channel_id": _CHANNEL, "thread_ts": thread_ts, "mention_ts": thread_ts},
  }


def _assistant(text: str) -> dict:
  return {"type": ET.ASSISTANT, "message": {"content": [{"type": "text", "text": text}]}}


def _done(input_event_id: Optional[str], exit_code: int = 0) -> dict:
  event = {"type": ET.MASTER_DONE, "exit_code": exit_code, "still_thinking": False}
  if input_event_id is not None:
    event["input_event_id"] = input_event_id
  return event


# ---------------------------------------------------------------------------
# Delivery
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_slack_round_posts_its_answer_to_the_paired_thread(tmp_path: Path) -> None:
  """The master_done funnel itself starts delivery, and it posts exactly once."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)

  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _assistant("git status is clean"))

  spawned: list[asyncio.Task] = []

  def _spawn(coro, *, name=None):
    task = asyncio.get_running_loop().create_task(coro)
    spawned.append(task)
    return task

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.sessions.create_logged_task", side_effect=_spawn),
  ):
    await session_mgr.persist_and_broadcast(sid, _done(summon["id"]))
    await asyncio.gather(*spawned)

  assert len(spawned) == 1
  assert client.posts == [{"channel": _CHANNEL, "text": "git status is clean", "thread_ts": _THREAD}]


@pytest.mark.asyncio
async def test_browser_typed_round_in_the_same_session_posts_nothing(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)

  typed = await _append(session_mgr, sid, {"type": ET.USER, "content": "from the browser"})
  await _append(session_mgr, sid, _assistant("browser answer"))
  done = await _append(session_mgr, sid, _done(typed["id"]))

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await deliver_done(sid, done, cfg, session_mgr) is False

  assert client.posts == []


@pytest.mark.asyncio
async def test_guard_path_done_without_input_event_id_posts_nothing(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)

  await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _assistant("half an answer"))
  done = await _append(session_mgr, sid, _done(None, exit_code=1))

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await deliver_done(sid, done, cfg, session_mgr) is False

  assert client.posts == []


@pytest.mark.asyncio
@pytest.mark.parametrize("exit_code", [0, -1])
async def test_duplicate_done_for_one_summon_posts_once(tmp_path: Path, exit_code: int) -> None:
  """A replayed round repeats the done; the input_event_id, not the exit code, discriminates."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)

  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _assistant("the answer"))
  first = await _append(session_mgr, sid, _done(summon["id"], exit_code=exit_code))
  second = await _append(session_mgr, sid, _done(summon["id"], exit_code=exit_code))

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await deliver_done(sid, first, cfg, session_mgr) is True
    assert await deliver_done(sid, second, cfg, session_mgr) is False

  assert [p["text"] for p in client.posts] == ["the answer"]


@pytest.mark.asyncio
async def test_summon_queued_behind_another_round_posts_only_its_own_text(tmp_path: Path) -> None:
  """The window is the previous done → this done, not the summon → this done."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)

  typed = await _append(session_mgr, sid, {"type": ET.USER, "content": "earlier round"})
  summon = await _append(session_mgr, sid, _summon())  # arrives while the first round runs
  await _append(session_mgr, sid, _assistant("first round answer"))
  await _append(session_mgr, sid, _done(typed["id"]))
  await _append(session_mgr, sid, _assistant("second round answer"))
  done = await _append(session_mgr, sid, _done(summon["id"]))

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await deliver_done(sid, done, cfg, session_mgr) is True

  assert [p["text"] for p in client.posts] == ["second round answer"]


@pytest.mark.asyncio
async def test_round_without_assistant_text_posts_the_failure_notice(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)

  summon = await _append(session_mgr, sid, _summon())
  done = await _append(session_mgr, sid, _done(summon["id"], exit_code=143))

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await deliver_done(sid, done, cfg, session_mgr) is True

  assert len(client.posts) == 1
  assert "exit_code=143" in client.posts[0]["text"]
  assert client.posts[0]["thread_ts"] == _THREAD


@pytest.mark.asyncio
async def test_over_length_answer_posts_a_link_to_a_written_artifact(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path, public_base_url="https://bot.example.com/")
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)

  long_text = "x" * 4000
  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _assistant(long_text))
  done = await _append(session_mgr, sid, _done(summon["id"]))

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await deliver_done(sid, done, cfg, session_mgr) is True

  artifact = cfg.sessions_dir / sid / "artifacts" / f"slack_reply_{_THREAD}.html"
  assert artifact.exists()
  page = artifact.read_text(encoding="utf-8")
  assert page.startswith("<!DOCTYPE html>")
  assert long_text in page

  assert len(client.posts) == 1
  text = client.posts[0]["text"]
  assert len(text) < len(long_text)
  assert f"https://bot.example.com/absolute_filepath{artifact.resolve()}" in text


@pytest.mark.asyncio
async def test_over_length_answer_without_public_base_url_posts_the_missing_setting(
    tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)

  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _assistant("y" * 4000))
  done = await _append(session_mgr, sid, _done(summon["id"]))

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await deliver_done(sid, done, cfg, session_mgr) is True

  assert len(client.posts) == 1
  text = client.posts[0]["text"]
  assert "public_base_url" in text
  assert "absolute_filepath" not in text
  assert not (cfg.sessions_dir / sid / "artifacts").exists()


@pytest.mark.asyncio
async def test_done_on_a_session_without_slack_origin_posts_nothing(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  meta = await session_mgr.create_session(CreateSessionRequest(name="browser session"))

  typed = await _append(session_mgr, meta.id, {"type": ET.USER, "content": "hello"})
  await _append(session_mgr, meta.id, _assistant("hi"))
  done = await _append(session_mgr, meta.id, _done(typed["id"]))

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await deliver_done(meta.id, done, cfg, session_mgr) is False

  assert client.posts == []


# ---------------------------------------------------------------------------
# Boot backfill
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_lost_summon_gets_one_notice_and_one_error_and_no_master_done(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await backfill_lost_summons(cfg, session_mgr) == 1

  assert len(client.posts) == 1
  assert client.posts[0]["channel"] == _CHANNEL
  assert client.posts[0]["thread_ts"] == _THREAD

  events = session_mgr.load_chat_events_sync(sid)
  errors = [ev for ev in events if ev.get("type") == ET.ASSISTANT_ERROR]
  assert len(errors) == 1
  assert errors[0]["slack_backfill"] == {"input_event_id": summon["id"]}
  assert errors[0]["content"]
  assert [ev for ev in events if ev.get("type") == ET.MASTER_DONE] == []


@pytest.mark.asyncio
async def test_backfill_run_twice_posts_once(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)
  await _append(session_mgr, sid, _summon())

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await backfill_lost_summons(cfg, session_mgr) == 1
    assert await backfill_lost_summons(cfg, session_mgr) == 0

  assert len(client.posts) == 1
  events = session_mgr.load_chat_events_sync(sid)
  assert len([ev for ev in events if ev.get("type") == ET.ASSISTANT_ERROR]) == 1
  assert [ev for ev in events if ev.get("type") == ET.MASTER_DONE] == []


@pytest.mark.asyncio
@pytest.mark.parametrize("exclusion", ["answered", "queued", "master_run"])
async def test_each_live_round_exclusion_suppresses_the_notice(tmp_path: Path, exclusion: str) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())

  queued: set[str] = set()
  if exclusion == "answered":
    await _append(session_mgr, sid, _done(summon["id"]))
  elif exclusion == "queued":
    queued = {summon["id"]}
  else:
    await session_mgr.persist_master_run(
        sid,
        MasterRunRecord(
            started_at=utc_now(), raw_log=str(tmp_path / "raw.jsonl"), user_event_id=summon["id"]))

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.agents.master_cc.queued_user_event_ids", return_value=queued),
  ):
    assert await backfill_lost_summons(cfg, session_mgr) == 0

  assert client.posts == []
  events = session_mgr.load_chat_events_sync(sid)
  assert [ev for ev in events if ev.get("type") == ET.ASSISTANT_ERROR] == []


@pytest.mark.asyncio
async def test_backfill_never_touches_a_session_without_slack_origin(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  meta = await session_mgr.create_session(CreateSessionRequest(name="browser session"))
  # Same shape as a lost summon, but the session was never summoned from Slack.
  await _append(session_mgr, meta.id, _summon())

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await backfill_lost_summons(cfg, session_mgr) == 0

  assert client.posts == []
  events = session_mgr.load_chat_events_sync(meta.id)
  assert [ev for ev in events if ev.get("type") == ET.ASSISTANT_ERROR] == []


@pytest.mark.asyncio
async def test_backfill_reports_an_archived_session_thread(tmp_path: Path) -> None:
  """Archived sessions are scanned too: the thread is still waiting on an answer."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)
  await _append(session_mgr, sid, _summon())
  await session_mgr.archive_session(sid)

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await backfill_lost_summons(cfg, session_mgr) == 1

  assert len(client.posts) == 1
