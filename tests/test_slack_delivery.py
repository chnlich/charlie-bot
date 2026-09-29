"""Acceptance tests for the Slack reply path, the round-end audit, and the boot backfill (src.core.slack_listener)."""

from __future__ import annotations

import asyncio
import contextlib
from collections.abc import Iterator
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
    PUBLISH_BASE_URL,
    SLACK_LISTENER_BOT_CLIENT_PATCH_TARGET,
    THREAD_ENTRY_CREATE_LOGGED_TASK_PATCH_TARGET,
    THREAD_ENTRY_TRIGGER_MASTER_PATCH_TARGET,
    FakeSlackClient,
    build_slack_cfg,
    make_task_spawner,
)
from structlog.testing import capture_logs

from src.agents import master_cc_state
from src.agents.backends.base import make_text_event
from src.core import event_types as ET
from src.core.config import CharlieBotConfig, PublishConfig
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
    _NO_REPLY_NOTICE,
    SlackReplyError,
    _lost_summons,
    backfill_lost_summons,
    deliver_done,
    post_reply,
)

_CHANNEL = "C_TEST"
_THREAD = "1700000000.000100"
_TEAM = "T_TEST"
_RETRY_DELAYS_PATCH_TARGET = "src.core.thread_entry._RETRY_DELAYS"
_QUEUED_USER_EVENT_IDS_PATCH_TARGET = "src.agents.master_cc.queued_user_event_ids"
_PERMALINK = "https://fake.slack.test/archives/C_TEST/p1700000000000100"
# A summon prompt embeds prompts/thread_reply_format.md, which names the reply
# command; the audit reads that name off the summon to know its contract.
_SUMMON_CONTENT = f"Slack 线程召唤：{_PERMALINK}\n\nPost the reply with `charliebot slack reply --file <path>`."


def _rig(tmp_path: Path,
         *,
         fail_posts: bool = False,
         fail_remove: bool = False) -> tuple[CharlieBotConfig, SessionManager, FakeSlackClient]:
  """Slack rig: cfg and manager rooted at tmp_path, plus a recording fake client."""
  cfg = build_slack_cfg(tmp_path)
  return cfg, SessionManager(cfg), FakeSlackClient(fail_posts=fail_posts, fail_remove=fail_remove)


@contextlib.contextmanager
def _listener_seam(
    client: FakeSlackClient,
    *,
    tasks: list[asyncio.Task] | None = None,
    trigger: AsyncMock | None = None,
    queued: set[str] | None = None,
) -> Iterator[None]:
  """Patch the slack_listener module seams; the seam wiring lives here and nowhere else.

  ``_bot_client`` always returns *client*. *tasks* feeds the thread core's
  ``create_logged_task`` (the ack and nudge task spawner), *trigger* replaces the
  thread core's ``trigger_master``, *queued* pins
  ``master_cc.queued_user_event_ids``; each stays unpatched when its argument is
  None. Any further patch a test needs (retry delays, log capture) stays visible
  at the call site as a sibling context.
  """
  with contextlib.ExitStack() as stack:
    stack.enter_context(patch(SLACK_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client))
    if tasks is not None:
      stack.enter_context(patch(THREAD_ENTRY_CREATE_LOGGED_TASK_PATCH_TARGET, side_effect=make_task_spawner(tasks)))
    if trigger is not None:
      stack.enter_context(patch(THREAD_ENTRY_TRIGGER_MASTER_PATCH_TARGET, trigger))
    if queued is not None:
      stack.enter_context(patch(_QUEUED_USER_EVENT_IDS_PATCH_TARGET, return_value=queued))
    yield


async def _slack_session(session_mgr: SessionManager) -> str:
  """Create a Slack-born session and return its id."""
  meta = await session_mgr.create_session(
      CreateSessionRequest(
          name="slack session", slack_origin=SlackOrigin(team_id=_TEAM, channel_id=_CHANNEL, thread_ts=_THREAD)))
  return meta.id


async def _append(session_mgr: SessionManager, sid: str, event: dict) -> dict:
  """Append one event to the session log (no aggregator, no audit hook) and return it."""
  await session_mgr.save_chat_event(sid, event)
  return event


