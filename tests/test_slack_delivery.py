"""Acceptance tests for Slack reply delivery and the boot backfill (src.core.slack_listener)."""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Callable, Optional
from unittest.mock import patch

import pytest
from structlog.testing import capture_logs

from src.core import event_types as ET
from src.core import tasks as tasks_module
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, MasterRunRecord, SlackOrigin, utc_now
from src.core.sessions import SessionManager
from src.core.slack_listener import (
  _MAX_POST_CHARS,
  _REPLY_BUDGET_CHARS,
  SlackClient,
  _chunk_text,
  backfill_lost_summons,
  deliver_done,
)

_CHANNEL = "C_TEST"
_THREAD = "1700000000.000100"
_TEAM = "T_TEST"


class _FakeSlackClient:
  """Records posts and models each message's live reaction set; never touches the network.

  ``reactions`` maps a message ts to the names currently on it — the Slack-side
  end state the ack-clear tests assert against.
  """

  def __init__(self, *, fail_posts: bool = False, fail_remove: bool = False) -> None:
    self.posts: list[dict] = []
    self.remove_calls: list[dict] = []
    self.reactions: dict[str, set[str]] = {}
    self._fail_posts = fail_posts
    self._fail_remove = fail_remove

  async def post_message(self, channel: str, text: str, thread_ts: Optional[str] = None) -> dict:
    if self._fail_posts:
      raise RuntimeError("chat.postMessage failed")
    self.posts.append({"channel": channel, "text": text, "thread_ts": thread_ts})
    return {"ok": True}

  async def add_reaction(self, channel: str, name: str, ts: str) -> dict:
    self.reactions.setdefault(ts, set()).add(name)
    return {"ok": True}

  async def remove_reaction(self, channel: str, name: str, ts: str) -> dict:
    """Mirror SlackClient's contract: no_reaction is a payload, other failures raise."""
    self.remove_calls.append({"channel": channel, "name": name, "ts": ts})
    if self._fail_remove:
      raise RuntimeError("reactions.remove failed: missing_scope")
    names = self.reactions.setdefault(ts, set())
    if name not in names:
      return {"ok": False, "error": "no_reaction"}
    names.discard(name)
    return {"ok": True}


def _cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
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
async def test_over_length_answer_posts_ordered_chunks_without_any_link(tmp_path: Path) -> None:
  """A reply past Slack's hard limit splits at a blank line into ordered replies under the cap."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)

  long_text = "a" * 25000 + "\n\n" + "b" * 14999
  assert len(long_text) == 40001
  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _assistant(long_text))
  done = await _append(session_mgr, sid, _done(summon["id"]))

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await deliver_done(sid, done, cfg, session_mgr) is True

  texts = [p["text"] for p in client.posts]
  assert len(texts) > 1
  assert all(len(t) <= _MAX_POST_CHARS for t in texts)
  assert "".join(texts) == long_text
  assert all(p["channel"] == _CHANNEL and p["thread_ts"] == _THREAD for p in client.posts)
  assert not (cfg.sessions_dir / sid / "artifacts").exists()
  for t in texts:
    assert "http://" not in t and "https://" not in t and "absolute_filepath" not in t


@pytest.mark.asyncio
async def test_longest_observed_reply_is_one_chunk_and_one_post(tmp_path: Path) -> None:
  """The 4929-char reply — the longest seen in real sessions — is one message, not two."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)

  long_text = "x" * 4929
  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _assistant(long_text))
  done = await _append(session_mgr, sid, _done(summon["id"]))

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await deliver_done(sid, done, cfg, session_mgr) is True

  assert _chunk_text(long_text) == [long_text]
  assert len(client.posts) == 1
  assert client.posts[0]["text"] == long_text


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "text,expect_over_budget",
    [("a short reply", False), ("z" * 600, True)],
)
async def test_delivery_log_carries_over_budget_and_budget(
    tmp_path: Path, text: str, expect_over_budget: bool) -> None:
  """slack_delivery_done records the reply against the 500-char budget."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)

  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _assistant(text))
  done = await _append(session_mgr, sid, _done(summon["id"]))

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      capture_logs() as logs,
  ):
    assert await deliver_done(sid, done, cfg, session_mgr) is True

  outcome = [ev for ev in logs if ev["event"] == "slack_delivery_done"]
  assert len(outcome) == 1
  assert outcome[0]["chars"] == len(text)
  assert outcome[0]["chunks"] == 1
  assert outcome[0]["over_budget"] is expect_over_budget
  assert outcome[0]["budget"] == _REPLY_BUDGET_CHARS


@pytest.mark.asyncio
async def test_round_posts_only_the_final_composed_assistant_message(tmp_path: Path) -> None:
  """Middle assistant messages are work narration; only the last one is the reply."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)

  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _assistant("narration A"))
  await _append(session_mgr, sid, _assistant("## 完成\nthe reply"))
  done = await _append(session_mgr, sid, _done(summon["id"]))

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await deliver_done(sid, done, cfg, session_mgr) is True

  assert client.posts == [{"channel": _CHANNEL, "text": "## 完成\nthe reply", "thread_ts": _THREAD}]


