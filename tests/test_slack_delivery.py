"""Acceptance tests for the Slack reply path, the round-end audit, and the boot backfill (src.core.slack_listener)."""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock, patch

import pytest
from conftest import build_slack_cfg, make_task_spawner
from fastapi import FastAPI
from fastapi.testclient import TestClient
from structlog.testing import capture_logs

from src.api import internal
from src.api.deps import get_session_manager
from src.api.internal import router as internal_router
from src.core import event_types as ET
from src.core import tasks as tasks_module
from src.core.config import CharlieBotConfig
from src.core.message_aggregator import MessageAggregator
from src.core.models import (
  CreateSessionRequest,
  MasterRunRecord,
  SessionMetadata,
  SlackOrigin,
  utc_now,
)
from src.core.sessions import SessionManager
from src.core.slack_listener import (
  _MAX_POST_CHARS,
  _NO_REPLY_NOTICE,
  _REPLY_BUDGET_CHARS,
  SlackClient,
  SlackReplyError,
  _chunk_text,
  backfill_lost_summons,
  deliver_done,
  post_reply,
)

_CHANNEL = "C_TEST"
_THREAD = "1700000000.000100"
_TEAM = "T_TEST"
_PERMALINK = "https://fake.slack.test/archives/C_TEST/p1700000000000100"
# A summon prompt embeds prompts/slack_reply_format.md, which names the reply
# command; the audit reads that name off the summon to know its contract.
_SUMMON_CONTENT = f"Slack 线程召唤：{_PERMALINK}\n\nPost the reply with `charliebot slack reply --file <path>`."
_MARKER_ERA_CONTENT = f"Slack 线程召唤：{_PERMALINK}\n\nThe reply begins after a line that reads exactly `SLACK REPLY:`."


class _FakeSlackClient:
  """Records posts and models each message's live reaction set; never touches the network.

  ``reactions`` maps a message ts to the names currently on it — the Slack-side
  end state the ack-clear tests assert against.
  """

  def __init__(self, *, fail_posts: bool = False, fail_remove: bool = False) -> None:
    self.posts: list[dict] = []
    self.remove_calls: list[dict] = []
    self.reactions: dict[str, set[str]] = {}
    self.fail_posts = fail_posts
    self._fail_remove = fail_remove

  async def post_message(self, channel: str, text: str, thread_ts: str | None = None) -> dict:
    if self.fail_posts:
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


def _rig(
    tmp_path: Path, *, fail_posts: bool = False, fail_remove: bool = False
) -> tuple[CharlieBotConfig, SessionManager, _FakeSlackClient]:
  """Slack rig: cfg and manager rooted at tmp_path, plus a recording fake client."""
  cfg = build_slack_cfg(tmp_path)
  return cfg, SessionManager(cfg), _FakeSlackClient(fail_posts=fail_posts, fail_remove=fail_remove)


async def _slack_session(session_mgr: SessionManager, *, thread_ts: str = _THREAD) -> str:
  """Create a Slack-born session and return its id."""
  meta = await session_mgr.create_session(
      CreateSessionRequest(
          name="slack session",
          slack_origin=SlackOrigin(team_id=_TEAM, channel_id=_CHANNEL, thread_ts=thread_ts)))
  return meta.id


async def _append(session_mgr: SessionManager, sid: str, event: dict) -> dict:
  """Append one event to the session log (no aggregator, no audit hook) and return it."""
  await session_mgr.save_chat_event(sid, event)
  return event


async def _run_record(session_mgr: SessionManager, sid: str, user_event_id: str | None, tmp_path: Path) -> None:
  """Record a running round whose input is *user_event_id* (what post_reply binds a reply to)."""
  await session_mgr.persist_master_run(
      sid, MasterRunRecord(started_at=utc_now(), raw_log=str(tmp_path / "raw.jsonl"), user_event_id=user_event_id))


def _summon(thread_ts: str = _THREAD, content: str = _SUMMON_CONTENT) -> dict:
  return {
      "type": ET.AGENT_MESSAGE,
      "content": content,
      "from_session": "src",
      "from_session_name": "Slack",
      "slack": {"channel_id": _CHANNEL, "thread_ts": thread_ts, "mention_ts": thread_ts},
  }


def _nudge(summon: dict) -> dict:
  """A nudge event as the audit persists it: the summon's slack block plus nudge_of."""
  return {
      "type": ET.AGENT_MESSAGE,
      "content": "decide whether to post",
      "from_session": "src",
      "from_session_name": "Slack",
      "slack": {**summon["slack"], "nudge_of": summon["id"]},
  }


def _reply(answers: str | None, text: str = "the answer") -> dict:
  return {
      "type": ET.SLACK_REPLY,
      "content": text,
      "slack_reply": {"answers": answers, "chars": len(text), "chunks": 1},
  }


