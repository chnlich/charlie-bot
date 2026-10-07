"""Acceptance tests for the Slack summon schema plumbing (config, session schema, master_done)."""

from __future__ import annotations

from unittest import mock

import conftest
import pytest

from src.infra import config, models
from src.infra import event_types as ET


def test_config_without_slack_keys_yields_defaults() -> None:
  cfg = config.CharlieBotConfig.model_validate({})
  assert cfg.slack.allowed_user_ids == []


def test_slack_tokens_come_from_credentials() -> None:
  conftest.stub_credentials({"slack": {
      "bot_token": "test-bot-token",
      "app_token": "test-app-token",
  }})
  creds = config.get_credentials()
  assert creds.get("slack", "bot_token") == "test-bot-token"
  assert creds.get("slack", "app_token") == "test-app-token"


async def _run_one_round(user_event_id: str | None) -> dict:
  """Run one synthetic work item through _session_consumer; return its MASTER_DONE payload."""
  session_id = f"slack-plumbing-{user_event_id or 'none'}"
  callbacks = conftest.mock_session_callbacks()
  item = conftest.make_work_item(
      mock.MagicMock(),
      models.SessionMetadata(id=session_id, name="t"),
      None,
      user_content="hi",
      callbacks=callbacks,
      user_event_id=user_event_id,
  )

  await conftest.run_session_consumer(session_id, [item], conftest.make_sound_round("cc-1"))

  done_events = [
      call.args[1]
      for call in callbacks.persist_and_broadcast.call_args_list
      if call.args[1].get("type") == ET.MASTER_DONE
  ]
  assert len(done_events) == 1
  return done_events[0]


@pytest.mark.asyncio
async def test_master_done_carries_input_event_ids_when_round_has_user_event() -> None:
  done = await _run_one_round("evt-1")
  assert done["input_event_ids"] == ["evt-1"]
