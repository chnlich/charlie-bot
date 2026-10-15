from __future__ import annotations

import datetime
import pathlib
from unittest import mock

import conftest
import pytest

from src.features.cron import config as cron_config
from src.infra import models


def test_parse_utc_datetime_accepts_z_and_normalizes_naive() -> None:
  parsed_z = models.parse_utc_datetime("2026-04-17T12:34:56Z")
  assert parsed_z == datetime.datetime(2026, 4, 17, 12, 34, 56, tzinfo=datetime.UTC)

  parsed_naive = models.parse_utc_datetime("2026-04-17T12:34:56")
  assert parsed_naive == datetime.datetime(2026, 4, 17, 12, 34, 56, tzinfo=datetime.UTC)


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