async def _run_record(session_mgr: SessionManager, sid: str, user_event_id: str | None, tmp_path: Path) -> None:
  """Record a running round whose input is *user_event_id* (what post_reply binds a reply to)."""
  await session_mgr.persist_master_run(
      sid,
      MasterRunRecord(
          started_at=utc_now(),
          raw_log=str(tmp_path / "raw.jsonl"),
          user_event_ids=[user_event_id] if user_event_id else []))


def _summon(content: str = _SUMMON_CONTENT) -> dict:
  return {
      "type": ET.AGENT_MESSAGE,
      "content": content,
      "from_session": "src",
      "from_session_name": "Slack",
      "slack": {
          "channel_id": _CHANNEL,
          "thread_ts": _THREAD,
          "mention_ts": _THREAD
      },
  }


def _nudge(summon: dict) -> dict:
  """A nudge event as the audit persists it: the summon's slack block plus nudge_of."""
  return {
      "type": ET.AGENT_MESSAGE,
      "content": "decide whether to post",
      "from_session": "src",
      "from_session_name": "Slack",
      "slack": {
          **summon["slack"], "nudge_of": summon["id"]
      },
  }


def _done(input_event_id: str | None, exit_code: int = 0) -> dict:
  event = {"type": ET.MASTER_DONE, "exit_code": exit_code, "still_thinking": False}
  if input_event_id is not None:
    event["input_event_id"] = input_event_id
  return event


async def _unanswered_nudge_round(session_mgr: SessionManager, client: FakeSlackClient) -> tuple[str, dict, dict]:
  """A Slack thread re-asked once with no reply yet; returns (sid, summon, nudge).

  The summon round completed and the eyes reaction marks the thread
  awaiting-answer, so the round-end audit and the boot backfill treat the
  nudge round as still open.
  """
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, _done(summon["id"]))
  nudge = await _append(session_mgr, sid, _nudge(summon))
  return sid, summon, nudge


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
  with _listener_seam(client, tasks=ack_tasks):
    result = await post_reply(sid, "the answer", cfg, session_mgr)
    await asyncio.gather(*ack_tasks)

  assert result == {
      "posted": True,
      "text": "the answer",
      "operator_only_note": None,
      "chars": 10,
      "chunks": 1,
      "over_budget": False,
      "answers": summon["id"],
  }
  assert client.posts == [{"channel": _CHANNEL, "text": "the answer", "thread_ts": _THREAD}]
  replies = _of_type(session_mgr.load_chat_events_sync(sid), ET.SLACK_REPLY)
  assert len(replies) == 1
  assert replies[0]["content"] == "the answer"
  assert replies[0]["slack_reply"] == {"answers": summon["id"], "chars": 10, "chunks": 1}
  assert client.remove_calls == [{"channel": _CHANNEL, "name": "eyes", "ts": _THREAD}]
  assert not client.reactions[_THREAD]


@pytest.mark.asyncio
@pytest.mark.parametrize("text", ["", "   \n\t"])
async def test_blank_reply_is_refused_422_before_any_post(tmp_path: Path, text: str) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  sid = await _slack_session(session_mgr)
  with _listener_seam(client), pytest.raises(SlackReplyError) as excinfo:
    await post_reply(sid, text, cfg, session_mgr)
  assert excinfo.value.status == 422
  assert not client.posts
  assert not _of_type(session_mgr.load_chat_events_sync(sid), ET.SLACK_REPLY)


@pytest.mark.asyncio
async def test_slack_rejecting_the_post_is_502_and_persists_nothing(tmp_path: Path) -> None:
  """The agent learns from the readback and may retry; the log records only replies that landed."""
  cfg, session_mgr, client = _rig(tmp_path, fail_posts=True)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  await _run_record(session_mgr, sid, summon["id"], tmp_path)

  with (
      _listener_seam(client),
      patch(_RETRY_DELAYS_PATCH_TARGET, (0.0, 0.0)),
      capture_logs() as logs,
      pytest.raises(SlackReplyError) as excinfo,
  ):
    await post_reply(sid, "never arrives", cfg, session_mgr)

  assert excinfo.value.status == 502
  assert "nothing was persisted" in excinfo.value.detail
  assert not client.posts
  assert not _of_type(session_mgr.load_chat_events_sync(sid), ET.SLACK_REPLY)
  assert client.reactions[_THREAD] == {"eyes"}
  assert any(ev["event"] == "slack_post_gave_up" for ev in logs)
  assert not any(ev["event"] == "slack_reply_posted" for ev in logs)


