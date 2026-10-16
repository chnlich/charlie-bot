"""The per-session pending-trigger limit: counting, rejection, concurrency, exemption.

The limit lives on TriggerManager (src/runtime/triggers.py): a schedule-trigger
registration that would push the session past MAX_PENDING_TRIGGERS pending
records is rejected with the fixed 422 detail, the count and the record write
sit under one per-session lock (two concurrent registrations against four
pending records produce exactly one), fired and cancelled records do not
count, and the Slack thread-follow re-arm is never rejected.
"""

from __future__ import annotations

import asyncio
import datetime
import pathlib
import sys
from unittest import mock

import conftest
import fastapi
import pytest
from fastapi import testclient

from src.features.chat_threads import thread_entry
from src.features.slack import slack_listener
from src.infra import config, models
from src.runtime import triggers
from src.runtime.api import deps, internal
from src.runtime.cli import schedule_trigger as cli_module


def _limit_detail(session_id: str, count: int) -> str:
  return (
      f"session {session_id} has {count} pending triggers (limit {triggers.MAX_PENDING_TRIGGERS}); "
      "watch several targets with one trigger: --watch A --watch B")


async def _seed(
    tmp_path: pathlib.Path,
    *,
    pending: int = 0,
    fired: int = 0,
    cancelled: int = 0,
) -> tuple[config.CharlieBotConfig, conftest.SessionBlocks, triggers.TriggerManager, str]:
  cfg = conftest.make_home_config(tmp_path)
  session_blocks = conftest.build_session_blocks(cfg)
  session = await conftest.create_root_session(session_blocks, models.CreateSessionRequest(name="limit"))
  trigger_mgr = triggers.TriggerManager(cfg, session_blocks.tree)
  for i in range(fired):
    await trigger_mgr._save_trigger(
        models.PendingTrigger(
            id=f"fired-{i}",
            session_id=session.id,
            fire_at=datetime.datetime.now(datetime.UTC) - datetime.timedelta(hours=1),
            message=f"fired {i}",
            status=models.TriggerStatus.FIRED,
        ))
  for i in range(cancelled):
    await trigger_mgr._save_trigger(
        models.PendingTrigger(
            id=f"cancelled-{i}",
            session_id=session.id,
            fire_at=datetime.datetime.now(datetime.UTC) - datetime.timedelta(hours=1),
            message=f"cancelled {i}",
            status=models.TriggerStatus.CANCELLED,
        ))
  for i in range(pending):
    await trigger_mgr.create_trigger(session.id, 3600, f"pending {i}")
  return cfg, session_blocks, trigger_mgr, session.id


async def _pending_count(trigger_mgr: triggers.TriggerManager, session_id: str) -> int:
  return sum(1 for t in await trigger_mgr.list_triggers(session_id) if t.status is models.TriggerStatus.PENDING)


# ---------------------------------------------------------------------------
# Rejection: the sixth registration, its exact detail, the API 422, the CLI exit 2
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_sixth_registration_raises_with_exact_detail(tmp_path: pathlib.Path) -> None:
  _cfg, _sessions, trigger_mgr, session_id = await _seed(tmp_path, pending=triggers.MAX_PENDING_TRIGGERS)

  with pytest.raises(triggers.PendingTriggerLimitError) as excinfo:
    await trigger_mgr.create_trigger(session_id, 3600, "one too many")

  assert str(excinfo.value) == _limit_detail(session_id, triggers.MAX_PENDING_TRIGGERS)
  assert await _pending_count(trigger_mgr, session_id) == triggers.MAX_PENDING_TRIGGERS


