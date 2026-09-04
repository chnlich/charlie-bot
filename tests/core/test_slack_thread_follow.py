"""Acceptance tests for Slack thread follow: guard chain, trigger arming, gate, ack, backfill."""

from __future__ import annotations

import asyncio
import json
from datetime import timedelta
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
import requests
from conftest import (
    CLI_COMMON_GET_CONFIG_PATCH_TARGET,
    CLI_COMMON_REQUESTS_POST_PATCH_TARGET,
    SLACK_LISTENER_BOT_CLIENT_PATCH_TARGET,
    SLACK_LISTENER_CREATE_LOGGED_TASK_PATCH_TARGET,
    SLACK_LISTENER_TRIGGER_MASTER_PATCH_TARGET,
    TRIGGER_MASTER_PATCH_TARGET,
    TRIGGERS_GET_CONFIG_PATCH_TARGET,
    build_slack_cfg,
    make_json_response,
    make_task_spawner,
    setup_session_cwd,
)
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import internal
from src.api.deps import get_session_manager
from src.api.internal import router as internal_router
from src.cli.slack import main as cli_main
from src.core import event_types as ET
from src.core.models import (
    CreateSessionRequest,
    PendingTrigger,
    SessionMetadata,
    SlackOrigin,
    TriggerStatus,
    utc_now,
)
from src.core.sessions import SessionManager
from src.core.slack_listener import (
    SlackReplyError,
    _arm_follow_trigger,
    _backfill_followed_threads,
    _build_follow_wake_message,
    ack_messages,
    assert_thread_fresh,
    handle_app_mention,
    handle_thread_message,
    summon_session_id,
)
from src.core.triggers import TriggerManager

_TEAM = "T_TEST"
_CHANNEL = "C_TEST"
_ROOT = "1700000000.000100"  # the summon ts; the thread's root
_MENTION_ERA_WATERMARK = "1700000000.000050"
_PERMALINK = f"https://fake.slack.test/archives/{_CHANNEL}/p{_ROOT}"


def _ts(seq: int) -> str:
  """A Slack-style ts string ordered by seq (slack ts strings compare lexicographically)."""
  return f"1700000000.{seq:06d}"


def _message_event(**overrides: object) -> dict:
  """An eligible thread message event from the allowed user, merging in per-test overrides."""
  base: dict = {
      "type": "message",
      "user": "U_ALLOWED",
      "team": _TEAM,
      "channel": _CHANNEL,
      "thread_ts": _ROOT,
      "ts": _ts(150),
      "text": "follow up",
  }
  base.update(overrides)
  return base


def _mention_event(ts: str) -> dict:
  return {
      "type": "app_mention",
      "user": "U_ALLOWED",
      "team": _TEAM,
      "channel": _CHANNEL,
      "thread_ts": _ROOT,
      "ts": ts,
      "text": "hey @charliebot",
      "channel_type": "channel",
  }


def _thread_message(seq: int, text: str, user: str = "U_ALLOWED", bot: bool = False) -> dict:
  message: dict = {"user": user, "ts": _ts(seq), "text": text}
  if bot:
    message["bot_id"] = "B_TEST"
    del message["user"]
  return message


class _FakeSlackClient:
  """Records calls; serves a seedable thread and fabricates permalinks; never touches the network."""

  def __init__(self) -> None:
    self.thread: list[dict] = []
    self.reply_calls = 0
    self.posts: list[dict] = []
    self.reactions: list[dict] = []

  async def get_permalink(self, channel: str, ts: str) -> str:
    return f"https://fake.slack.test/archives/{channel}/p{ts}"

  async def get_channel_name(self, channel_id: str) -> str | None:
    return f"name-of-{channel_id}"

  async def get_thread_replies(self, channel: str, thread_ts: str) -> list[dict]:
    self.reply_calls += 1
    return list(self.thread)

  async def post_message(self, channel: str, text: str, thread_ts: str | None = None) -> dict:
    self.posts.append({"channel": channel, "text": text, "thread_ts": thread_ts})
    return {"ok": True}

  async def add_reaction(self, channel: str, name: str, ts: str) -> dict:
    self.reactions.append({"channel": channel, "name": name, "ts": ts})
    return {"ok": True}

  async def remove_reaction(self, channel: str, name: str, ts: str) -> dict:
    return {"ok": True}


