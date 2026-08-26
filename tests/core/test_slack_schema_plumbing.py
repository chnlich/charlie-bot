"""Acceptance tests for the Slack summon schema plumbing (config, session schema, master_done)."""

from __future__ import annotations

import uuid
from pathlib import Path
from typing import Optional
from unittest.mock import MagicMock

import pytest
from conftest import make_work_item, mock_session_callbacks, run_session_consumer

from src.agents import master_cc
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import (
  CreateSessionRequest,
  SessionMetadata,
  SlackOrigin,
)
from src.core.sessions import SessionManager


def test_config_without_slack_keys_yields_defaults() -> None:
  cfg = CharlieBotConfig.model_validate({})
  assert cfg.slack_bot_token is None
  assert cfg.slack_app_token is None
  assert cfg.slack_allowed_user_ids == []


def test_config_round_trips_slack_keys() -> None:
  cfg = CharlieBotConfig.model_validate({
      "slack_bot_token": "test-bot-token",
      "slack_app_token": "test-app-token",
      "slack_allowed_user_ids": ["U_TEST"],
  })
  assert cfg.slack_bot_token == "test-bot-token"
  assert cfg.slack_app_token == "test-app-token"
  assert cfg.slack_allowed_user_ids == ["U_TEST"]


@pytest.mark.asyncio
async def test_create_session_accepts_caller_supplied_id_and_slack_origin(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  origin = SlackOrigin(team_id="T_TEST", channel_id="C_TEST", thread_ts="1700000000.000100")
  meta = await SessionManager(cfg).create_session(
      CreateSessionRequest(session_id="fixed-id-0001", slack_origin=origin))
  assert meta.id == "fixed-id-0001"
  assert meta.slack_origin == origin

  # Re-read through a fresh manager so the value comes from disk, not the cache.
  reloaded = await SessionManager(cfg).get_session("fixed-id-0001")
  assert reloaded is not None
  assert reloaded.slack_origin == origin


@pytest.mark.asyncio
async def test_create_session_defaults_still_generate_uuid4_and_no_origin(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  meta = await SessionManager(cfg).create_session(CreateSessionRequest(name="t"))
  parsed = uuid.UUID(meta.id)
  assert parsed.version == 4
  assert meta.slack_origin is None


async def _run_one_round(user_event_id: Optional[str]) -> dict:
  """Run one synthetic work item through _session_consumer; return its MASTER_DONE payload."""
  session_id = f"slack-plumbing-{user_event_id or 'none'}"
  callbacks = mock_session_callbacks()
  item = make_work_item(
      MagicMock(),
      SessionMetadata(id=session_id, name="t"),
      None,
      user_content="hi",
      callbacks=callbacks,
      user_event_id=user_event_id,
  )

  async def fake_run_cc(_item: master_cc._WorkItem) -> tuple:
    return ("cc-1", 0, None, {})

  await run_session_consumer(session_id, [item], fake_run_cc)

  done_events = [
      call.args[1] for call in callbacks.persist_and_broadcast.call_args_list
      if call.args[1].get("type") == ET.MASTER_DONE
  ]
  assert len(done_events) == 1
  return done_events[0]


@pytest.mark.asyncio
async def test_master_done_carries_input_event_id_when_round_has_user_event() -> None:
  done = await _run_one_round("evt-1")
  assert done["input_event_id"] == "evt-1"


@pytest.mark.asyncio
async def test_master_done_omits_input_event_id_when_round_has_no_user_event() -> None:
  done = await _run_one_round(None)
  assert "input_event_id" not in done