def _assistant(text: str) -> dict:
  return {"type": ET.ASSISTANT, "message": {"content": [{"type": "text", "text": text}]}}


def _done(input_event_id: str | None, exit_code: int = 0) -> dict:
  event = {"type": ET.MASTER_DONE, "exit_code": exit_code, "still_thinking": False}
  if input_event_id is not None:
    event["input_event_id"] = input_event_id
  return event


def _of_type(events: list[dict], event_type: str) -> list[dict]:
  return [ev for ev in events if ev.get("type") == event_type]


def _nudges(events: list[dict]) -> list[dict]:
  return [ev for ev in _of_type(events, ET.AGENT_MESSAGE) if "nudge_of" in (ev.get("slack") or {})]


def _notices(events: list[dict]) -> list[dict]:
  return [ev for ev in events if "slack_notice" in ev]


# ---------------------------------------------------------------------------
# Reply: post_reply
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_reply_in_a_summon_round_posts_persists_and_clears_the_eye(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  client.reactions[_THREAD] = {"eyes"}  # lit at the summon
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  await _run_record(session_mgr, sid, summon["id"], tmp_path)

  ack_tasks: list[asyncio.Task] = []
  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=make_task_spawner(ack_tasks)),
  ):
    result = await post_reply(sid, "the answer", cfg, session_mgr)
    await asyncio.gather(*ack_tasks)

  assert result == {"posted": True, "chars": 10, "chunks": 1, "over_budget": False, "answers": summon["id"]}
  assert client.posts == [{"channel": _CHANNEL, "text": "the answer", "thread_ts": _THREAD}]
  replies = _of_type(session_mgr.load_chat_events_sync(sid), ET.SLACK_REPLY)
  assert len(replies) == 1
  assert replies[0]["content"] == "the answer"
  assert replies[0]["slack_reply"] == {"answers": summon["id"], "chars": 10, "chunks": 1}
  assert client.remove_calls == [{"channel": _CHANNEL, "name": "eyes", "ts": _THREAD}]
  assert client.reactions[_THREAD] == set()


@pytest.mark.asyncio
@pytest.mark.parametrize("binding", ["no_run", "browser_typed", "trigger_wake"])
async def test_reply_from_a_round_no_summon_started_posts_with_null_answers_and_leaves_the_eye(
    tmp_path: Path, binding: str) -> None:
  """Any round of a Slack session may post; only a summon-bound round answers a summon."""
  cfg, session_mgr, client = _rig(tmp_path)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)
  await _append(session_mgr, sid, _summon())
  if binding == "browser_typed":
    typed = await _append(session_mgr, sid, {"type": ET.USER, "content": "from the browser"})
    await _run_record(session_mgr, sid, typed["id"], tmp_path)
  elif binding == "trigger_wake":
    wake = await _append(session_mgr, sid, {"type": ET.SCHEDULED_TRIGGER, "content": "watch fired"})
    await _run_record(session_mgr, sid, wake["id"], tmp_path)

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=make_task_spawner([])),
  ):
    result = await post_reply(sid, "fresh device code", cfg, session_mgr)

  assert result["posted"] is True
  assert result["answers"] is None
  assert [p["text"] for p in client.posts] == ["fresh device code"]
  assert _of_type(session_mgr.load_chat_events_sync(sid), ET.SLACK_REPLY)[0]["slack_reply"]["answers"] is None
  assert client.remove_calls == []
  assert client.reactions[_THREAD] == {"eyes"}


@pytest.mark.asyncio
async def test_reply_in_a_nudge_round_answers_the_original_summon_and_clears_its_eye(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _done(summon["id"]))
  nudge = await _append(session_mgr, sid, _nudge(summon))
  await _run_record(session_mgr, sid, nudge["id"], tmp_path)

  ack_tasks: list[asyncio.Task] = []
  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=make_task_spawner(ack_tasks)),
  ):
    result = await post_reply(sid, "late answer", cfg, session_mgr)
    await asyncio.gather(*ack_tasks)

  assert result["answers"] == summon["id"]
  assert client.reactions[_THREAD] == set()


