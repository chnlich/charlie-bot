"""Unit tests for the TriggerManager.list_triggers per-file memo."""
from __future__ import annotations

from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import count_path_read_text, make_home_config

from src.core.models import PendingTrigger, TriggerStatus
from src.core.sessions import SessionManager
from src.core.triggers import TriggerManager


def _make_manager(tmp_path: Path) -> TriggerManager:
  cfg = make_home_config(tmp_path)
  cfg.sessions_dir.mkdir(parents=True)
  return TriggerManager(cfg, SessionManager(cfg))


async def _save(mgr: TriggerManager, session_id: str, message: str) -> PendingTrigger:
  trigger = PendingTrigger(
      session_id=session_id,
      fire_at=datetime.now(UTC) + timedelta(hours=1),
      message=message,
      watch_targets=[],
  )
  await mgr._save_trigger(trigger)
  return trigger


@pytest.mark.asyncio
async def test_list_triggers_steady_state_reads_no_files(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  mgr = _make_manager(tmp_path)
  saved = [await _save(mgr, "s1", f"trigger {i}") for i in range(3)]

  first = await mgr.list_triggers("s1")
  assert {t.id for t in first} == {t.id for t in saved}

  reads = count_path_read_text(monkeypatch, lambda path: True)
  for _ in range(3):
    again = await mgr.list_triggers("s1")
    assert [t.id for t in again] == [t.id for t in first]
  assert reads == []


@pytest.mark.asyncio
async def test_list_triggers_rereads_after_rewrite(tmp_path: Path) -> None:
  mgr = _make_manager(tmp_path)
  trigger = await _save(mgr, "s1", "rewrite me")
  assert (await mgr.list_triggers("s1"))[0].status == TriggerStatus.PENDING

  trigger.status = TriggerStatus.CANCELLED
  await mgr._save_trigger(trigger)
  listed = await mgr.list_triggers("s1")
  assert len(listed) == 1
  assert listed[0].status == TriggerStatus.CANCELLED


@pytest.mark.asyncio
async def test_list_triggers_drops_deleted_files(tmp_path: Path) -> None:
  mgr = _make_manager(tmp_path)
  keep = await _save(mgr, "s1", "keep")
  drop = await _save(mgr, "s1", "drop")
  assert len(await mgr.list_triggers("s1")) == 2

  mgr._trigger_path("s1", drop.id).unlink()
  listed = await mgr.list_triggers("s1")
  assert [t.id for t in listed] == [keep.id]


@pytest.mark.asyncio
async def test_list_triggers_missing_dir_returns_empty(tmp_path: Path) -> None:
  mgr = _make_manager(tmp_path)
  assert await mgr.list_triggers("no-such-session") == []


@pytest.mark.asyncio
async def test_list_triggers_memo_lru_evicts_oldest_session(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  mgr = _make_manager(tmp_path)
  monkeypatch.setattr(mgr._list_memo, "_limit", 2)
  for sid in ("a", "b", "c"):
    await _save(mgr, sid, f"trigger for {sid}")
    await mgr.list_triggers(sid)

  assert list(mgr._list_memo) == ["b", "c"]