# ---------------------------------------------------------------------------
# Reply: the publish-lane rewrite before any chunk posts
# ---------------------------------------------------------------------------

_PUB_BASE = PUBLISH_BASE_URL
_FILE_HOST = "https://agent.example.test:18498"


def _pub_cfg(tmp_path: Path) -> CharlieBotConfig:
  """The slack rig's cfg with the publish lane deployed under tmp_path."""
  lane = tmp_path / "publish"
  lane.mkdir(parents=True, exist_ok=True)
  return build_slack_cfg(tmp_path).model_copy(update={"publish": PublishConfig(dir=lane, public_base_url=_PUB_BASE)})


def _rig_with_publish_lane(tmp_path: Path) -> tuple[CharlieBotConfig, SessionManager, FakeSlackClient]:
  """The slack rig with the publish lane deployed: the rewrite tests' shared fixture."""
  cfg = _pub_cfg(tmp_path)
  return cfg, SessionManager(cfg), FakeSlackClient()


@pytest.mark.asyncio
async def test_reply_refuses_as_a_whole_when_the_linked_file_is_gone(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig_with_publish_lane(tmp_path)
  gone = tmp_path / "artifacts" / "gone.html"
  sid = await _slack_session(session_mgr)

  with _listener_seam(client), pytest.raises(SlackReplyError) as excinfo:
    await post_reply(sid, f"details: {_FILE_HOST}/absolute_filepath/{gone}", cfg, session_mgr)

  assert excinfo.value.status == 422
  assert f"{_FILE_HOST}/absolute_filepath/{gone}" in excinfo.value.detail
  assert not client.posts
  assert not _of_type(session_mgr.load_chat_events_sync(sid), ET.SLACK_REPLY)
  assert not any(cfg.publish.dir.iterdir())


# ---------------------------------------------------------------------------
# Reply: the internal route
# ---------------------------------------------------------------------------


def test_slack_reply_event_projects_as_a_system_message() -> None:
  """The persisted reply shows in the chat as what was posted, through the existing system role."""
  agg = MessageAggregator()
  deltas = list(
      agg.feed(
          {
              "type": ET.SLACK_REPLY,
              "content": "hi there",
              "slack_reply": {
                  "answers": None,
                  "chars": 8,
                  "chunks": 1
              },
              "timestamp": "2026-08-26T08:00:00Z",
          }))
  assert len(deltas) == 1
  assert deltas[0]["message"]["role"] == "system"
  assert deltas[0]["message"]["content"] == "Posted to Slack: hi there"


# ---------------------------------------------------------------------------
# Round-end audit: deliver_done
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_summon_round_without_a_reply_wakes_the_master_once_with_a_nudge(tmp_path: Path) -> None:
  """The nudge copies the summon's slack block, names the summon, and starts one round bound to itself."""
  cfg, session_mgr, client = _rig(tmp_path)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)
  summon = await _append(session_mgr, sid, _summon())
  await _append(session_mgr, sid, make_text_event("wrote the reply into a shell variable and stopped"))
  done = await _append(session_mgr, sid, _done(summon["id"]))
  tasks: list[asyncio.Task] = []
  trigger = AsyncMock()

  with _listener_seam(client, tasks=tasks, trigger=trigger), capture_logs() as logs:
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
  assert not client.posts
  assert client.reactions[_THREAD] == {"eyes"}  # the question is still open
  assert next(ev for ev in logs if ev["event"] == "slack_reply_nudge")["summon_id"] == summon["id"]