@pytest.mark.asyncio
async def test_reply_for_a_session_without_a_slack_thread_is_refused_409(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  meta = await session_mgr.create_session(CreateSessionRequest(name="browser session"))

  with patch("src.core.slack_listener._bot_client", return_value=client):
    with pytest.raises(SlackReplyError) as excinfo:
      await post_reply(meta.id, "hello", cfg, session_mgr)

  assert excinfo.value.status == 409
  assert client.posts == []
  assert _of_type(session_mgr.load_chat_events_sync(meta.id), ET.SLACK_REPLY) == []


@pytest.mark.asyncio
async def test_unknown_session_is_refused_404(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  with patch("src.core.slack_listener._bot_client", return_value=client):
    with pytest.raises(SlackReplyError) as excinfo:
      await post_reply("no-such-session", "hello", cfg, session_mgr)
  assert excinfo.value.status == 404
  assert client.posts == []


@pytest.mark.asyncio
@pytest.mark.parametrize("text", ["", "   \n\t"])
async def test_blank_reply_is_refused_422_before_any_post(tmp_path: Path, text: str) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  sid = await _slack_session(session_mgr)
  with patch("src.core.slack_listener._bot_client", return_value=client):
    with pytest.raises(SlackReplyError) as excinfo:
      await post_reply(sid, text, cfg, session_mgr)
  assert excinfo.value.status == 422
  assert client.posts == []
  assert _of_type(session_mgr.load_chat_events_sync(sid), ET.SLACK_REPLY) == []


@pytest.mark.asyncio
async def test_slack_rejecting_the_post_is_502_and_persists_nothing(tmp_path: Path) -> None:
  """The agent learns from the readback and may retry; the log records only replies that landed."""
  cfg, session_mgr, client = _rig(tmp_path, fail_posts=True)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  await _run_record(session_mgr, sid, summon["id"], tmp_path)

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener._RETRY_DELAYS", (0.0, 0.0)),
      capture_logs() as logs,
  ):
    with pytest.raises(SlackReplyError) as excinfo:
      await post_reply(sid, "never arrives", cfg, session_mgr)

  assert excinfo.value.status == 502
  assert "nothing was persisted" in excinfo.value.detail
  assert client.posts == []
  assert _of_type(session_mgr.load_chat_events_sync(sid), ET.SLACK_REPLY) == []
  assert client.reactions[_THREAD] == {"eyes"}
  assert any(ev["event"] == "slack_post_gave_up" for ev in logs)
  assert not any(ev["event"] == "slack_reply_posted" for ev in logs)


@pytest.mark.asyncio
async def test_reply_past_the_post_cap_posts_ordered_chunks_and_counts_them(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  sid = await _slack_session(session_mgr)
  long_text = "a" * 25000 + "\n\n" + "b" * 14999
  assert len(long_text) == 40001

  with patch("src.core.slack_listener._bot_client", return_value=client):
    result = await post_reply(sid, long_text, cfg, session_mgr)

  texts = [p["text"] for p in client.posts]
  assert len(texts) > 1
  assert all(len(t) <= _MAX_POST_CHARS for t in texts)
  assert "".join(texts) == long_text
  assert all(p["channel"] == _CHANNEL and p["thread_ts"] == _THREAD for p in client.posts)
  assert result["chunks"] == len(texts)
  assert result["chars"] == len(long_text)
  assert result["over_budget"] is True
  reply = _of_type(session_mgr.load_chat_events_sync(sid), ET.SLACK_REPLY)[0]
  assert reply["slack_reply"]["chunks"] == len(texts)
  assert reply["content"] == long_text


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "text,expect_over_budget",
    [("a short reply", False), ("z" * 600, True)],
)
async def test_readback_and_log_measure_the_reply_against_the_budget(
    tmp_path: Path, text: str, expect_over_budget: bool) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  sid = await _slack_session(session_mgr)

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      capture_logs() as logs,
  ):
    result = await post_reply(sid, text, cfg, session_mgr)

  assert result["over_budget"] is expect_over_budget
  posted = [ev for ev in logs if ev["event"] == "slack_reply_posted"]
  assert len(posted) == 1
  assert posted[0]["chars"] == len(text)
  assert posted[0]["chunks"] == 1
  assert posted[0]["over_budget"] is expect_over_budget
  assert posted[0]["budget"] == _REPLY_BUDGET_CHARS
  assert posted[0]["answers"] is None


# ---------------------------------------------------------------------------
# Reply: the internal route
# ---------------------------------------------------------------------------


class _RouteSessions:
  """Session-manager double for the route tests: one session, a canned log, a persisted list."""

  def __init__(self, meta: SessionMetadata | None, events: list[dict] | None = None) -> None:
    self.meta = meta
    self.events = events or []
    self.persisted: list[tuple[str, dict[str, Any]]] = []

  async def get_session(self, session_id: str) -> SessionMetadata | None:
    return self.meta if self.meta is not None and self.meta.id == session_id else None

  def load_chat_events_sync(self, session_id: str) -> list[dict]:
    return self.events

  async def persist_and_broadcast(self, session_id: str, event: dict) -> None:
    self.persisted.append((session_id, event))


def _route_client(session_mgr: _RouteSessions) -> TestClient:
  app = FastAPI()
  app.include_router(internal_router, prefix="/api/internal")
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[internal.get_config] = lambda: build_slack_cfg(Path("/nonexistent"))
  return TestClient(app)


