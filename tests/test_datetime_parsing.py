from __future__ import annotations

import datetime
import pathlib
import types
from unittest import mock

import conftest
import pytest
import yaml

from src.features.backlog import backlog_loop
from src.features.backlog import config as backlog_config
from src.features.cron import config as cron_config
from src.infra import models


def test_parse_utc_datetime_accepts_z_and_normalizes_naive() -> None:
  parsed_z = models.parse_utc_datetime("2026-04-17T12:34:56Z")
  assert parsed_z == datetime.datetime(2026, 4, 17, 12, 34, 56, tzinfo=datetime.UTC)

  parsed_naive = models.parse_utc_datetime("2026-04-17T12:34:56")
  assert parsed_naive == datetime.datetime(2026, 4, 17, 12, 34, 56, tzinfo=datetime.UTC)


@pytest.mark.asyncio
async def test_handle_stale_accepts_z_timestamp(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: pathlib.Path,
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
  cfg = backlog_config.ImprovementLoopConfig(
      backlog="backlog/backlog.yaml",
      role="test agent",
      scope_files=["src/runtime"],
      stale_timeout_hours=1.0,
  )
  commit_mock = mock.AsyncMock()

  monkeypatch.setattr(conftest.BACKLOG_LOOP_GIT_ADD_COMMIT_PUSH_PATCH_TARGET, commit_mock)
  # The stand-in mimics the datetime module surface _handle_stale reads: .datetime.now for the
  # clock and .UTC for the tz argument passed at the same call.
  monkeypatch.setattr(
      "src.features.backlog.backlog_loop.datetime",
      types.SimpleNamespace(
          datetime=types.SimpleNamespace(now=lambda tz: datetime.datetime(2026, 4, 17, 2, 30, tzinfo=tz)),
          UTC=datetime.UTC,
      ))

  modified = await backlog_loop._handle_stale(items, backlog_path, cfg, tmp_path)

  assert modified is True
  assert items[0]["status"] == "failed"
  assert items[0]["failed_reason"] == "Timed out after 1.0 hour(s)"
  assert yaml.safe_load(backlog_path.read_text(encoding="utf-8"))[0]["status"] == "failed"
  commit_mock.assert_awaited_once()


@pytest.mark.asyncio
async def test_scheduler_maybe_run_accepts_naive_last_scheduled_run(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: pathlib.Path,
) -> None:
  cfg = conftest.make_home_config(tmp_path)
  session_blocks = conftest.build_session_blocks(cfg)
  tree = conftest.build_task_tree(cfg, session_blocks)
  conftest.bind_deps_blocks(monkeypatch, tree, session_blocks)
  scheduler = conftest.build_scheduler(cfg, session_blocks)
  session = await conftest.create_scheduled_node(tree, name="backup", backend=None)
  # Base is in the future so croniter's next fire is always after now, removing the minute-boundary
  # race a past base had: with cron "* * * * *" it fired whenever the test ran just after a boundary.
  session.last_scheduled_run = (datetime.datetime.now(datetime.UTC) +
                                datetime.timedelta(minutes=5)).replace(tzinfo=None).isoformat()
  await session_blocks.store.save_metadata(session)
  task_cfg = cron_config.ScheduledTaskConfig(
      name="backup",
      cron="* * * * *",
      handler="backup",
      timezone="UTC",
      session_id=session.id,
  )
  execute_task = mock.AsyncMock()

  monkeypatch.setattr(scheduler, "_execute_task", execute_task)

  await scheduler._maybe_run(task_cfg, session_blocks.events, {}, cfg)

  execute_task.assert_not_awaited()