@pytest.mark.asyncio
@pytest.mark.parametrize("trailing", ["", "  \n "])
async def test_trailing_empty_assistant_message_defers_to_the_last_non_empty(
    tmp_path: Path, trailing: str) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  sid = await _slack_session(session_mgr)

  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _assistant("real reply"))
  await _append(session_mgr, sid, _assistant(trailing))
  done = await _append(session_mgr, sid, _done(summon["id"]))

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await deliver_done(sid, done, cfg, session_mgr) is True

  assert [p["text"] for p in client.posts] == ["real reply"]


# ---------------------------------------------------------------------------
# _chunk_text
# ---------------------------------------------------------------------------


def test_chunk_text_below_the_limit_is_one_chunk() -> None:
  assert _chunk_text("x" * 3000) == ["x" * 3000]


def test_chunk_text_exact_limit_boundaries() -> None:
  """A text at the cap stays whole; a unit at the cap stays packed, then splits."""
  assert _chunk_text("y" * 40000) == ["y" * 40000]
  assert _chunk_text("aa\n\nbb", limit=4) == ["aa\n\n", "bb"]


def test_chunk_text_splits_between_paragraphs() -> None:
  text = "a" * 24999 + "\n\n" + "b" * 15001
  assert len(text) > _MAX_POST_CHARS
  chunks = _chunk_text(text)
  assert chunks == ["a" * 24999 + "\n\n", "b" * 15001]


def test_chunk_text_paragraph_over_the_limit_falls_to_newline_splits() -> None:
  text = "x" * 20000 + "\n" + "y" * 20000  # one paragraph, no blank line
  assert _chunk_text(text) == ["x" * 20000 + "\n", "y" * 20000]


def test_chunk_text_single_line_over_the_limit_hard_cuts() -> None:
  assert _chunk_text("z" * 40000) == ["z" * 40000]
  assert _chunk_text("z" * 45000) == ["z" * 40000, "z" * 5000]


def test_chunk_text_preserves_the_content_losslessly_and_in_order() -> None:
  text = "\n\n".join(f"para {i} " + "content " * 2000 for i in range(6))
  chunks = _chunk_text(text)
  assert len(chunks) > 1
  assert all(len(c) <= _MAX_POST_CHARS for c in chunks)
  assert "".join(chunks) == text


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


# ---------------------------------------------------------------------------
# Ack reaction lifecycle
# ---------------------------------------------------------------------------


def _spawner(tasks: list[asyncio.Task]) -> Callable[..., asyncio.Task]:
  """A create_logged_task substitute that spawns eagerly and captures every task."""

  def _spawn(coro, *, name=None):
    task = asyncio.get_running_loop().create_task(coro, name=name)
    tasks.append(task)
    return task

  return _spawn


class _StubSlackResponse:
  """A Slack Web API HTTP response carrying one fixed payload."""

  def __init__(self, payload: dict) -> None:
    self._payload = payload

  def raise_for_status(self) -> None:
    return None

  def json(self) -> dict:
    return self._payload


class _StubSlackHttp:
  """Answers every POST with one fixed Slack API payload."""

  def __init__(self, payload: dict) -> None:
    self._payload = payload

  async def post(self, url: str, **kwargs: object) -> _StubSlackResponse:
    return _StubSlackResponse(self._payload)


@pytest.mark.asyncio
async def test_delivered_round_clears_the_summons_eye(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  client.reactions[_THREAD] = {"eyes"}  # lit at the summon
  sid = await _slack_session(session_mgr)

  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _assistant("the answer"))
  done = await _append(session_mgr, sid, _done(summon["id"]))

  ack_tasks: list[asyncio.Task] = []
  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=_spawner(ack_tasks)),
  ):
    assert await deliver_done(sid, done, cfg, session_mgr) is True
    await asyncio.gather(*ack_tasks)

  assert [t.get_name() for t in ack_tasks] == [f"slack-ack-clear-{sid}"]
  assert client.remove_calls == [{"channel": _CHANNEL, "name": "eyes", "ts": _THREAD}]
  assert client.reactions[_THREAD] == set()