def _slack_meta() -> SessionMetadata:
  return SessionMetadata(
      id="s1", name="slack", slack_origin=SlackOrigin(team_id=_TEAM, channel_id=_CHANNEL, thread_ts=_THREAD))


def test_route_returns_the_readback_json() -> None:
  session_mgr = _RouteSessions(_slack_meta())
  client = _FakeSlackClient()
  with patch("src.core.slack_listener._bot_client", return_value=client), _route_client(session_mgr) as http:
    resp = http.post("/api/internal/slack/reply", json={"session_id": "s1", "text": "hi"})

  assert resp.status_code == 200
  assert resp.json() == {"posted": True, "chars": 2, "chunks": 1, "over_budget": False, "answers": None}
  assert [p["text"] for p in client.posts] == ["hi"]
  assert [ev["type"] for _, ev in session_mgr.persisted] == [ET.SLACK_REPLY]


@pytest.mark.parametrize(
    "meta,session_id,text,status,detail_fragment",
    [
        (None, "s1", "hi", 404, "Session not found"),
        (SessionMetadata(id="s1", name="browser"), "s1", "hi", 409, "no Slack thread"),
        (_slack_meta(), "s1", "   ", 422, "empty"),
    ],
)
def test_route_maps_refusals_to_status_codes_and_persists_nothing(
    meta: SessionMetadata | None, session_id: str, text: str, status: int, detail_fragment: str) -> None:
  session_mgr = _RouteSessions(meta)
  client = _FakeSlackClient()
  with patch("src.core.slack_listener._bot_client", return_value=client), _route_client(session_mgr) as http:
    resp = http.post("/api/internal/slack/reply", json={"session_id": session_id, "text": text})

  assert resp.status_code == status
  assert detail_fragment in resp.json()["detail"]
  assert client.posts == []
  assert session_mgr.persisted == []


def test_route_maps_a_rejected_post_to_502() -> None:
  session_mgr = _RouteSessions(_slack_meta())
  client = _FakeSlackClient(fail_posts=True)
  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener._RETRY_DELAYS", (0.0, 0.0)),
      _route_client(session_mgr) as http,
  ):
    resp = http.post("/api/internal/slack/reply", json={"session_id": "s1", "text": "hi"})

  assert resp.status_code == 502
  assert session_mgr.persisted == []


def test_route_rejects_extra_fields() -> None:
  session_mgr = _RouteSessions(_slack_meta())
  with _route_client(session_mgr) as http:
    resp = http.post("/api/internal/slack/reply", json={"session_id": "s1", "text": "hi", "channel": "C_X"})
  assert resp.status_code == 422
  assert session_mgr.persisted == []


def test_slack_reply_event_projects_as_a_system_message() -> None:
  """The persisted reply shows in the chat as what was posted, through the existing system role."""
  agg = MessageAggregator()
  deltas = list(agg.feed({
      "type": ET.SLACK_REPLY,
      "content": "hi there",
      "slack_reply": {"answers": None, "chars": 8, "chunks": 1},
      "timestamp": "2026-08-26T08:00:00Z",
  }))
  assert len(deltas) == 1
  assert deltas[0]["message"]["role"] == "system"
  assert deltas[0]["message"]["content"] == "Posted to Slack: hi there"


# ---------------------------------------------------------------------------
# Round-end audit: deliver_done
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_summon_round_with_a_reply_is_left_alone(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _reply(summon["id"]))
  done = await _append(session_mgr, sid, _done(summon["id"]))
  before = len(session_mgr.load_chat_events_sync(sid))
  trigger = AsyncMock()

  with patch.multiple("src.core.slack_listener", _bot_client=lambda cfg: client, trigger_master=trigger):
    assert await deliver_done(sid, done, cfg, session_mgr) is False

  assert client.posts == []
  assert trigger.await_count == 0
  assert len(session_mgr.load_chat_events_sync(sid)) == before


