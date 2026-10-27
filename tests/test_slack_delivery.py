"""Acceptance tests for the Slack reply path, the round-end audit, and the boot backfill (src.features.slack.slack_listener)."""

from __future__ import annotations

import asyncio
import contextlib
import dataclasses
import datetime
import pathlib
from collections.abc import Iterator
from unittest import mock

import conftest
import pytest
from structlog import testing

from src.features.chat_threads import thread_entry
from src.features.slack import event_types, metadata, slack_listener, turn_contribution
from src.infra import config, models
from src.infra import event_types as ET
from src.runtime import master_cc_state, message_aggregator, triggers
from src.runtime.agent_process import base

_CHANNEL = "C_TEST"
_THREAD = "1700000000.000100"
_TEAM = "T_TEST"
_RETRY_DELAYS_PATCH_TARGET = "src.features.chat_threads.thread_entry._RETRY_DELAYS"
_QUEUED_USER_EVENT_IDS_PATCH_TARGET = "src.runtime.master_cc_queue.queued_user_event_ids"
_PERMALINK = "https://fake.slack.test/archives/C_TEST/p1700000000000100"
# The older full-prompt summon format: its content names the reply command, so
# the round-end audit still reads the summon as issued under the current
# contract (the current format is the one line `SLACK.summon_line`).
_SUMMON_CONTENT = f"Slack 线程召唤：{_PERMALINK}\n\nPost the reply with `charliebot slack reply --file <path>`."


def _rig(tmp_path: pathlib.Path,
         *,
         fail_posts: bool = False,
         fail_remove: bool = False) -> tuple[config.CharlieBotConfig, conftest.SessionBlocks, conftest.FakeSlackClient]:
  """Slack rig: cfg and manager rooted at tmp_path, plus a recording fake client."""
  cfg = conftest.build_slack_cfg(tmp_path)
  return cfg, conftest.build_session_blocks(cfg), conftest.FakeSlackClient(
      fail_posts=fail_posts, fail_remove=fail_remove)


@contextlib.contextmanager
def _listener_seam(
    client: conftest.FakeSlackClient,
    *,
    tasks: list[asyncio.Task] | None = None,
    trigger: mock.AsyncMock | None = None,
    queued: set[str] | None = None,
) -> Iterator[None]:
  """Patch the slack_listener module seams; the seam wiring lives here and nowhere else.

  ``_bot_client`` always returns *client*. *tasks* feeds ``create_logged_task``'s
  task spawner, *trigger* replaces ``trigger_master``, *queued* pins
  ``master_cc_queue.queued_user_event_ids``; each stays unpatched when its argument is
  None. Any further patch a test needs (retry delays, log capture) stays visible
  at the call site as a sibling context.
  """
  with contextlib.ExitStack() as stack:
    stack.enter_context(mock.patch(conftest.SLACK_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client))
    if tasks is not None:
      stack.enter_context(
          mock.patch(conftest.TASKS_CREATE_LOGGED_TASK_PATCH_TARGET, side_effect=conftest.make_task_spawner(tasks)))
    if trigger is not None:
      stack.enter_context(mock.patch(conftest.MASTER_TRIGGER_PATCH_TARGET, trigger))
    if queued is not None:
      stack.enter_context(mock.patch(_QUEUED_USER_EVENT_IDS_PATCH_TARGET, return_value=queued))
    yield


async def _slack_session(session_blocks: conftest.SessionBlocks) -> str:
  """Create a Slack-born session and return its id."""
  meta = await conftest.create_root_session(
      session_blocks,
      models.CreateSessionRequest(
          name="slack session",
          slack_origin=metadata.SlackOrigin(team_id=_TEAM, channel_id=_CHANNEL, thread_ts=_THREAD)))
  return meta.id


async def _append(session_blocks: conftest.SessionBlocks, sid: str, event: dict) -> dict:
  """Append one event to the session log (no aggregator, no audit hook) and return it."""
  await session_blocks.events.save_chat_event(sid, event)
  return event


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