def _rig(tmp_path: Path) -> tuple:
  """Slack rig: cfg and managers rooted at tmp_path, a recording fake client."""
  cfg = build_slack_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  return cfg, session_mgr, TriggerManager(cfg, session_mgr), _FakeSlackClient()


def _shut_down(trigger_mgr: TriggerManager) -> None:
  """Cancel every sleeping trigger task; persisted records are untouched."""
  for task in list(trigger_mgr._tasks.values()):
    task.cancel()


async def _make_session(
    session_mgr: SessionManager,
    watermark: str | None = _MENTION_ERA_WATERMARK,
    thread_ts: str = _ROOT,
) -> SessionMetadata:
  """Create one Slack thread's deterministic session and (unless None) stamp its watermark."""
  meta = await session_mgr.create_session(
      CreateSessionRequest(
          session_id=summon_session_id(_TEAM, _CHANNEL, thread_ts),
          name="slack session",
          slack_origin=SlackOrigin(team_id=_TEAM, channel_id=_CHANNEL, thread_ts=thread_ts)))
  if watermark is not None:
    meta.slack_watermark_ts = watermark
    await session_mgr.save_metadata(meta)
  return meta


async def _armed(trigger_mgr: TriggerManager, session_id: str) -> list[PendingTrigger]:
  return [
      t for t in await trigger_mgr.list_triggers(session_id)
      if t.status == TriggerStatus.PENDING and t.message.startswith("slack-thread-follow")
  ]


async def _handle_mention(cfg, session_mgr, trigger_mgr, client, ts: str) -> str:
  """Run the summon path for one mention (all round machinery mocked out); return the session id."""
  tasks: list[asyncio.Task] = []
  with (
      patch(SLACK_LISTENER_TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()),
      patch(SLACK_LISTENER_CREATE_LOGGED_TASK_PATCH_TARGET, side_effect=make_task_spawner(tasks)),
  ):
    sid = await handle_app_mention(_mention_event(ts), cfg, session_mgr, client, trigger_mgr)
    await asyncio.gather(*tasks)
  assert sid is not None
  return sid


# ---------------------------------------------------------------------------
# Schema
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_watermark_persists_through_metadata_json(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, _client = _rig(tmp_path)
  meta = await _make_session(session_mgr)
  assert meta.slack_watermark_ts == _MENTION_ERA_WATERMARK
  reloaded = await SessionManager(cfg).get_session(meta.id)
  assert reloaded is not None
  assert reloaded.slack_watermark_ts == _MENTION_ERA_WATERMARK


# ---------------------------------------------------------------------------
# Guard chain
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "case", [
        "edit_subtype",
        "delete_subtype",
        "bot_authored",
        "disallowed_user",
        "archived_session",
        "at_watermark",
        "below_watermark",
    ])
async def test_thread_message_guard_chain_drops(tmp_path: Path, case: str) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(tmp_path)
  meta = await _make_session(session_mgr)
  if case == "archived_session":
    await session_mgr.archive_session(meta.id)
  overrides: dict = {"ts": _ts(150)}
  if case == "edit_subtype":
    overrides["subtype"] = "message_changed"
  elif case == "delete_subtype":
    overrides["subtype"] = "message_deleted"
  elif case == "bot_authored":
    overrides["bot_id"] = "B_X"
  elif case == "disallowed_user":
    overrides["user"] = "U_OTHER"
  elif case == "at_watermark":
    overrides["ts"] = _MENTION_ERA_WATERMARK
  elif case == "below_watermark":
    overrides["ts"] = _ts(40)

  sid = await handle_thread_message(_message_event(**overrides), cfg, session_mgr, client, trigger_mgr)

  assert sid is None
  assert await _armed(trigger_mgr, meta.id) == []
  _shut_down(trigger_mgr)