def test_api_returns_422_with_exact_detail_and_cli_exits_2(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, sessions, trigger_mgr, session_id = asyncio.run(_seed(tmp_path, pending=triggers.MAX_PENDING_TRIGGERS))
  detail = _limit_detail(session_id, triggers.MAX_PENDING_TRIGGERS)

  app = fastapi.FastAPI()
  app.include_router(internal.router, prefix="/api/internal")
  app.dependency_overrides[deps.get_session_store] = lambda: sessions.store
  app.dependency_overrides[deps.get_trigger_manager] = lambda: trigger_mgr
  res = testclient.TestClient(app).post(
      "/api/internal/schedule-trigger",
      json={
          "session_id": session_id,
          "delay_seconds": 3600,
          "message": "one too many"
      },
  )
  assert res.status_code == 422
  assert res.json()["detail"] == detail

  # The CLI maps the same rejection to exit 2 through its existing 422 contract.
  monkeypatch.setattr(conftest.CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr(
      conftest.CLI_COMMON_TRANSPORT_POST_PATCH_TARGET,
      lambda *a, **k: conftest.make_json_response({"detail": detail}, status_code=422))
  conftest.fake_cli_cfg(monkeypatch, cfg.sessions_dir)
  with (
      mock.patch.object(sys, "argv", conftest.schedule_trigger_argv("one too many")),
      pytest.raises(SystemExit) as excinfo,
  ):
    cli_module.main()
  assert excinfo.value.code == 2


# ---------------------------------------------------------------------------
# Concurrency: count and write under one per-session lock
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_concurrent_registrations_against_four_pending_create_exactly_one(tmp_path: pathlib.Path) -> None:
  _cfg, _sessions, trigger_mgr, session_id = await _seed(tmp_path, pending=4)

  results = await asyncio.gather(
      trigger_mgr.create_trigger(session_id, 3600, "racer A"),
      trigger_mgr.create_trigger(session_id, 3600, "racer B"),
      return_exceptions=True,
  )
  succeeded = [r for r in results if not isinstance(r, BaseException)]
  rejected = [r for r in results if isinstance(r, triggers.PendingTriggerLimitError)]
  assert len(succeeded) == 1
  assert len(rejected) == 1
  # The loser counts after the winner's save landed under the lock: 5 pending.
  assert str(rejected[0]) == _limit_detail(session_id, 5)
  assert await _pending_count(trigger_mgr, session_id) == 5


# ---------------------------------------------------------------------------
# The Slack thread-follow re-arm is exempt; fired and cancelled do not count
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_slack_follow_rearm_succeeds_on_a_full_session(tmp_path: pathlib.Path) -> None:
  _cfg, _sessions, trigger_mgr, session_id = await _seed(tmp_path, pending=triggers.MAX_PENDING_TRIGGERS)

  with (
      mock.patch(conftest.BROADCAST_PATCH_TARGET, new=mock.MagicMock()),
      mock.patch(conftest.TRIGGER_TASK_DELIVERY_PATCH_TARGET, new=mock.MagicMock()),
  ):
    trigger = await thread_entry.arm_follow_trigger(
        slack_listener.SLACK,
        trigger_mgr,
        session_id,
        floor="1700000000.000100",
        log_fields={
            "channel": "C1",
            "thread_ts": "1700000000.000100"
        },
    )

  assert trigger is not None
  assert await _pending_count(trigger_mgr, session_id) == triggers.MAX_PENDING_TRIGGERS + 1


@pytest.mark.asyncio
async def test_fired_and_cancelled_records_do_not_count(tmp_path: pathlib.Path) -> None:
  _cfg, _sessions, trigger_mgr, session_id = await _seed(tmp_path, pending=3, fired=2, cancelled=2)

  # 3 pending count: two more registrations land, the sixth pending is rejected.
  await trigger_mgr.create_trigger(session_id, 3600, "fourth")
  await trigger_mgr.create_trigger(session_id, 3600, "fifth")
  with pytest.raises(triggers.PendingTriggerLimitError) as excinfo:
    await trigger_mgr.create_trigger(session_id, 3600, "sixth")
  assert str(excinfo.value) == _limit_detail(session_id, 5)
  assert await _pending_count(trigger_mgr, session_id) == 5