@pytest.mark.asyncio
async def test_summon_round_without_a_reply_wakes_the_master_once_with_a_nudge(tmp_path: Path) -> None:
  """The nudge copies the summon's slack block, names the summon, and starts one round bound to itself."""
  cfg, session_mgr, client = _rig(tmp_path)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _assistant("wrote the reply into a shell variable and stopped"))
  done = await _append(session_mgr, sid, _done(summon["id"]))
  tasks: list[asyncio.Task] = []
  trigger = AsyncMock()

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=make_task_spawner(tasks)),
      patch("src.core.slack_listener.trigger_master", trigger),
      capture_logs() as logs,
  ):
    assert await deliver_done(sid, done, cfg, session_mgr) is True
    await asyncio.gather(*tasks)

  nudges = _nudges(session_mgr.load_chat_events_sync(sid))
  assert len(nudges) == 1
  nudge = nudges[0]
  assert nudge["type"] == ET.AGENT_MESSAGE
  assert nudge["slack"] == {**summon["slack"], "nudge_of": summon["id"]}
  assert _PERMALINK in nudge["content"]
  assert "charliebot slack reply --file" in nudge["content"]
  trigger.assert_awaited_once()
  assert trigger.await_args.args[:2] == (sid, nudge["content"])
  assert trigger.await_args.kwargs["user_event_id"] == nudge["id"]
  assert [t.get_name() for t in tasks] == [f"slack-nudge-{sid}"]
  assert client.posts == []
  assert client.reactions[_THREAD] == {"eyes"}  # the question is still open
  assert [ev for ev in logs if ev["event"] == "slack_reply_nudge"][0]["summon_id"] == summon["id"]


@pytest.mark.asyncio
@pytest.mark.parametrize("exit_code", [0, -1])
async def test_replayed_done_for_the_summon_round_adds_no_second_nudge(tmp_path: Path, exit_code: int) -> None:
  """A replayed round repeats the done; the nudge event in the log, not the exit code, discriminates."""
  cfg, session_mgr, client = _rig(tmp_path)
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  first = await _append(session_mgr, sid, _done(summon["id"], exit_code=exit_code))
  second = await _append(session_mgr, sid, _done(summon["id"], exit_code=exit_code))
  tasks: list[asyncio.Task] = []
  trigger = AsyncMock()

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=make_task_spawner(tasks)),
      patch("src.core.slack_listener.trigger_master", trigger),
  ):
    assert await deliver_done(sid, first, cfg, session_mgr) is True
    assert await deliver_done(sid, second, cfg, session_mgr) is False
    await asyncio.gather(*tasks)

  assert len(_nudges(session_mgr.load_chat_events_sync(sid))) == 1
  assert trigger.await_count == 1


@pytest.mark.asyncio
async def test_nudge_round_without_a_reply_posts_the_notice_once_and_clears_the_eye(tmp_path: Path) -> None:
  """Asked twice and silent twice: the thread hears the notice, the eye goes out, no third generation."""
  cfg, session_mgr, client = _rig(tmp_path)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _done(summon["id"]))
  nudge = await _append(session_mgr, sid, _nudge(summon))
  await _append(session_mgr, sid, _assistant("nothing to add"))
  done = await _append(session_mgr, sid, _done(nudge["id"]))
  replay = await _append(session_mgr, sid, _done(nudge["id"]))
  tasks: list[asyncio.Task] = []
  trigger = AsyncMock()

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=make_task_spawner(tasks)),
      patch("src.core.slack_listener.trigger_master", trigger),
  ):
    assert await deliver_done(sid, done, cfg, session_mgr) is True
    assert await deliver_done(sid, replay, cfg, session_mgr) is False
    await asyncio.gather(*tasks)

  assert client.posts == [{"channel": _CHANNEL, "text": _NO_REPLY_NOTICE, "thread_ts": _THREAD}]
  events = session_mgr.load_chat_events_sync(sid)
  notices = _notices(events)
  assert len(notices) == 1
  assert notices[0]["type"] == ET.ASSISTANT_ERROR
  assert notices[0]["slack_notice"] == {"input_event_id": summon["id"]}
  assert notices[0]["content"]
  assert len(_nudges(events)) == 1
  assert trigger.await_count == 0
  assert [t.get_name() for t in tasks] == [f"slack-ack-clear-{sid}"]
  assert client.reactions[_THREAD] == set()


@pytest.mark.asyncio
async def test_nudge_round_with_a_reply_posts_no_notice(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _done(summon["id"]))
  nudge = await _append(session_mgr, sid, _nudge(summon))
  await _append(session_mgr, sid, _reply(summon["id"], "late answer"))
  done = await _append(session_mgr, sid, _done(nudge["id"]))

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await deliver_done(sid, done, cfg, session_mgr) is False

  assert client.posts == []
  assert _notices(session_mgr.load_chat_events_sync(sid)) == []