@pytest.mark.asyncio
@pytest.mark.parametrize("watermark", [None, _MENTION_ERA_WATERMARK])
async def test_eligible_thread_message_arms_the_follow_trigger(tmp_path: Path, watermark: str | None) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(tmp_path)
  meta = await _make_session(session_mgr, watermark=watermark)

  sid = await handle_thread_message(_message_event(), cfg, session_mgr, client, trigger_mgr)

  assert sid == meta.id
  armed = await _armed(trigger_mgr, meta.id)
  assert len(armed) == 1
  assert f"floor={_ts(150)}" in armed[0].message
  _shut_down(trigger_mgr)


@pytest.mark.asyncio
async def test_arm_follow_trigger_on_archived_session_returns_without_a_record(tmp_path: Path) -> None:
  """The create-time rejection stops the thread-follow when its session is archived:
  the re-arm logs the refusal and returns without a new trigger record."""
  cfg, session_mgr, trigger_mgr, _client = _rig(tmp_path)
  meta = await _make_session(session_mgr, watermark=None)
  await session_mgr.archive_session(meta.id)

  trigger = await _arm_follow_trigger(trigger_mgr, meta.id, _CHANNEL, _ROOT, _PERMALINK, _ts(150))

  assert trigger is None
  assert await trigger_mgr.list_triggers(meta.id) == []
  _shut_down(trigger_mgr)