@pytest.mark.asyncio
async def test_notice_post_failure_leaves_no_marker_and_the_boot_audit_posts_it_later(tmp_path: Path) -> None:
  """Post first, mark on success: a failed post leaves the collector a retry instead of a silent thread."""
  cfg, session_mgr, client = _rig(tmp_path, fail_posts=True)
  sid, _summon, nudge = await _unanswered_nudge_round(session_mgr, client)
  done = await _append(session_mgr, sid, _done(nudge["id"]))
  tasks: list[asyncio.Task] = []

  with (
      _listener_seam(client, tasks=tasks),
      patch(_RETRY_DELAYS_PATCH_TARGET, (0.0, 0.0)),
      capture_logs() as logs,
  ):
    assert await deliver_done(sid, done, cfg, session_mgr) is False

  assert not client.posts
  assert not _notices(session_mgr.load_chat_events_sync(sid))
  assert client.reactions[_THREAD] == {"eyes"}
  assert any(ev["event"] == "slack_post_gave_up" for ev in logs)

  client.fail_posts = False  # Slack is back at the next boot
  with _listener_seam(client, tasks=tasks, queued=set()):
    assert await backfill_lost_summons(cfg, session_mgr) == 1
    await asyncio.gather(*tasks)

  assert [p["text"] for p in client.posts] == [_NO_REPLY_NOTICE]
  assert len(_notices(session_mgr.load_chat_events_sync(sid))) == 1
  assert not client.reactions[_THREAD]


# ---------------------------------------------------------------------------
# _chunk_text

# ---------------------------------------------------------------------------
# Boot backfill: lost summons
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_backfill_run_twice_posts_once(tmp_path: Path) -> None:
  cfg, session_mgr, client = _rig(tmp_path)
  sid = await _slack_session(session_mgr)
  await _append(session_mgr, sid, _summon())

  with _listener_seam(client):
    assert await backfill_lost_summons(cfg, session_mgr) == 1
    assert await backfill_lost_summons(cfg, session_mgr) == 0

  assert len(client.posts) == 1
  events = session_mgr.load_chat_events_sync(sid)
  assert len(_of_type(events, ET.ASSISTANT_ERROR)) == 1
  assert not _of_type(events, ET.MASTER_DONE)


# ---------------------------------------------------------------------------
# Boot backfill: the round-end audit over finished rounds

# ---------------------------------------------------------------------------
# Ack reaction lifecycle


def _running_item(
    cfg: CharlieBotConfig, session_mgr: SessionManager, sid: str,
    user_event_id: str | None) -> master_cc_state._WorkItem:
  """A work item as the consumer parks it in ``master_cc_state._current_items`` while a round runs."""
  return master_cc_state._WorkItem(
      cfg=cfg,
      session_meta=SessionMetadata(id=sid, name="slack session"),
      user_content="summon prompt",
      callbacks=session_mgr.callbacks(),
      is_voice=False,
      auto_trigger=False,
      backend_option=None,
      extra_claude_flags=None,
      should_check_tex=False,
      future=asyncio.get_running_loop().create_future(),
      user_event_ids=[user_event_id] if user_event_id else [])


@pytest.mark.asyncio
async def test_batch_holding_two_summons_binds_the_reply_to_the_newer_one(tmp_path: Path) -> None:
  """A merged round answering two Slack summons of the thread binds the reply
  to the newer summon, and both count as answered in the lost-summon check."""
  cfg, session_mgr, client = _rig(tmp_path)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_mgr)
  older = await _append(session_mgr, sid, _summon("older ask"))
  newer = await _append(session_mgr, sid, _summon("newer ask"))

  master_cc_state._current_items[sid] = _running_item(cfg, session_mgr, sid, older["id"])
  master_cc_state._current_items[sid].user_event_ids = [older["id"], newer["id"]]
  try:
    ack_tasks: list[asyncio.Task] = []
    with _listener_seam(client, tasks=ack_tasks):
      result = await post_reply(sid, "one answer for both", cfg, session_mgr)
      await asyncio.gather(*ack_tasks)
  finally:
    master_cc_state._current_items.pop(sid, None)

  assert result["answers"] == newer["id"]
  reply_event = _of_type(session_mgr.load_chat_events_sync(sid), ET.SLACK_REPLY)[0]
  assert reply_event["slack_reply"]["answers"] == newer["id"]

  # Both summons count as answered: one MASTER_DONE carries the whole list.
  done = _done(None)
  done[ET.INPUT_EVENT_IDS] = [older["id"], newer["id"]]
  await _append(session_mgr, sid, done)
  lost = _lost_summons(session_mgr.load_chat_events_sync(sid), owned=set(), running=set())
  assert lost == []