@pytest.mark.asyncio
async def test_failed_post_still_clears_the_eye_and_still_logs_the_delivery(tmp_path: Path) -> None:
  """A chunk that gave up leaves posted=False on the log, not a lingering eye."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient(fail_posts=True)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)

  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _assistant("never arrives"))
  done = await _append(session_mgr, sid, _done(summon["id"]))

  ack_tasks: list[asyncio.Task] = []
  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=_spawner(ack_tasks)),
      patch("src.core.slack_listener._RETRY_DELAYS", (0.0, 0.0)),
      capture_logs() as logs,
  ):
    assert await deliver_done(sid, done, cfg, session_mgr) is False
    await asyncio.gather(*ack_tasks)

  assert client.reactions[_THREAD] == set()
  outcomes = [ev for ev in logs if ev["event"] == "slack_delivery_done"]
  assert len(outcomes) == 1
  assert outcomes[0]["posted"] is False
  assert any(ev["event"] == "slack_post_gave_up" for ev in logs)


@pytest.mark.asyncio
async def test_summon_without_mention_ts_posts_normally_and_clears_nothing(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)

  summon_event = _summon()
  del summon_event["slack"]["mention_ts"]
  summon = await _append(session_mgr, sid, summon_event)
  await _append(session_mgr, sid, _assistant("the answer"))
  done = await _append(session_mgr, sid, _done(summon["id"]))

  ack_tasks: list[asyncio.Task] = []
  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=_spawner(ack_tasks)),
  ):
    assert await deliver_done(sid, done, cfg, session_mgr) is True
    await asyncio.gather(*ack_tasks)

  assert ack_tasks == []
  assert client.remove_calls == []
  assert client.reactions[_THREAD] == {"eyes"}
  assert [p["text"] for p in client.posts] == ["the answer"]


@pytest.mark.asyncio
async def test_remove_reaction_treats_no_reaction_as_the_end_state() -> None:
  """ok=false with error=no_reaction returns normally (idempotent); other errors raise."""
  client = SlackClient(
      _StubSlackHttp({"ok": False, "error": "no_reaction"}), bot_token="b", app_token="a")

  assert await client.remove_reaction("C_TEST", "eyes", _THREAD) == {
      "ok": False,
      "error": "no_reaction",
  }

  raising = SlackClient(
      _StubSlackHttp({"ok": False, "error": "missing_scope"}), bot_token="b", app_token="a")
  with pytest.raises(RuntimeError, match="reactions.remove failed"):
    await raising.remove_reaction("C_TEST", "eyes", _THREAD)


@pytest.mark.asyncio
async def test_backfill_posting_a_lost_summon_clears_its_eye(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient()
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)
  await _append(session_mgr, sid, _summon())

  ack_tasks: list[asyncio.Task] = []
  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=_spawner(ack_tasks)),
  ):
    assert await backfill_lost_summons(cfg, session_mgr) == 1
    await asyncio.gather(*ack_tasks)

  assert [t.get_name() for t in ack_tasks] == [f"slack-ack-clear-{sid}"]
  assert client.reactions[_THREAD] == set()
  assert len(client.posts) == 1


@pytest.mark.asyncio
async def test_remove_failure_leaves_a_stale_eye_and_stays_in_the_ack_task(tmp_path: Path) -> None:
  """missing_scope on the remove: delivery result and its log are unaffected."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  client = _FakeSlackClient(fail_remove=True)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)

  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _assistant("the answer"))
  done = await _append(session_mgr, sid, _done(summon["id"]))

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      capture_logs() as logs,
  ):
    assert await deliver_done(sid, done, cfg, session_mgr) is True
    ack = next(
        t for t in tasks_module._background_tasks
        if t.get_name() == f"slack-ack-clear-{sid}")
    await asyncio.gather(ack, return_exceptions=True)
    await asyncio.sleep(0)  # the task's logging done callback runs one tick later

  assert client.reactions[_THREAD] == {"eyes"}
  outcomes = [ev for ev in logs if ev["event"] == "slack_delivery_done"]
  assert len(outcomes) == 1
  assert outcomes[0]["posted"] is True
  failures = [ev for ev in logs if ev["event"] == "background_task_failed"]
  assert len(failures) == 1
  assert failures[0]["task_name"] == f"slack-ack-clear-{sid}"
  assert failures[0]["log_level"] == "error"