# ---------------------------------------------------------------------------
# Arming: coalescing and the flush cap
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_five_messages_coalesce_into_one_wake_at_the_oldest_floor(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(tmp_path)
  meta = await _make_session(session_mgr)

  with (
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_trigger_master,
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ):
    for seq in (151, 152, 153, 154, 155):
      sid = await handle_thread_message(_message_event(ts=_ts(seq)), cfg, session_mgr, client, trigger_mgr)
      assert sid == meta.id
      assert len(await _armed(trigger_mgr, meta.id)) == 1  # cancel-then-create each time

    armed = (await _armed(trigger_mgr, meta.id))[0]
    assert f"floor={_ts(151)}" in armed.message
    assert f"/p{_ROOT}" in armed.message  # the label points at the thread permalink

    # Fire the armed record now and assert the wake lands exactly once.
    armed.fire_at = utc_now()
    await trigger_mgr._save_trigger(armed)
    await trigger_mgr._wait_and_fire(armed)

  events = session_mgr.load_chat_events_sync(meta.id)
  wakes = [ev for ev in events if ev.get("type") == ET.SCHEDULED_TRIGGER]
  assert len(wakes) == 1
  assert "slack" not in wakes[0]
  assert f"floor={_ts(151)}" in wakes[0]["content"]
  mock_trigger_master.assert_awaited_once()
  stored = await trigger_mgr._load_trigger(meta.id, armed.id)
  assert stored.status == TriggerStatus.FIRED
  _shut_down(trigger_mgr)


@pytest.mark.asyncio
async def test_steady_trickle_flush_stays_pinned_at_chain_start_plus_cap(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(tmp_path)
  meta = await _make_session(session_mgr)
  # The chain started 280s ago: a message every 60s has kept re-arming it.
  chain_start = utc_now() - timedelta(seconds=280)
  legacy = PendingTrigger(
      session_id=meta.id,
      fire_at=utc_now() + timedelta(seconds=45),
      message=_build_follow_wake_message(_ts(151), _PERMALINK),
      created_at=chain_start,
  )
  await trigger_mgr._save_trigger(legacy)

  await handle_thread_message(_message_event(ts=_ts(156)), cfg, session_mgr, client, trigger_mgr)
  first = (await _armed(trigger_mgr, meta.id))[0]
  # The re-armed record inherits the chain's start; the flush fires at chain_start + 300s.
  assert first.created_at == chain_start
  assert 5 <= (first.fire_at - utc_now()).total_seconds() <= 25
  assert f"floor={_ts(151)}" in first.message

  # The next trickle message re-arms to the same deadline instead of sliding forward.
  await handle_thread_message(_message_event(ts=_ts(157)), cfg, session_mgr, client, trigger_mgr)
  armed = await _armed(trigger_mgr, meta.id)
  assert len(armed) == 1
  assert armed[0].created_at == chain_start
  cap_deadline = chain_start + timedelta(seconds=300)
  assert abs((armed[0].fire_at - cap_deadline).total_seconds()) < 5
  assert abs((first.fire_at - cap_deadline).total_seconds()) < 5
  _shut_down(trigger_mgr)


# ---------------------------------------------------------------------------
# Mention pairing dedup, both delivery orders
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_mention_first_then_message_event_drops_the_duplicate(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(tmp_path)
  meta = await _make_session(session_mgr)
  sid = await _handle_mention(cfg, session_mgr, trigger_mgr, client, _ts(100))
  assert sid == meta.id

  # The sibling message event of the same @ lands at or below the fresh watermark.
  dropped = await handle_thread_message(_message_event(ts=_ts(100)), cfg, session_mgr, client, trigger_mgr)

  assert dropped is None
  assert await _armed(trigger_mgr, meta.id) == []
  reloaded = await session_mgr.get_session(meta.id)
  assert reloaded is not None and reloaded.slack_watermark_ts == _ts(100)
  _shut_down(trigger_mgr)


@pytest.mark.asyncio
async def test_message_event_first_then_mention_cancels_the_armed_trigger(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(tmp_path)
  meta = await _make_session(session_mgr)

  sid = await handle_thread_message(_message_event(ts=_ts(100)), cfg, session_mgr, client, trigger_mgr)
  assert sid == meta.id
  assert len(await _armed(trigger_mgr, meta.id)) == 1

  summoned = await _handle_mention(cfg, session_mgr, trigger_mgr, client, _ts(100))

  assert summoned == meta.id
  assert await _armed(trigger_mgr, meta.id) == []
  reloaded = await session_mgr.get_session(meta.id)
  assert reloaded is not None and reloaded.slack_watermark_ts == _ts(100)
  _shut_down(trigger_mgr)


# ---------------------------------------------------------------------------
# Reply gate and ack
# ---------------------------------------------------------------------------


def _seed_gate_thread(client: _FakeSlackClient) -> None:
  """One unread-eligible pair around noise that must not count."""
  client.thread = [
      {
          "user": "U_ALLOWED",
          "ts": _MENTION_ERA_WATERMARK,
          "text": "the summon itself"
      },
      _thread_message(110, "first follow up"),
      _thread_message(115, "bot noise", bot=True),
      _thread_message(120, "lurker", user="U_OTHER"),
      _thread_message(130, "second follow up"),
  ]


@pytest.mark.asyncio
async def test_reply_gate_refuses_the_stale_thread_and_persists_nothing(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(tmp_path)
  meta = await _make_session(session_mgr)
  _seed_gate_thread(client)

  with (
      patch(SLACK_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client),
      pytest.raises(SlackReplyError) as excinfo,
  ):
    await assert_thread_fresh(meta.id, cfg, session_mgr)

  assert excinfo.value.status == 412
  payload = excinfo.value.detail
  assert payload["error"] == "stale_thread"
  assert payload["watermark_ts"] == _MENTION_ERA_WATERMARK
  assert [m["ts"] for m in payload["new_messages"]] == [_ts(110), _ts(130)]
  assert payload["new_messages"][0] == {"ts": _ts(110), "user": "U_ALLOWED", "text_preview": "first follow up"}
  assert client.posts == []
  assert not [ev for ev in session_mgr.load_chat_events_sync(meta.id) if ev.get("type") == ET.SLACK_REPLY]


@pytest.mark.asyncio
async def test_ack_completeness_advance_and_idempotence(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(tmp_path)
  meta = await _make_session(session_mgr)
  _seed_gate_thread(client)

  with patch(SLACK_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client):
    # A skip attempt names the jumped-over unread id and persists nothing.
    with pytest.raises(SlackReplyError) as excinfo:
      await ack_messages(meta.id, [_ts(130)], cfg, session_mgr)
    assert excinfo.value.status == 422
    assert _ts(110) in str(excinfo.value.detail)
    with pytest.raises(SlackReplyError) as excinfo:
      await ack_messages(meta.id, [_ts(110), _ts(999)], cfg, session_mgr)
    assert excinfo.value.status == 422
    assert _ts(999) in str(excinfo.value.detail)
    reloaded = await session_mgr.get_session(meta.id)
    assert reloaded is not None and reloaded.slack_watermark_ts == _MENTION_ERA_WATERMARK

    # The full unread set advances the watermark and persists the audit record.
    result = await ack_messages(meta.id, [_ts(110), _ts(130)], cfg, session_mgr)

  assert result == {"acked": 2, "watermark_ts": _ts(130)}
  acks = [ev for ev in session_mgr.load_chat_events_sync(meta.id) if "slack_ack" in ev]
  assert len(acks) == 1
  assert acks[0]["slack_ack"] == {"message_ids": [_ts(110), _ts(130)], "watermark_ts": _ts(130)}

  with patch(SLACK_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client):
    # Re-acking at or below the watermark is an idempotent no-op counted as acked.
    assert await ack_messages(meta.id, [_ts(110)], cfg, session_mgr) == {"acked": 1, "watermark_ts": _ts(130)}
    reloaded = await session_mgr.get_session(meta.id)
    assert reloaded is not None and reloaded.slack_watermark_ts == _ts(130)
    # And once the thread is acked through, the gate passes.
    await assert_thread_fresh(meta.id, cfg, session_mgr)


def _route_client(cfg, session_mgr: SessionManager) -> TestClient:
  app = FastAPI()
  app.include_router(internal_router, prefix="/api/internal")
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[internal.get_config] = lambda: cfg
  return TestClient(app)


@pytest.mark.asyncio
async def test_gated_route_412_then_ack_then_reply_posts(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(tmp_path)
  meta = await _make_session(session_mgr)
  _seed_gate_thread(client)

  with patch(SLACK_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client), _route_client(cfg, session_mgr) as http:
    resp = http.post("/api/internal/slack/reply", json={"session_id": meta.id, "text": "the answer"})
    assert resp.status_code == 412
    body = resp.json()["detail"]
    assert body["error"] == "stale_thread"
    assert [m["ts"] for m in body["new_messages"]] == [_ts(110), _ts(130)]
    assert client.posts == []

    ack = http.post("/api/internal/slack/ack", json={"session_id": meta.id, "message_ids": [_ts(110), _ts(130)]})
    assert ack.status_code == 200
    assert ack.json() == {"acked": 2, "watermark_ts": _ts(130)}

    resp = http.post("/api/internal/slack/reply", json={"session_id": meta.id, "text": "the answer"})
    assert resp.status_code == 200
    assert resp.json()["posted"] is True

  assert [p["text"] for p in client.posts] == ["the answer"]
  assert len([ev for ev in session_mgr.load_chat_events_sync(meta.id) if ev.get("type") == ET.SLACK_REPLY]) == 1


@pytest.mark.asyncio
async def test_ack_route_maps_refusals(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(tmp_path)
  meta = await _make_session(session_mgr)
  plain = await session_mgr.create_session(CreateSessionRequest(name="browser session"))
  _seed_gate_thread(client)

  with patch(SLACK_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client), _route_client(cfg, session_mgr) as http:
    resp = http.post("/api/internal/slack/ack", json={"session_id": "no-such", "message_ids": [_ts(110)]})
    assert resp.status_code == 404
    resp = http.post("/api/internal/slack/ack", json={"session_id": plain.id, "message_ids": [_ts(110)]})
    assert resp.status_code == 409
    resp = http.post("/api/internal/slack/ack", json={"session_id": meta.id, "message_ids": [_ts(130)]})
    assert resp.status_code == 422
    assert _ts(110) in resp.json()["detail"]


# ---------------------------------------------------------------------------
# Restart and reconnect
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_armed_follow_trigger_rehydrates_and_fires_after_restart(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(tmp_path)
  meta = await _make_session(session_mgr)
  await handle_thread_message(_message_event(), cfg, session_mgr, client, trigger_mgr)
  armed = (await _armed(trigger_mgr, meta.id))[0]

  # The process dies: in-memory sleep tasks vanish; the record stays PENDING.
  _shut_down(trigger_mgr)

  # After restart the boot scan picks the record up; its deadline passed during
  # the outage, so the rehydrated task fires without sleeping.
  armed.fire_at = utc_now()
  await trigger_mgr._save_trigger(armed)
  boot_mgr = TriggerManager(cfg, session_mgr)
  with (
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_trigger_master,
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ):
    await boot_mgr.recover_pending()
    tasks = list(boot_mgr._tasks.values())
    assert len(tasks) == 1
    await asyncio.gather(*tasks)

  mock_trigger_master.assert_awaited_once()
  stored = await trigger_mgr._load_trigger(meta.id, armed.id)
  assert stored.status == TriggerStatus.FIRED
  wakes = [ev for ev in session_mgr.load_chat_events_sync(meta.id) if ev.get("type") == ET.SCHEDULED_TRIGGER]
  assert len(wakes) == 1
  _shut_down(boot_mgr)


@pytest.mark.asyncio
async def test_reconnect_backfill_arms_once_per_followed_thread(tmp_path: Path) -> None:
  cfg, session_mgr, trigger_mgr, client = _rig(tmp_path)
  meta = await _make_session(session_mgr)
  _seed_gate_thread(client)
  archived = await _make_session(session_mgr, thread_ts=_ts(900))
  await session_mgr.archive_session(archived.id)
  plain = await session_mgr.create_session(CreateSessionRequest(name="browser session"))

  armed = await _backfill_followed_threads(cfg, session_mgr, client, trigger_mgr)

  assert armed == 1
  triggers = await _armed(trigger_mgr, meta.id)
  assert len(triggers) == 1
  assert f"floor={_ts(110)}" in triggers[0].message
  assert client.reply_calls == 1  # one conversations.replies per followed thread, not per message

  # A second reconnect re-arms through the same path: still exactly one pending trigger.
  assert await _backfill_followed_threads(cfg, session_mgr, client, trigger_mgr) == 1
  assert len(await _armed(trigger_mgr, meta.id)) == 1
  assert client.reply_calls == 2
  assert await _armed(trigger_mgr, archived.id) == []
  assert await _armed(trigger_mgr, plain.id) == []
  _shut_down(trigger_mgr)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def test_cli_ack_posts_the_ids_and_prints_the_readback(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = setup_session_cwd(tmp_path, monkeypatch, "abc")
  readback = {"acked": 2, "watermark_ts": _ts(130)}
  with (
      patch("sys.argv", ["slack", "ack", "--message-id", _ts(110), _ts(130)]),
      patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg),
      patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET, return_value=make_json_response(readback)) as post,
  ):
    cli_main()

  assert post.call_args.args[0].endswith("/api/internal/slack/ack")
  assert post.call_args.kwargs["json"] == {"session_id": "abc", "message_ids": [_ts(110), _ts(130)]}
  out = capsys.readouterr().out
  assert out.count("\n") == 1
  assert json.loads(out) == readback


def test_cli_reply_412_refusal_exits_nonzero_with_the_payload_on_stderr(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = setup_session_cwd(tmp_path, monkeypatch, "abc")
  reply_file = tmp_path / "reply.md"
  reply_file.write_text("the answer", encoding="utf-8")
  refusal_payload = {
      "error": "stale_thread",
      "new_messages": [{
          "ts": _ts(110),
          "user": "U_ALLOWED",
          "text_preview": "first follow up"
      }],
      "watermark_ts": _MENTION_ERA_WATERMARK,
  }
  refusal = MagicMock()
  refusal.status_code = 412
  refusal.json.return_value = {"detail": refusal_payload}
  refusal.raise_for_status.side_effect = requests.HTTPError(response=refusal)
  with (
      patch("sys.argv", ["slack", "reply", "--file", str(reply_file)]),
      patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg),
      patch("src.cli.common._maybe_version_skew_hint", return_value=None),
      patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET, return_value=refusal),
      pytest.raises(SystemExit) as exc_info,
  ):
    cli_main()

  assert exc_info.value.code != 0
  captured = capsys.readouterr()
  assert captured.out == ""
  assert json.loads(captured.err)["error"] == refusal_payload
