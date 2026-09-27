"""Acceptance tests for Slack thread follow: guard chain, trigger arming, gate, ack, backfill."""

from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from conftest import (
    SLACK_LISTENER_BOT_CLIENT_PATCH_TARGET,
    TRIGGER_MASTER_PATCH_TARGET,
    TRIGGERS_GET_CONFIG_PATCH_TARGET,
    FakeSlackClient,
    build_slack_cfg,
    make_internal_router_client,
)

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
    assert_thread_fresh,
    handle_thread_message,
    summon_session_id,
)
from src.core.triggers import TriggerManager

_TEAM = "T_TEST"
_CHANNEL = "C_TEST"
_ROOT = "1700000000.000100"  # the summon ts; the thread's root
_MENTION_ERA_WATERMARK = "1700000000.000050"


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


def _thread_message(seq: int, text: str, user: str = "U_ALLOWED", bot: bool = False) -> dict:
  message: dict = {"user": user, "ts": _ts(seq), "text": text}
  if bot:
    message["bot_id"] = "B_TEST"
    del message["user"]
  return message


def _rig(tmp_path: Path) -> tuple:
  """Slack rig: cfg and managers rooted at tmp_path, a recording fake client."""
  cfg = build_slack_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  return cfg, session_mgr, TriggerManager(cfg, session_mgr), FakeSlackClient()


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


# ---------------------------------------------------------------------------
# Schema
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_watermark_persists_through_metadata_json(tmp_path: Path) -> None:
  cfg, session_mgr, _trigger_mgr, _client = _rig(tmp_path)
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


# ---------------------------------------------------------------------------
# Arming: coalescing and the flush cap


# ---------------------------------------------------------------------------
# Mention pairing dedup, both delivery orders


# ---------------------------------------------------------------------------
# Reply gate and ack
# ---------------------------------------------------------------------------


def _seed_gate_thread(client: FakeSlackClient) -> None:
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
  cfg, session_mgr, _trigger_mgr, client = _rig(tmp_path)
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
async def test_gated_route_412_then_ack_then_reply_posts(tmp_path: Path) -> None:
  cfg, session_mgr, _trigger_mgr, client = _rig(tmp_path)
  meta = await _make_session(session_mgr)
  _seed_gate_thread(client)

  with (
      patch(SLACK_LISTENER_BOT_CLIENT_PATCH_TARGET, return_value=client),
      make_internal_router_client(cfg, session_mgr) as http,
  ):
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

  mock_trigger_master.assert_called_once()
  stored = await trigger_mgr._load_trigger(meta.id, armed.id)
  assert stored.status == TriggerStatus.FIRED
  wakes = [ev for ev in session_mgr.load_chat_events_sync(meta.id) if ev.get("type") == ET.SCHEDULED_TRIGGER]
  assert len(wakes) == 1
  _shut_down(boot_mgr)


# ---------------------------------------------------------------------------
# CLI
