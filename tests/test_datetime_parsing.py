from __future__ import annotations

from datetime import UTC, datetime, timedelta
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest
import yaml

from src.core.backlog_loop import _handle_stale
from src.core.config import CharlieBotConfig, ImprovementLoopConfig, ScheduledTaskConfig
from src.core.models import SessionMetadata, parse_utc_datetime
from src.core.scheduler import Scheduler


def test_parse_utc_datetime_accepts_z_and_normalizes_naive() -> None:
  parsed_z = parse_utc_datetime("2026-04-17T12:34:56Z")
  assert parsed_z == datetime(2026, 4, 17, 12, 34, 56, tzinfo=UTC)

  parsed_naive = parse_utc_datetime("2026-04-17T12:34:56")
  assert parsed_naive == datetime(2026, 4, 17, 12, 34, 56, tzinfo=UTC)


@pytest.mark.asyncio
async def test_handle_stale_accepts_z_timestamp(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  backlog_path = tmp_path / "backlog.yaml"
  items = [
      {
          "id": "099",
          "status": "in_progress",
          "title": "Promote datetime parser",
          "created": "2026-04-17T00:00:00Z",
      }
  ]
  cfg = ImprovementLoopConfig(
      backlog="backlog/backlog.yaml",
      role="test agent",
      scope_files=["src/core"],
      stale_timeout_hours=1.0,
  )
  commit_mock = AsyncMock()

  monkeypatch.setattr("src.core.backlog_loop.git_add_commit_push", commit_mock)
  monkeypatch.setattr(
      "src.core.backlog_loop.datetime", SimpleNamespace(now=lambda tz: datetime(2026, 4, 17, 2, 30, tzinfo=tz),))

  modified = await _handle_stale(items, backlog_path, cfg, tmp_path)

  assert modified is True
  assert items[0]["status"] == "failed"
  assert items[0]["failed_reason"] == "Timed out after 1.0 hour(s)"
  assert yaml.safe_load(backlog_path.read_text(encoding="utf-8"))[0]["status"] == "failed"
  commit_mock.assert_awaited_once()


@pytest.mark.asyncio
async def test_scheduler_maybe_run_accepts_naive_last_scheduled_run(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "charliebot-home")
  scheduler = Scheduler(cfg, AsyncMock())
  session = SessionMetadata(name="Backup session")
  # Base is in the future so croniter's next fire is always after now, removing the minute-boundary
  # race a past base had: with cron "* * * * *" it fired whenever the test ran just after a boundary.
  session.last_scheduled_run = (datetime.now(UTC) + timedelta(minutes=5)).replace(tzinfo=None).isoformat()
  task_cfg = ScheduledTaskConfig(
      name="backup",
      cron="* * * * *",
      handler="backup",
      timezone="UTC",
  )
  execute_task = AsyncMock()

  monkeypatch.setattr(scheduler, "_get_or_create_session", AsyncMock(return_value=session))
  monkeypatch.setattr(scheduler, "_execute_task", execute_task)

  await scheduler._maybe_run(task_cfg, AsyncMock(), {})

  execute_task.assert_not_awaited()
