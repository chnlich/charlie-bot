"""Acceptance tests for cron load-failure Telegram alerting.

At every cron snapshot reload, ``src/infra/config.py::_fire_cron_error_alert``
compares the fresh broken-task name set against the last-alerted set persisted
at ``<CHARLIEBOT_HOME>/state/cron_alert_fingerprint.json``: a transition to a
non-empty set fires one ``"⚠️ cron tasks failed to load: <names>"``, a transition back
to empty fires one ``"✅ all cron load failures resolved"``, and an identical set stays
silent — including across a restart, because the fingerprint lives on disk. A
synchronous (no running loop) context skips the send without persisting, so the
scheduler's unconditional 60s tick still fires the alert. Telegram failures are
log-only and never escape into the loader.
"""

import asyncio
import json
import pathlib

import conftest
import pytest

from src.infra import config

# Import-path patch target for the Telegram delivery the cron-load alert posts. The alert helper
# in src/infra/config.py imports send_telegram at call time (lazy, notifications imports config),
# so that import resolves the stand-in landed on the src.infra.notifications module attribute;
# import-scope binders of the same function keep their own bound object and
# are not intercepted through this route.
NOTIFICATIONS_SEND_TELEGRAM_PATCH_TARGET = "src.infra.notifications.send_telegram"


@pytest.fixture
def sent(monkeypatch: pytest.MonkeyPatch) -> list[str]:
  """Replace Telegram delivery with a recording stub."""
  messages: list[str] = []

  async def fake_send_telegram(message: str, cfg: config.CharlieBotConfig) -> None:
    messages.append(message)

  monkeypatch.setattr(NOTIFICATIONS_SEND_TELEGRAM_PATCH_TARGET, fake_send_telegram)
  return messages


def _state_file(home: pathlib.Path) -> pathlib.Path:
  return home / ".charliebot" / "state" / "cron_alert_fingerprint.json"


def _fire(error_names: list[str]) -> None:
  """Run one alert evaluation on a fresh loop, draining the fired send task."""

  async def go() -> None:
    config._fire_cron_error_alert(error_names)
    await asyncio.sleep(0)
    await asyncio.sleep(0)

  asyncio.run(go())


def test_alert_fires_once_on_transition_recovers_once_and_repeats_nothing(
    temp_home: pathlib.Path,
    sent: list[str],
) -> None:
  _fire(["beta", "alpha"])
  assert sent == ["⚠️ cron tasks failed to load: alpha, beta"]
  assert json.loads(_state_file(temp_home).read_text(encoding="utf-8")) == ["alpha", "beta"]

  # The identical set stays silent — including across a simulated restart
  # (fresh in-memory caches; only the persisted fingerprint survives).
  conftest.reset_config_caches()
  _fire(["alpha", "beta"])
  assert sent == ["⚠️ cron tasks failed to load: alpha, beta"]

  # Recovery (non-empty → empty) fires exactly once.
  _fire([])
  assert sent == ["⚠️ cron tasks failed to load: alpha, beta", "✅ all cron load failures resolved"]
  assert json.loads(_state_file(temp_home).read_text(encoding="utf-8")) == []

  _fire([])
  assert len(sent) == 2


def test_no_event_loop_skips_send_without_persisting(
    temp_home: pathlib.Path,
    sent: list[str],
) -> None:
  # Synchronous CLI context: no running loop, so nothing is sent and nothing is
  # persisted — the next looped evaluation transitions again and fires.
  config._fire_cron_error_alert(["x"])
  assert not sent
  assert not _state_file(temp_home).exists()

  _fire(["x"])
  assert sent == ["⚠️ cron tasks failed to load: x"]


def test_telegram_failure_is_log_only(temp_home: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:

  async def raising_send_telegram(message: str, cfg: config.CharlieBotConfig) -> None:
    raise RuntimeError("telegram delivery failed")

  monkeypatch.setattr(NOTIFICATIONS_SEND_TELEGRAM_PATCH_TARGET, raising_send_telegram)

  _fire(["x"])  # must not raise out of the evaluation
  assert json.loads(_state_file(temp_home).read_text(encoding="utf-8")) == ["x"]