@pytest.mark.asyncio
async def test_notice_post_failure_leaves_no_marker_and_the_boot_audit_posts_it_later(tmp_path: Path) -> None:
  """Post first, mark on success: a failed post leaves the collector a retry instead of a silent thread."""
  cfg, session_mgr, client = _rig(tmp_path, fail_posts=True)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _done(summon["id"]))
  nudge = await _append(session_mgr, sid, _nudge(summon))
  done = await _append(session_mgr, sid, _done(nudge["id"]))
  tasks: list[asyncio.Task] = []

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=make_task_spawner(tasks)),
      patch("src.core.slack_listener._RETRY_DELAYS", (0.0, 0.0)),
      capture_logs() as logs,
  ):
    assert await deliver_done(sid, done, cfg, session_mgr) is False

  assert client.posts == []
  assert _notices(session_mgr.load_chat_events_sync(sid)) == []
  assert client.reactions[_THREAD] == {"eyes"}
  assert any(ev["event"] == "slack_post_gave_up" for ev in logs)

  client.fail_posts = False  # Slack is back at the next boot
  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=make_task_spawner(tasks)),
      patch("src.agents.master_cc.queued_user_event_ids", return_value=set()),
  ):
    assert await backfill_lost_summons(cfg, session_mgr) == 1
    await asyncio.gather(*tasks)

  assert [p["text"] for p in client.posts] == [_NO_REPLY_NOTICE]
  assert len(_notices(session_mgr.load_chat_events_sync(sid))) == 1
  assert client.reactions[_THREAD] == set()


@pytest.mark.asyncio
@pytest.mark.parametrize("kind", ["browser_typed", "guard_path", "no_slack_origin"])
async def test_rounds_outside_a_summon_are_not_audited(tmp_path: Path, kind: str) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  if kind == "no_slack_origin":
    sid = (await session_mgr.create_session(CreateSessionRequest(name="browser session"))).id
    typed = await _append(session_mgr, sid, {"type": ET.USER, "content": "hello"})
    done = await _append(session_mgr, sid, _done(typed["id"]))
  else:
    sid = await _slack_session(session_mgr)
    await _append(session_mgr, sid, _summon())
    if kind == "browser_typed":
      typed = await _append(session_mgr, sid, {"type": ET.USER, "content": "from the browser"})
      done = await _append(session_mgr, sid, _done(typed["id"]))
    else:
      done = await _append(session_mgr, sid, _done(None, exit_code=1))
  before = len(session_mgr.load_chat_events_sync(sid))
  trigger = AsyncMock()

  with patch.multiple("src.core.slack_listener", _bot_client=lambda cfg: client, trigger_master=trigger):
    assert await deliver_done(sid, done, cfg, session_mgr) is False

  assert client.posts == []
  assert trigger.await_count == 0
  assert len(session_mgr.load_chat_events_sync(sid)) == before


@pytest.mark.asyncio
async def test_summon_issued_under_the_marker_contract_is_outside_the_audit(tmp_path: Path) -> None:
  """History from before the reply command carries no unanswered question the audit can act on."""
  cfg, session_mgr, client = _rig(tmp_path)
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon(content=_MARKER_ERA_CONTENT))
  await _append(session_mgr, sid, _assistant("SLACK REPLY:\nthe old-style answer"))
  done = await _append(session_mgr, sid, _done(summon["id"]))
  trigger = AsyncMock()

  with patch.multiple("src.core.slack_listener", _bot_client=lambda cfg: client, trigger_master=trigger):
    assert await deliver_done(sid, done, cfg, session_mgr) is False
    with patch("src.agents.master_cc.queued_user_event_ids", return_value=set()):
      assert await backfill_lost_summons(cfg, session_mgr) == 0

  assert client.posts == []
  assert trigger.await_count == 0
  assert _nudges(session_mgr.load_chat_events_sync(sid)) == []


@pytest.mark.asyncio
async def test_the_master_done_funnel_itself_starts_the_audit(tmp_path: Path) -> None:
  """persist_and_broadcast spawns the audit for every done; here it produces the nudge."""
  cfg, session_mgr, client = _rig(tmp_path)
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  funnel_tasks: list[asyncio.Task] = []
  audit_tasks: list[asyncio.Task] = []
  trigger = AsyncMock()

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.sessions.create_logged_task", side_effect=make_task_spawner(funnel_tasks)),
      patch("src.core.slack_listener.create_logged_task", side_effect=make_task_spawner(audit_tasks)),
      patch("src.core.slack_listener.trigger_master", trigger),
  ):
    await session_mgr.persist_and_broadcast(sid, _done(summon["id"]))
    await asyncio.gather(*funnel_tasks)
    await asyncio.gather(*audit_tasks)

  assert [t.get_name() for t in funnel_tasks] == [f"slack-deliver-{sid}"]
  assert len(_nudges(session_mgr.load_chat_events_sync(sid))) == 1
  trigger.assert_awaited_once()


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


