"""Unit tests for the trigger --message short-label limit (200 chars max)."""
from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from pydantic import ValidationError

from src.cli import schedule_trigger as cli_module
from src.core.config import CharlieBotConfig
from src.core.models import (
  CreateSessionRequest,
  MAX_TRIGGER_MESSAGE_CHARS,
  ScheduleTriggerRequest,
  TriggerStatus,
)
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager


def _fake_cli_cfg(monkeypatch, triggers_dir: Path | None = None) -> None:
  """Point the CLI HTTP layer at a fake config so tests never touch a real server."""

  class _Cfg:
    server_base_url = "https://server"
    charliebot_access_key = ""
    sessions_dir = triggers_dir if triggers_dir is not None else Path("/nonexistent-sessions")

  monkeypatch.setattr("src.cli.common.get_config", lambda: _Cfg())


# ---------------------------------------------------------------------------
# CLI: 201-char --message rejected before any network call, exit 2
# ---------------------------------------------------------------------------


def test_cli_rejects_201_char_message(monkeypatch, tmp_path: Path) -> None:
  triggers_dir = tmp_path / "s1" / "triggers"
  triggers_dir.mkdir(parents=True)
  argv = [
      "schedule_trigger",
      "--session", "s1",
      "--max-wait", "60",
      "--message", "x" * 201,
  ]
  _fake_cli_cfg(monkeypatch, triggers_dir)

  with patch.object(sys, "argv", argv):
    with patch("src.cli.schedule_trigger.post_internal_api") as mock_post:
      with pytest.raises(SystemExit) as excinfo:
        cli_module.main()

  assert excinfo.value.code == 2
  mock_post.assert_not_called()
  # Zero new trigger JSON files were written under the session's triggers dir.
  assert list(triggers_dir.glob("*.json")) == []


# ---------------------------------------------------------------------------
# Pydantic model: 201-char message raises ValidationError
# ---------------------------------------------------------------------------


def test_schedule_trigger_request_rejects_201_char_message() -> None:
  with pytest.raises(ValidationError):
    ScheduleTriggerRequest(
        session_id="s1",
        delay_seconds=60,
        message="x" * 201,
    )


def test_schedule_trigger_request_accepts_200_char_message() -> None:
  req = ScheduleTriggerRequest(
      session_id="s1",
      delay_seconds=60,
      message="x" * MAX_TRIGGER_MESSAGE_CHARS,
  )
  assert req.message == "x" * MAX_TRIGGER_MESSAGE_CHARS


# ---------------------------------------------------------------------------
# Help / error text embeds the constant value
# ---------------------------------------------------------------------------


def test_cli_help_embeds_constant() -> None:
  parser = cli_module._build_parser()
  help_text = parser.format_help()
  assert str(MAX_TRIGGER_MESSAGE_CHARS) in help_text


def test_cli_error_embeds_constant() -> None:
  with pytest.raises(argparse.ArgumentTypeError) as excinfo:
    cli_module._validate_message("x" * 201)
  assert str(MAX_TRIGGER_MESSAGE_CHARS) in str(excinfo.value)


# ---------------------------------------------------------------------------
# Persisted over-limit PendingTrigger fires with the message injected verbatim
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_persisted_over_limit_message_fires_verbatim(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="Over-limit fire"))
  trigger_mgr = TriggerManager(cfg, session_mgr)

  trigger_id = "over-limit-1"
  fire_at = (datetime.now(timezone.utc) + timedelta(seconds=30)).isoformat()
  now = datetime.now(timezone.utc).isoformat()
  long_message = "y" * 201
  triggers_dir = cfg.sessions_dir / session.id / "triggers"
  triggers_dir.mkdir(parents=True)
  (triggers_dir / f"{trigger_id}.json").write_text(
      json.dumps(
          {
              "id": trigger_id,
              "session_id": session.id,
              "fire_at": fire_at,
              "message": long_message,
              "created_at": now,
              "status": "pending",
              "fired_at": None,
              "watch_targets": [],
          }
      ),
      encoding="utf-8",
  )

  with (
      patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()),
      patch("src.core.triggers.trigger_master", new=AsyncMock()) as mock_master,
  ):
    trigger = await trigger_mgr._load_trigger(session.id, trigger_id)
    await trigger_mgr._wait_and_fire(trigger)

  stored = await trigger_mgr._load_trigger(session.id, trigger_id)
  assert stored.status == TriggerStatus.FIRED
  msg = mock_master.await_args.args[1]
  assert msg == f"[Scheduled trigger fired] {long_message}"