async def _unanswered_nudge_round(session_blocks: conftest.SessionBlocks,
                                  client: conftest.FakeSlackClient) -> tuple[str, dict, dict]:
  """A Slack thread re-asked once with no reply yet; returns (sid, summon, nudge).

  The summon round completed and the eyes reaction marks the thread
  awaiting-answer, so the round-end audit and the boot backfill treat the
  nudge round as still open.
  """
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_blocks)
  summon = await _append(session_blocks, sid, _summon())
  await _append(session_blocks, sid, _done(summon["id"]))
  nudge = await _append(session_blocks, sid, _nudge(summon))
  return sid, summon, nudge


def _nudges(events: list[dict]) -> list[dict]:
  return [ev for ev in conftest.events_of_type(events, ET.AGENT_MESSAGE) if "nudge_of" in (ev.get("slack") or {})]


def _notices(events: list[dict]) -> list[dict]:
  return [ev for ev in events if "slack_notice" in ev]


@contextlib.contextmanager
def _running_round(
    cfg: config.CharlieBotConfig, session_blocks: conftest.SessionBlocks, sid: str,
    user_event_id: str | None) -> Iterator[None]:
  """Expose one task manager work item as the currently running round."""
  master_cc_state._current_items[sid] = _running_item(cfg, session_blocks, sid, user_event_id)
  try:
    yield
  finally:
    master_cc_state._current_items.pop(sid, None)


