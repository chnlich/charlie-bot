"""Acceptance tests for cron load-failure Telegram alerting.

At every cron snapshot reload, ``src/core/config.py::_fire_cron_error_alert``
compares the fresh broken-task name set against the last-alerted set persisted
at ``<CHARLIEBOT_HOME>/state/cron_alert_fingerprint.json``: a transition to a
non-empty set fires one ``"⚠️ cron 任务加载失败: <names>"``, a transition back
to empty fires one ``"✅ cron 加载失败已全部解除"``, and an identical set stays
silent — including across a restart, because the fingerprint lives on disk. A
synchronous (no running loop) context skips the send without persisting, so the
scheduler's unconditional 60s tick still fires the alert. Telegram failures are
log-only and never escape into the loader.
"""

import asyncio
import json
from pathlib import Path

import pytest
from conftest import cron_d_dir as _cron_d_dir
from conftest import dump_yaml as _dump
from conftest import write_cron_task as _write_task_text

import src.core.config as cm


@pytest.fixture
def temp_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  """Point HOME at a temp dir and reset the config/cron module-level caches."""
  monkeypatch.delenv("CHARLIEBOT_HOME", raising=False)
  monkeypatch.setenv("HOME", str(tmp_path))
  cm._config = None
  cm._config_mtime = 0.0
  cm._cron_snapshot = cm._CronSnapshot()
  return tmp_path


@pytest.fixture
def sent(monkeypatch: pytest.MonkeyPatch) -> list[str]:
  """Replace Telegram delivery with a recording stub."""
  messages: list[str] = []

  async def fake_send_telegram(message: str, cfg) -> None:
    messages.append(message)

  monkeypatch.setattr("src.core.notifications.send_telegram", fake_send_telegram)
  return messages


def _state_file(home: Path) -> Path:
  return home / ".charliebot" / "state" / "cron_alert_fingerprint.json"


def _fire(error_names: list[str]) -> None:
  """Run one alert evaluation on a fresh loop, draining the fired send task."""
  async def go() -> None:
    cm._fire_cron_error_alert(error_names)
    await asyncio.sleep(0)
    await asyncio.sleep(0)

  asyncio.run(go())


def test_alert_fires_once_on_transition_recovers_once_and_repeats_nothing(
    temp_home: Path,
    sent: list[str],
) -> None:
  _fire(["beta", "alpha"])
  assert sent == ["⚠️ cron 任务加载失败: alpha, beta"]
  assert json.loads(_state_file(temp_home).read_text(encoding="utf-8")) == ["alpha", "beta"]

  # The identical set stays silent — including across a simulated restart
  # (fresh in-memory caches; only the persisted fingerprint survives).
  cm._config = None
  cm._config_mtime = 0.0
  cm._cron_snapshot = cm._CronSnapshot()
  _fire(["alpha", "beta"])
  assert sent == ["⚠️ cron 任务加载失败: alpha, beta"]

  # Recovery (non-empty → empty) fires exactly once.
  _fire([])
  assert sent == ["⚠️ cron 任务加载失败: alpha, beta", "✅ cron 加载失败已全部解除"]
  assert json.loads(_state_file(temp_home).read_text(encoding="utf-8")) == []

  _fire([])
  assert len(sent) == 2


def test_no_event_loop_skips_send_without_persisting(
    temp_home: Path,
    sent: list[str],
) -> None:
  # Synchronous CLI context: no running loop, so nothing is sent and nothing is
  # persisted — the next looped evaluation transitions again and fires.
  cm._fire_cron_error_alert(["x"])
  assert sent == []
  assert not _state_file(temp_home).exists()

  _fire(["x"])
  assert sent == ["⚠️ cron 任务加载失败: x"]


def test_telegram_failure_is_log_only(temp_home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  async def raising_send_telegram(message: str, cfg) -> None:
    raise RuntimeError("telegram_bot_token is not configured")

  monkeypatch.setattr("src.core.notifications.send_telegram", raising_send_telegram)

  _fire(["x"])  # must not raise out of the evaluation
  assert json.loads(_state_file(temp_home).read_text(encoding="utf-8")) == ["x"]


def test_alert_fires_through_loader_refresh(
    temp_home: Path,
    sent: list[str],
) -> None:
  """The scheduler tick path: get_scheduled_tasks → snapshot refresh → alert."""
  target = temp_home / "prompts" / "memory_curator.md"
  target.parent.mkdir(parents=True, exist_ok=True)
  _write_task_text(temp_home, "memory-curator", _dump(
      {"cron": "27 6 * * *", "prompt_file": str(target)}))

  async def refresh() -> None:
    assert cm.get_scheduled_tasks() == []
    await asyncio.sleep(0)
    await asyncio.sleep(0)

  asyncio.run(refresh())
  assert sent == ["⚠️ cron 任务加载失败: memory-curator"]

  # Restoring the pointed file (the host file only carries the path to it) flips
  # the job back to healthy on the next refresh, and the recovery notification
  # goes out once.
  target.write_text("curate memory now", encoding="utf-8")

  async def refresh_again() -> None:
    assert [t.name for t in cm.get_scheduled_tasks()] == ["memory-curator"]
    await asyncio.sleep(0)
    await asyncio.sleep(0)

  asyncio.run(refresh_again())
  assert sent == ["⚠️ cron 任务加载失败: memory-curator", "✅ cron 加载失败已全部解除"]