# ---------------------------------------------------------------------------
# Boot backfill: lost summons
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_lost_summon_gets_one_notice_and_one_error_and_no_master_done(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await backfill_lost_summons(cfg, session_mgr) == 1

  assert len(client.posts) == 1
  assert client.posts[0]["channel"] == _CHANNEL
  assert client.posts[0]["thread_ts"] == _THREAD

  events = session_mgr.load_chat_events_sync(sid)
  errors = _of_type(events, ET.ASSISTANT_ERROR)
  assert len(errors) == 1
  assert errors[0]["slack_backfill"] == {"input_event_id": summon["id"]}
  assert errors[0]["content"]
  assert _of_type(events, ET.MASTER_DONE) == []


@pytest.mark.asyncio
async def test_backfill_run_twice_posts_once(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  sid = await _slack_session(session_mgr)
  await _append(session_mgr, sid, _summon())

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await backfill_lost_summons(cfg, session_mgr) == 1
    assert await backfill_lost_summons(cfg, session_mgr) == 0

  assert len(client.posts) == 1
  events = session_mgr.load_chat_events_sync(sid)
  assert len(_of_type(events, ET.ASSISTANT_ERROR)) == 1
  assert _of_type(events, ET.MASTER_DONE) == []


@pytest.mark.asyncio
@pytest.mark.parametrize("exclusion", ["queued", "master_run"])
async def test_each_live_round_exclusion_suppresses_the_notice(tmp_path: Path, exclusion: str) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())

  queued: set[str] = set()
  if exclusion == "queued":
    queued = {summon["id"]}
  else:
    await _run_record(session_mgr, sid, summon["id"], tmp_path)

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.agents.master_cc.queued_user_event_ids", return_value=queued),
  ):
    assert await backfill_lost_summons(cfg, session_mgr) == 0

  assert client.posts == []
  events = session_mgr.load_chat_events_sync(sid)
  assert _of_type(events, ET.ASSISTANT_ERROR) == []


@pytest.mark.asyncio
async def test_answered_summon_is_not_lost_and_its_reply_settles_the_audit(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _reply(summon["id"]))
  await _append(session_mgr, sid, _done(summon["id"]))

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.agents.master_cc.queued_user_event_ids", return_value=set()),
  ):
    assert await backfill_lost_summons(cfg, session_mgr) == 0

  assert client.posts == []
  assert _of_type(session_mgr.load_chat_events_sync(sid), ET.ASSISTANT_ERROR) == []


@pytest.mark.asyncio
async def test_backfill_never_touches_a_session_without_slack_origin(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  meta = await session_mgr.create_session(CreateSessionRequest(name="browser session"))
  # Same shape as a lost summon, but the session was never summoned from Slack.
  await _append(session_mgr, meta.id, _summon())

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await backfill_lost_summons(cfg, session_mgr) == 0

  assert client.posts == []
  events = session_mgr.load_chat_events_sync(meta.id)
  assert _of_type(events, ET.ASSISTANT_ERROR) == []


@pytest.mark.asyncio
async def test_backfill_reports_an_archived_session_thread(tmp_path: Path) -> None:
  """Archived sessions are scanned too: the thread is still waiting on an answer."""
  cfg, session_mgr, client = _rig(tmp_path)
  sid = await _slack_session(session_mgr)
  await _append(session_mgr, sid, _summon())
  await session_mgr.archive_session(sid)

  with patch("src.core.slack_listener._bot_client", return_value=client):
    assert await backfill_lost_summons(cfg, session_mgr) == 1

  assert len(client.posts) == 1


# ---------------------------------------------------------------------------
# Boot backfill: the round-end audit over finished rounds
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_boot_audit_nudges_a_summon_round_left_without_a_reply_once(tmp_path: Path) -> None:
  """The crash window between a done and its nudge closes at the next boot; a second boot finds nothing."""
  cfg, session_mgr, client = _rig(tmp_path)
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _done(summon["id"]))
  tasks: list[asyncio.Task] = []
  trigger = AsyncMock()

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=make_task_spawner(tasks)),
      patch("src.core.slack_listener.trigger_master", trigger),
      patch("src.agents.master_cc.queued_user_event_ids", return_value=set()),
  ):
    assert await backfill_lost_summons(cfg, session_mgr) == 1
    await asyncio.gather(*tasks)
    # The nudge round is now running (trigger_master is a mock here, so record
    # the run by hand); a second boot finds the nudge and leaves the summon alone.
    nudges = _nudges(session_mgr.load_chat_events_sync(sid))
    await _run_record(session_mgr, sid, nudges[0]["id"], tmp_path)
    assert await backfill_lost_summons(cfg, session_mgr) == 0

  assert len(nudges) == 1
  assert nudges[0]["slack"]["nudge_of"] == summon["id"]
  assert len(_nudges(session_mgr.load_chat_events_sync(sid))) == 1
  assert trigger.await_count == 1
  assert client.posts == []


