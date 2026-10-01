"""Host login authorization: classification, baseline accounting, estimates, routes, and repo hygiene."""

from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest

from src.api import host_auth as api
from src.core import host_auth as core


def _iso(moment: datetime) -> str:
  return moment.isoformat()


def _entry(alias: str, hostname: str, **overrides: Any) -> dict:
  # Builds through the production constructor so the fixture's shape is the
  # shape _merge_hosts writes into the state, not a second copy of it.
  entry = core._new_entry(alias, hostname)
  entry.update(overrides)
  return entry


@pytest.fixture(autouse=True)
def _reset_api_round_state() -> Any:
  api._round_running = False
  api._poller.task = None
  yield
  api._round_running = False
  api._poller.task = None


@pytest.mark.parametrize(
    ("returncode", "output", "expected"),
    [
        pytest.param(124, "Okta verification required", core.STATUS_NEEDS_OKTA, id="held-output-beats-a-killed-exit"),
        pytest.param(
            -9,
            "Enroll at https://login.example.internal/activate?user_code=7f3c9d21",
            core.STATUS_NEEDS_OKTA,
            id="user-code-marker"),
        pytest.param(0, "", core.STATUS_OK, id="exit-zero"),
        pytest.param(0, "some unrelated banner", core.STATUS_OK, id="exit-zero-with-noise"),
        pytest.param(
            1,
            "# Tailscale SSH requires an additional check.\nTo authenticate, visit: https://login.example.net/a/7f3c9d21",
            core.STATUS_NEEDS_INTERACTIVE_AUTH,
            id="additional-check"),
        pytest.param(255, "Host key verification failed.", core.STATUS_NEEDS_INTERACTIVE_AUTH, id="host-key"),
        pytest.param(255, "ssh: connect to host port 22: Connection timed out", core.STATUS_UNREACHABLE, id="timeout"),
        pytest.param(
            124,
            "ssh: connect to host port 22: Connection timed out",
            core.STATUS_UNREACHABLE,
            id="killed-without-markers"),
    ],
)
def test_classification_reads_output_before_the_return_code(returncode: int, output: str, expected: str) -> None:
  """A held host's prompt waits until the probe timeout kills it, so the exit code cannot classify."""
  assert core.classify(returncode, output) == expected


def test_baseline_moves_on_the_held_to_direct_transition_and_not_on_a_repeat() -> None:
  adjacent = _entry("login-a", "login-a.example.internal", status=core.STATUS_NEEDS_OKTA)
  moment = datetime(2026, 9, 15, 8, 5, tzinfo=UTC)
  core.apply_probe_result(adjacent, status=core.STATUS_OK, detail="exit 0", probed_at=moment)
  assert adjacent["enrolled_observed_at"] == _iso(moment)
  repeat = moment + timedelta(minutes=30)
  core.apply_probe_result(adjacent, status=core.STATUS_OK, detail="exit 0", probed_at=repeat)
  assert adjacent["enrolled_observed_at"] == _iso(moment)


def test_held_host_backs_off_twelve_times_the_standing_period() -> None:
  assert core.BLOCKED_BACKOFF_SEC == 21600
  now = datetime.now(UTC)
  held = _entry(
      "login-a",
      "login-a.example.internal",
      status=core.STATUS_NEEDS_OKTA,
      last_probe_at=_iso(now - timedelta(seconds=1800)))
  direct = _entry(
      "login-b", "login-b.example.internal", status=core.STATUS_OK, last_probe_at=_iso(now - timedelta(seconds=1800)))
  assert not core.host_due(held, now)
  assert core.host_due(held, now + timedelta(seconds=21600))
  assert core.host_due(direct, now)
  assert core.host_due(None, now)
  assert core.host_due(_entry("login-c", "login-c.example.internal"), now)


@pytest.mark.asyncio
async def test_round_probes_due_hosts_and_publishes_the_state(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  config = tmp_path / "ssh_config"
  config.write_text(
      "Host login-a\n  HostName login-a.example.internal\n\nHost gpu-box-1\n  HostName gpu-box-1.example.internal\n",
      encoding="utf-8")
  state_path = tmp_path / "state.json"
  now = datetime.now(UTC)
  seed = core.empty_state()
  seed["hosts"] = [
      _entry(
          "login-a",
          "login-a.example.internal",
          status=core.STATUS_NEEDS_OKTA,
          last_probe_at=_iso(now - timedelta(minutes=1)))
  ]
  state_path.write_text(json.dumps(seed), encoding="utf-8")

  probed: list[str] = []

  async def fake_probe(alias: str) -> tuple[int | None, str]:
    probed.append(alias)
    if alias == "login-a":
      return (0, "")
    return (255, "ssh: connect to host port 22: Connection timed out")

  monkeypatch.setattr(core, "probe_host", fake_probe)

  state = await core.run_round(state_path=state_path, ssh_config_path=config)
  # The held host probed a minute ago is inside its 21600 s backoff; the new host is always due.
  assert probed == ["gpu-box-1"]
  by_alias = {entry["alias"]: entry for entry in state["hosts"]}
  assert by_alias["gpu-box-1"]["status"] == core.STATUS_UNREACHABLE
  assert by_alias["login-a"]["status"] == core.STATUS_NEEDS_OKTA
  assert state["probe_running"] is False
  assert state["probed_at"] is not None
  assert state["interval_sec"] == 1800
  assert state["ttl_sec"] == 604800
  published = json.loads(state_path.read_text(encoding="utf-8"))
  assert [entry["alias"] for entry in published["hosts"]] == ["login-a", "gpu-box-1"]

  state = await core.run_round(force=True, state_path=state_path, ssh_config_path=config)
  assert probed == ["gpu-box-1", "login-a", "gpu-box-1"]
  by_alias = {entry["alias"]: entry for entry in state["hosts"]}
  # The manual round ignores the backoff, and the held-to-direct transition writes the baseline.
  assert by_alias["login-a"]["status"] == core.STATUS_OK
  assert by_alias["login-a"]["enrolled_observed_at"] is not None
  assert by_alias["gpu-box-1"]["enrolled_observed_at"] is None

  baseline = by_alias["login-a"]["enrolled_observed_at"]
  state = await core.run_round(force=True, state_path=state_path, ssh_config_path=config)
  by_alias = {entry["alias"]: entry for entry in state["hosts"]}
  assert by_alias["login-a"]["enrolled_observed_at"] == baseline