# ---------------------------------------------------------------------------
# Reply: post_reply
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_reply_in_a_summon_round_posts_persists_and_clears_the_eye(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, client = _rig(tmp_path)
  client.reactions[_THREAD] = {"eyes"}  # lit at the summon
  sid = await _slack_session(session_blocks)
  summon = await _append(session_blocks, sid, _summon())

  ack_tasks: list[asyncio.Task] = []
  with _running_round(cfg, session_blocks, sid, summon["id"]), _listener_seam(client, tasks=ack_tasks):
    result = await slack_listener.post_reply(
        sid, "the answer", cfg, session_blocks.store, session_blocks.events, tree=session_blocks.tree)
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
  replies = conftest.events_of_type(session_blocks.events.load_chat_events_sync(sid), event_types.SLACK_REPLY)
  assert len(replies) == 1
  assert replies[0]["content"] == "the answer"
  assert replies[0]["slack_reply"] == {"answers": summon["id"], "chars": 10, "chunks": 1}
  assert client.remove_calls == [{"channel": _CHANNEL, "name": "eyes", "ts": _THREAD}]
  assert not client.reactions[_THREAD]


@pytest.mark.asyncio
async def test_duplicate_slack_app_mention_starts_one_summon_and_one_ack(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, client = _rig(tmp_path)
  tree = conftest.build_task_tree(cfg, session_blocks)
  conftest.bind_deps_blocks(monkeypatch, tree, session_blocks)
  trigger_mgr = triggers.TriggerManager(cfg, tree)
  event = {
      "type": "app_mention",
      "team": _TEAM,
      "channel": _CHANNEL,
      "thread_ts": _THREAD,
      "ts": "1700000000.000321",
      "user": cfg.slack.allowed_user_ids[0],
  }
  name_entered = asyncio.Event()
  both_named = asyncio.Event()
  name_arrivals = 0
  original_name = client.get_channel_name

  async def synchronize_name(channel_id: str) -> str:
    nonlocal name_arrivals
    name_arrivals += 1
    if name_arrivals == 2:
      both_named.set()
    await both_named.wait()
    name_entered.set()
    return await original_name(channel_id)

  client.get_channel_name = synchronize_name
  tasks: list[asyncio.Task] = []
  trigger = mock.AsyncMock()
  with _listener_seam(client, tasks=tasks, trigger=trigger):
    first_pair = await asyncio.gather(
        *[
            slack_listener.handle_app_mention(event, cfg, *conftest.thread_blocks(session_blocks), client, trigger_mgr)
            for _ in range(2)
        ])
    assert name_entered.is_set()
    for _ in range(2):
      assert await slack_listener.handle_app_mention(
          event, cfg, *conftest.thread_blocks(session_blocks), client, trigger_mgr) == first_pair[0]
    await asyncio.gather(*tasks)

  assert first_pair[0] == first_pair[1]
  sid = first_pair[0]
  assert sid is not None
  summons = [ev for ev in tree.fact_history(sid) if ev.get("type") == ET.AGENT_MESSAGE and "slack" in ev]
  assert len(summons) == 1
  assert summons[0]["request_id"] == f"slack-mention:{event['ts']}"
  trigger.assert_awaited_once()
  assert len([call for method, call in client.calls if method == "add_reaction"]) == 1


@pytest.mark.asyncio
async def test_slack_accepted_post_then_read_timeout_is_found_by_key(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, client = _rig(tmp_path)
  sid = await _slack_session(session_blocks)
  client.raise_after_accept_once = True

  with _listener_seam(client):
    result = await slack_listener.post_reply(
        sid,
        "accepted then timed out",
        cfg,
        session_blocks.store,
        session_blocks.events,
        request_id="timeout-request",
        run_started_at=datetime.datetime.now(datetime.UTC),
        tree=session_blocks.tree,
    )

  assert result["posted"] is True
  assert len(client.posts) == 1
  replies = conftest.events_of_type(session_blocks.events.load_chat_events_sync(sid), event_types.SLACK_REPLY)
  assert len(replies) == 1
  assert replies[0]["request_id"] == "timeout-request"


@pytest.mark.asyncio
async def test_slack_retry_after_event_write_failure_finds_existing_post(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, client = _rig(tmp_path)
  sid = await _slack_session(session_blocks)
  original_save = session_blocks.events.save_chat_event
  failed = False

  async def fail_reply_write(session_id: str, event: dict) -> None:
    nonlocal failed
    if not failed and event.get("type") == event_types.SLACK_REPLY:
      failed = True
      raise RuntimeError("injected event write failure")
    await original_save(session_id, event)

  monkeypatch.setattr(session_blocks.events, "save_chat_event", fail_reply_write)
  with _listener_seam(client), pytest.raises(RuntimeError, match="injected event write failure"):
    await slack_listener.post_reply(
        sid,
        "post before event",
        cfg,
        session_blocks.store,
        session_blocks.events,
        request_id="write-request",
        run_started_at=datetime.datetime.now(datetime.UTC),
        tree=session_blocks.tree)

  monkeypatch.setattr(session_blocks.events, "save_chat_event", original_save)
  with _listener_seam(client):
    result = await slack_listener.post_reply(
        sid,
        "post before event",
        cfg,
        session_blocks.store,
        session_blocks.events,
        request_id="write-request",
        run_started_at=datetime.datetime.now(datetime.UTC),
        tree=session_blocks.tree)

  assert result["posted"] is True
  assert len(client.posts) == 1
  replies = conftest.events_of_type(session_blocks.events.load_chat_events_sync(sid), event_types.SLACK_REPLY)
  assert len(replies) == 1
  assert replies[0]["request_id"] == "write-request"


@pytest.mark.asyncio
async def test_slack_partial_segment_retry_posts_only_the_missing_segment(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, client = _rig(tmp_path)
  sid = await _slack_session(session_blocks)
  small_platform = dataclasses.replace(slack_listener.SLACK, max_post_chars=5)
  monkeypatch.setattr(slack_listener.SlackThreadAdapter, "platform", small_platform)
  text = "abcdefghij"
  client.fail_post_keys.add("segment-request#2")

  with _listener_seam(client), mock.patch(_RETRY_DELAYS_PATCH_TARGET,
                                          (0.0, 0.0)), pytest.raises(slack_listener.SlackReplyError) as excinfo:
    await slack_listener.post_reply(
        sid,
        text,
        cfg,
        session_blocks.store,
        session_blocks.events,
        request_id="segment-request",
        run_started_at=datetime.datetime.now(datetime.UTC),
        tree=session_blocks.tree)
  assert excinfo.value.status == 502
  assert len(client.posts) == 1
  assert client.posts[0]["text"] == text[:5]

  client.fail_post_keys.clear()
  with _listener_seam(client):
    result = await slack_listener.post_reply(
        sid,
        text,
        cfg,
        session_blocks.store,
        session_blocks.events,
        request_id="segment-request",
        run_started_at=datetime.datetime.now(datetime.UTC),
        tree=session_blocks.tree)

  assert result["chunks"] == 2
  assert [post["text"] for post in client.posts] == [text[:5], text[5:]]
  assert len(conftest.events_of_type(session_blocks.events.load_chat_events_sync(sid), event_types.SLACK_REPLY)) == 1


@pytest.mark.asyncio
@pytest.mark.parametrize("text", ["", "   \n\t"])
async def test_blank_reply_is_refused_422_before_any_post(tmp_path: pathlib.Path, text: str) -> None:
  cfg, session_blocks, client = _rig(tmp_path)
  sid = await _slack_session(session_blocks)
  with _listener_seam(client), pytest.raises(slack_listener.SlackReplyError) as excinfo:
    await slack_listener.post_reply(
        sid, text, cfg, session_blocks.store, session_blocks.events, tree=session_blocks.tree)
  assert excinfo.value.status == 422
  assert not client.posts
  assert not conftest.events_of_type(session_blocks.events.load_chat_events_sync(sid), event_types.SLACK_REPLY)


@pytest.mark.asyncio
async def test_slack_rejecting_the_post_is_502_and_persists_nothing(tmp_path: pathlib.Path) -> None:
  """The agent learns from the readback and may retry; the log records only replies that landed."""
  cfg, session_blocks, client = _rig(tmp_path, fail_posts=True)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_blocks)
  summon = await _append(session_blocks, sid, _summon())

  with (
      _running_round(cfg, session_blocks, sid, summon["id"]),
      _listener_seam(client),
      mock.patch(_RETRY_DELAYS_PATCH_TARGET, (0.0, 0.0)),
      testing.capture_logs() as logs,
      pytest.raises(slack_listener.SlackReplyError) as excinfo,
  ):
    await slack_listener.post_reply(
        sid, "never arrives", cfg, session_blocks.store, session_blocks.events, tree=session_blocks.tree)

  assert excinfo.value.status == 502
  assert "nothing was persisted" in excinfo.value.detail
  assert not client.posts
  assert not conftest.events_of_type(session_blocks.events.load_chat_events_sync(sid), event_types.SLACK_REPLY)
  assert client.reactions[_THREAD] == {"eyes"}
  assert any(ev["event"] == "slack_post_gave_up" for ev in logs)
  assert not any(ev["event"] == "slack_reply_posted" for ev in logs)


@pytest.mark.asyncio
async def test_reply_to_an_unknown_session_is_404_without_slack_credentials(tmp_path: pathlib.Path) -> None:
  """The refusal precedes any Slack call: a host without Slack credentials gets
  the 404 for an unknown session, not a credentials.slack.bot_token error."""
  cfg, session_blocks, _ = _rig(tmp_path)
  conftest.stub_credentials({})  # no slack section: building the bot client raises

  with pytest.raises(slack_listener.SlackReplyError) as excinfo:
    await slack_listener.post_reply(
        "no-such-session", "hello", cfg, session_blocks.store, session_blocks.events, tree=session_blocks.tree)

  assert excinfo.value.status == 404


# ---------------------------------------------------------------------------
# Reply: the file-server-link check before any chunk posts
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_reply_refuses_as_a_whole_when_it_still_links_the_file_server(tmp_path: pathlib.Path) -> None:
  """A portless CharlieBot file-server link refuses the reply with the publish command that fixes it."""
  cfg, session_blocks, client = _rig(tmp_path)
  sid = await _slack_session(session_blocks)
  link = "https://charliebot.example/absolute_filepath/home/u/artifacts/page.html"

  with _listener_seam(client), pytest.raises(slack_listener.SlackReplyError) as excinfo:
    await slack_listener.post_reply(
        sid, f"details: {link}", cfg, session_blocks.store, session_blocks.events, tree=session_blocks.tree)

  assert excinfo.value.status == 422
  assert link in excinfo.value.detail
  assert "charliebot publish /home/u/artifacts/page.html" in excinfo.value.detail
  assert not client.posts
  assert not conftest.events_of_type(session_blocks.events.load_chat_events_sync(sid), event_types.SLACK_REPLY)


@pytest.mark.asyncio
async def test_reply_with_a_published_url_posts_the_text_as_written(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, client = _rig(tmp_path)
  sid = await _slack_session(session_blocks)
  text = f"see {conftest.PUBLISH_BASE_URL}/Ab3dEf6hIj8kLm1nOp2q/page.html for details"

  with _listener_seam(client):
    result = await slack_listener.post_reply(
        sid, text, cfg, session_blocks.store, session_blocks.events, tree=session_blocks.tree)

  assert result["text"] == text
  assert client.posts == [{"channel": _CHANNEL, "text": text, "thread_ts": _THREAD}]


# ---------------------------------------------------------------------------
# Reply: the internal route
# ---------------------------------------------------------------------------


def test_slack_reply_event_projects_as_a_system_message() -> None:
  """The persisted reply shows in the chat as what was posted, through the existing system role."""
  agg = message_aggregator.MessageAggregator()
  deltas = list(
      agg.feed(
          {
              "type": event_types.SLACK_REPLY,
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
async def test_summon_round_without_a_reply_wakes_the_master_once_with_a_nudge(tmp_path: pathlib.Path) -> None:
  """The nudge copies the summon's slack block, names the summon, and starts one round bound to itself."""
  cfg, session_blocks, client = _rig(tmp_path)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_blocks)
  summon = await _append(session_blocks, sid, _summon())
  await _append(session_blocks, sid, base.make_text_event("wrote the reply into a shell variable and stopped"))
  done = await _append(session_blocks, sid, _done(summon["id"]))
  tasks: list[asyncio.Task] = []
  trigger = mock.AsyncMock()

  with _listener_seam(client, tasks=tasks, trigger=trigger), testing.capture_logs() as logs:
    assert await slack_listener.deliver_done(
        sid, done, cfg, *conftest.thread_blocks(session_blocks), tree=session_blocks.tree) is True
    await asyncio.gather(*tasks)

  nudges = _nudges(session_blocks.events.load_chat_events_sync(sid))
  assert len(nudges) == 1
  nudge = nudges[0]
  assert nudge["type"] == ET.AGENT_MESSAGE
  assert nudge["slack"] == {**summon["slack"], "nudge_of": summon["id"]}
  # The nudge carries the decision only: no link, no reply-command usage.
  assert _PERMALINK not in nudge["content"]
  assert "charliebot slack reply" not in nudge["content"]
  assert "Slack thread: the round answering this mention ended without posting a reply" in nudge["content"]
  trigger.assert_awaited_once()
  assert trigger.await_args.args[:2] == (sid, nudge["content"])
  assert trigger.await_args.kwargs["input_id"] == nudge["id"]
  assert [t.get_name() for t in tasks] == [f"slack-nudge-{sid}"]
  assert not client.posts
  assert client.reactions[_THREAD] == {"eyes"}  # the question is still open
  assert next(ev for ev in logs if ev["event"] == "slack_reply_nudge")["summon_id"] == summon["id"]


@pytest.mark.asyncio
async def test_deliver_done_and_boot_audit_serialize_nudge_then_notice(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, client = _rig(tmp_path)
  tree = conftest.build_task_tree(cfg, session_blocks)
  sid = await _slack_session(session_blocks)
  client.reactions[_THREAD] = {"eyes"}
  summon = await _append(session_blocks, sid, _summon())
  summon_done = await _append(session_blocks, sid, _done(summon["id"]))
  tasks: list[asyncio.Task] = []
  trigger = mock.AsyncMock()

  def running_nudges(session_id: str) -> set[str]:
    return {event["id"] for event in _nudges(tree.fact_history(session_id))}

  with (
      _listener_seam(client, tasks=tasks, trigger=trigger),
      mock.patch("src.runtime.master_cc_state.running_user_event_ids", side_effect=running_nudges),
  ):
    await asyncio.gather(
        slack_listener.deliver_done(sid, summon_done, cfg, *conftest.thread_blocks(session_blocks), tree=tree),
        slack_listener.backfill_lost_summons(
            cfg, session_blocks.listing, *conftest.thread_blocks(session_blocks), tree=tree),
    )
    await asyncio.gather(*tasks)

    nudge = _nudges(tree.fact_history(sid))[0]
    nudge_done = await _append(session_blocks, sid, _done(nudge["id"]))
    await asyncio.gather(
        slack_listener.deliver_done(sid, nudge_done, cfg, *conftest.thread_blocks(session_blocks), tree=tree),
        slack_listener.backfill_lost_summons(
            cfg, session_blocks.listing, *conftest.thread_blocks(session_blocks), tree=tree),
    )
    await asyncio.gather(*tasks)

  events = tree.fact_history(sid)
  nudges = _nudges(events)
  notices = _notices(events)
  assert len(nudges) == len(notices) == 1
  assert nudges[0]["request_id"] == f"slack-nudge:{summon['id']}"
  assert notices[0]["request_id"] == f"slack-notice:{summon['id']}"
  assert trigger.await_count == 1
  assert len(client.posts) == 1
  assert [call["metadata"]["event_payload"]["key"] for method, call in client.calls if method == "post_message"
         ] == [f"slack-notice:{summon['id']}#1"]


@pytest.mark.asyncio
async def test_after_turn_audits_the_round_on_the_bound_blocks_and_persists_the_nudge(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The funnel's hook reads the session's log through the block accessors and writes the nudge event there."""
  cfg, session_blocks, client = _rig(tmp_path)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_blocks)
  summon = await _append(session_blocks, sid, _summon())
  done = await _append(session_blocks, sid, _done(summon["id"]))
  conftest.bind_deps_blocks(monkeypatch, session_blocks.tree, session_blocks)
  meta = await session_blocks.store.get_session(sid)
  tasks: list[asyncio.Task] = []

  with _listener_seam(client, tasks=tasks, trigger=mock.AsyncMock()):
    await turn_contribution.SlackTurnContribution().after_turn(meta, done, cfg=cfg)
    await asyncio.gather(*tasks)

  nudges = _nudges(session_blocks.events.load_chat_events_sync(sid))
  assert [nudge["slack"] for nudge in nudges] == [{**summon["slack"], "nudge_of": summon["id"]}]


@pytest.mark.asyncio
async def test_notice_post_failure_leaves_no_marker_and_the_boot_audit_posts_it_later(tmp_path: pathlib.Path) -> None:
  """Post first, mark on success: a failed post leaves the collector a retry instead of a silent thread."""
  cfg, session_blocks, client = _rig(tmp_path, fail_posts=True)
  sid, _summon, nudge = await _unanswered_nudge_round(session_blocks, client)
  done = await _append(session_blocks, sid, _done(nudge["id"]))
  tasks: list[asyncio.Task] = []

  with (
      _listener_seam(client, tasks=tasks),
      mock.patch(_RETRY_DELAYS_PATCH_TARGET, (0.0, 0.0)),
      testing.capture_logs() as logs,
  ):
    assert await slack_listener.deliver_done(
        sid, done, cfg, *conftest.thread_blocks(session_blocks), tree=session_blocks.tree) is False

  assert not client.posts
  assert not _notices(session_blocks.events.load_chat_events_sync(sid))
  assert client.reactions[_THREAD] == {"eyes"}
  assert any(ev["event"] == "slack_post_gave_up" for ev in logs)

  client.fail_posts = False  # Slack is back at the next boot
  with _listener_seam(client, tasks=tasks, queued=set()):
    assert await slack_listener.backfill_lost_summons(
        cfg, session_blocks.listing, *conftest.thread_blocks(session_blocks), tree=session_blocks.tree) == 1
    await asyncio.gather(*tasks)

  assert [p["text"] for p in client.posts] == [thread_entry._NO_REPLY_NOTICE]
  assert len(_notices(session_blocks.events.load_chat_events_sync(sid))) == 1
  assert not client.reactions[_THREAD]


@pytest.mark.asyncio
async def test_round_end_without_a_slack_thread_answers_false_without_slack_credentials(tmp_path: pathlib.Path) -> None:
  """persist_and_broadcast fires deliver_done for every master_done: a host
  without Slack credentials survives a non-Slack session's round end — the
  audit answers False and never builds the bot client."""
  cfg, session_blocks, _ = _rig(tmp_path)
  conftest.stub_credentials({})  # no slack section: building the bot client raises
  meta = await conftest.create_root_session(session_blocks, models.CreateSessionRequest(name="plain session"))

  assert await slack_listener.deliver_done(
      meta.id, _done(None), cfg, *conftest.thread_blocks(session_blocks), tree=session_blocks.tree) is False


# ---------------------------------------------------------------------------
# Boot backfill: lost summons
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_backfill_run_twice_posts_once(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, client = _rig(tmp_path)
  sid = await _slack_session(session_blocks)
  summon = await _append(session_blocks, sid, _summon())

  with _listener_seam(client):
    assert await slack_listener.backfill_lost_summons(
        cfg, session_blocks.listing, *conftest.thread_blocks(session_blocks), tree=session_blocks.tree) == 1
    assert await slack_listener.backfill_lost_summons(
        cfg, session_blocks.listing, *conftest.thread_blocks(session_blocks), tree=session_blocks.tree) == 0

  assert len(client.posts) == 1
  events = session_blocks.events.load_chat_events_sync(sid)
  markers = conftest.events_of_type(events, ET.ASSISTANT_ERROR)
  assert len(markers) == 1
  assert markers[0]["request_id"] == f"slack-lost:{summon['id']}"
  assert [call["metadata"]["event_payload"]["key"] for method, call in client.calls if method == "post_message"
         ] == [f"slack-lost:{summon['id']}#1"]
  assert not conftest.events_of_type(events, ET.MASTER_DONE)


# ---------------------------------------------------------------------------
# Ack reaction lifecycle


def _running_item(
    cfg: config.CharlieBotConfig, session_blocks: conftest.SessionBlocks, sid: str,
    user_event_id: str | None) -> master_cc_state._WorkItem:
  """A work item as the consumer parks it in ``master_cc_state._current_items`` while a round runs."""
  return conftest.make_work_item(
      cfg,
      models.SessionMetadata(id=sid, name="slack session", profile="manager"),
      None,
      user_content="summon prompt",
      callbacks=session_blocks.callbacks(),
      user_event_id=user_event_id)


@pytest.mark.asyncio
async def test_batch_holding_two_summons_binds_the_reply_to_the_newer_one(tmp_path: pathlib.Path) -> None:
  """A merged round answering two Slack summons of the thread binds the reply
  to the newer summon, and both count as answered in the lost-summon check."""
  cfg, session_blocks, client = _rig(tmp_path)
  client.reactions[_THREAD] = {"eyes"}
  sid = await _slack_session(session_blocks)
  older = await _append(session_blocks, sid, _summon("older ask"))
  newer = await _append(session_blocks, sid, _summon("newer ask"))

  master_cc_state._current_items[sid] = _running_item(cfg, session_blocks, sid, older["id"])
  master_cc_state._current_items[sid].user_event_ids = [older["id"], newer["id"]]
  try:
    ack_tasks: list[asyncio.Task] = []
    with _listener_seam(client, tasks=ack_tasks):
      result = await slack_listener.post_reply(
          sid, "one answer for both", cfg, session_blocks.store, session_blocks.events, tree=session_blocks.tree)
      await asyncio.gather(*ack_tasks)
  finally:
    master_cc_state._current_items.pop(sid, None)

  assert result["answers"] == newer["id"]
  reply_event = conftest.events_of_type(session_blocks.events.load_chat_events_sync(sid), event_types.SLACK_REPLY)[0]
  assert reply_event["slack_reply"]["answers"] == newer["id"]

  # Both summons count as answered: one MASTER_DONE carries the whole list.
  done = _done(None)
  done[ET.INPUT_EVENT_IDS] = [older["id"], newer["id"]]
  await _append(session_blocks, sid, done)
  lost = thread_entry.lost_summons(
      slack_listener.SLACK, session_blocks.events.load_chat_events_sync(sid), owned=set(), running=set())
  assert lost == []