@pytest.mark.asyncio
async def test_boot_audit_posts_the_notice_for_a_nudge_round_left_without_a_reply_once(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _done(summon["id"]))
  nudge = await _append(session_mgr, sid, _nudge(summon))
  await _append(session_mgr, sid, _done(nudge["id"]))
  tasks: list[asyncio.Task] = []
  trigger = AsyncMock()

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=make_task_spawner(tasks)),
      patch("src.core.slack_listener.trigger_master", trigger),
      patch("src.agents.master_cc.queued_user_event_ids", return_value=set()),
  ):
    assert await backfill_lost_summons(cfg, session_mgr) == 1
    assert await backfill_lost_summons(cfg, session_mgr) == 0
    await asyncio.gather(*tasks)

  assert [p["text"] for p in client.posts] == [_NO_REPLY_NOTICE]
  events = session_mgr.load_chat_events_sync(sid)
  assert len(_notices(events)) == 1
  assert len(_nudges(events)) == 1
  assert trigger.await_count == 0
  assert client.reactions[_THREAD] == set()


@pytest.mark.asyncio
async def test_lost_nudge_gets_the_lost_summon_report_and_no_second_action(tmp_path: Path) -> None:
  """A nudge still queued at the crash is collected by _lost_summons; the summon's done then finds the nudge."""
  cfg, session_mgr, client = _rig(tmp_path)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _done(summon["id"]))
  nudge = await _append(session_mgr, sid, _nudge(summon))
  tasks: list[asyncio.Task] = []
  trigger = AsyncMock()

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=make_task_spawner(tasks)),
      patch("src.core.slack_listener.trigger_master", trigger),
      patch("src.agents.master_cc.queued_user_event_ids", return_value=set()),
  ):
    assert await backfill_lost_summons(cfg, session_mgr) == 1
    await asyncio.gather(*tasks)

  assert len(client.posts) == 1
  assert client.posts[0]["text"] != _NO_REPLY_NOTICE  # the lost-summon report, not the no-reply notice
  events = session_mgr.load_chat_events_sync(sid)
  assert [ev["slack_backfill"] for ev in events if "slack_backfill" in ev] == [{"input_event_id": nudge["id"]}]
  assert _notices(events) == []
  assert len(_nudges(events)) == 1
  assert trigger.await_count == 0
  assert client.reactions[_THREAD] == set()


# ---------------------------------------------------------------------------
# Ack reaction lifecycle
# ---------------------------------------------------------------------------


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
async def test_reply_to_a_summon_without_mention_ts_posts_normally_and_clears_nothing(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)
  summon_event = _summon()
  del summon_event["slack"]["mention_ts"]
  summon = await _append(session_mgr, sid, summon_event)
  await _run_record(session_mgr, sid, summon["id"], tmp_path)
  ack_tasks: list[asyncio.Task] = []

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=make_task_spawner(ack_tasks)),
  ):
    result = await post_reply(sid, "the answer", cfg, session_mgr)
    await asyncio.gather(*ack_tasks)

  assert result["answers"] == summon["id"]
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
  cfg, session_mgr, client = _rig(tmp_path)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)
  await _append(session_mgr, sid, _summon())

  ack_tasks: list[asyncio.Task] = []
  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      patch("src.core.slack_listener.create_logged_task", side_effect=make_task_spawner(ack_tasks)),
  ):
    assert await backfill_lost_summons(cfg, session_mgr) == 1
    await asyncio.gather(*ack_tasks)

  assert [t.get_name() for t in ack_tasks] == [f"slack-ack-clear-{sid}"]
  assert client.reactions[_THREAD] == set()
  assert len(client.posts) == 1


@pytest.mark.asyncio
async def test_remove_failure_leaves_a_stale_eye_and_stays_in_the_ack_task(tmp_path: Path) -> None:
  """missing_scope on the remove: the reply, its readback, and its log are unaffected."""
  cfg, session_mgr, client = _rig(tmp_path, fail_remove=True)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  await _run_record(session_mgr, sid, summon["id"], tmp_path)

  with (
      patch("src.core.slack_listener._bot_client", return_value=client),
      capture_logs() as logs,
  ):
    result = await post_reply(sid, "the answer", cfg, session_mgr)
    ack = next(
        t for t in tasks_module._background_tasks
        if t.get_name() == f"slack-ack-clear-{sid}")
    await asyncio.gather(ack, return_exceptions=True)
    await asyncio.sleep(0)  # the task's logging done callback runs one tick later

  assert result["posted"] is True
  assert client.reactions[_THREAD] == {"eyes"}
  assert len([ev for ev in logs if ev["event"] == "slack_reply_posted"]) == 1
  failures = [ev for ev in logs if ev["event"] == "background_task_failed"]
  assert len(failures) == 1
  assert failures[0]["task_name"] == f"slack-ack-clear-{sid}"
  assert failures[0]["log_level"] == "error"
