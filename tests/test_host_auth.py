"""Host login authorization: classification, baseline accounting, estimates, routes, and repo hygiene."""

from __future__ import annotations

import asyncio
import json
import os
import re
import subprocess
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest
from conftest import ROOT, make_page_request

from src.api import host_auth as api
from src.core import host_auth as core

# The four files the leak scanner must cover for this panel; the same list lives
# in scripts/check-skills-host-leak.sh and scripts/git-hooks/pre-commit.
SCAN_LIST = (
    "src/core/host_auth.py",
    "src/api/host_auth.py",
    "web/templates/host_auth.html",
    "tests/test_host_auth.py",
)


def _iso(moment: datetime) -> str:
  return moment.isoformat()


def _entry(alias: str, hostname: str, **overrides: Any) -> dict:
  entry = {
      "alias": alias,
      "hostname": hostname,
      "status": None,
      "detail": "",
      "last_probe_at": None,
      "last_change_at": None,
      "last_ok_at": None,
      "enrolled_observed_at": None,
  }
  entry.update(overrides)
  return entry


def _write_state(home: Path, state: dict) -> None:
  (home / "host_auth.json").write_text(json.dumps(state), encoding="utf-8")


def _flat(body: str) -> str:
  """The page with runs of whitespace collapsed, so prose assertions survive template line wrapping."""
  return " ".join(body.split())


@pytest.fixture(autouse=True)
def _reset_api_round_state() -> Any:
  api._round_running = False
  api._poller.task = None
  yield
  api._round_running = False
  api._poller.task = None


@pytest.fixture
def home_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  """The routes read their state under the CharlieBot home, so point the profile at tmp_path."""
  monkeypatch.setenv("CHARLIEBOT_HOME", str(tmp_path))
  return tmp_path


class _LogRecorder:
  """Record the event names a module logs, so failure paths assert their event."""

  def __init__(self) -> None:
    self.events: list[str] = []

  def info(self, event: str, **fields: Any) -> None:
    self.events.append(event)

  def warning(self, event: str, **fields: Any) -> None:
    self.events.append(event)

  def error(self, event: str, **fields: Any) -> None:
    self.events.append(event)


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


def test_detail_carries_the_first_line_and_the_first_url_line() -> None:
  output = "Dear user,\nsee https://login.example.net/enroll?ticket=7f3c9d21\nlater line https://elsewhere ignored"
  assert core.build_detail(124, output) == "Dear user,\nsee https://login.example.net/enroll?ticket=7f3c9d21"


def test_detail_does_not_duplicate_a_first_line_that_already_carries_the_url() -> None:
  output = "To authenticate, visit: https://login.example.net/a/7f3c9d21"
  assert core.build_detail(1, output) == output


def test_detail_falls_back_to_the_exit_code_when_the_output_is_blank() -> None:
  assert core.build_detail(0, "") == "exit 0"
  assert core.build_detail(255, "\n") == "exit 255"


def test_detail_is_capped() -> None:
  assert len(core.build_detail(1, "x" * 500)) == core.DETAIL_MAX_CHARS


def test_baseline_moves_on_the_held_to_direct_transition_and_not_on_a_repeat() -> None:
  adjacent = _entry("login-a", "login-a.example.internal", status=core.STATUS_NEEDS_OKTA)
  moment = datetime(2026, 9, 15, 8, 5, tzinfo=UTC)
  core.apply_probe_result(adjacent, status=core.STATUS_OK, detail="exit 0", probed_at=moment)
  assert adjacent["enrolled_observed_at"] == _iso(moment)
  repeat = moment + timedelta(minutes=30)
  core.apply_probe_result(adjacent, status=core.STATUS_OK, detail="exit 0", probed_at=repeat)
  assert adjacent["enrolled_observed_at"] == _iso(moment)


@pytest.mark.parametrize("intermediate", [core.STATUS_UNREACHABLE, core.STATUS_NEEDS_INTERACTIVE_AUTH])
def test_passage_through_intermediate_states_does_not_write_the_baseline(intermediate: str) -> None:
  """A held host seen down (or needing a human check) before it answers has no observed transition."""
  entry = _entry("login-a", "login-a.example.internal", status=core.STATUS_NEEDS_OKTA)
  first = datetime(2026, 9, 15, 8, 5, tzinfo=UTC)
  core.apply_probe_result(entry, status=intermediate, detail="not an answer", probed_at=first)
  assert entry["enrolled_observed_at"] is None
  second = first + timedelta(minutes=30)
  core.apply_probe_result(entry, status=core.STATUS_OK, detail="exit 0", probed_at=second)
  assert entry["enrolled_observed_at"] is None


def test_first_sighted_direct_host_earns_no_baseline() -> None:
  entry = _entry("login-b", "login-b.example.internal")
  core.apply_probe_result(entry, status=core.STATUS_OK, detail="exit 0", probed_at=datetime.now(UTC))
  assert entry["enrolled_observed_at"] is None


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


def test_estimate_is_the_baseline_plus_the_trust_cache_lifetime() -> None:
  baseline = datetime(2026, 9, 11, 23, 10, tzinfo=UTC)
  entry = _entry("login-c", "login-c.example.internal", enrolled_observed_at=_iso(baseline))
  now = baseline + timedelta(days=6, hours=11, minutes=24)
  estimate = core.derive_estimate(entry, now)
  assert core.TRUST_CACHE_TTL_SEC == 604800
  assert estimate["estimated_expires_at"] == _iso(baseline + timedelta(seconds=core.TRUST_CACHE_TTL_SEC))
  assert estimate["remaining_sec"] == pytest.approx(12 * 3600 + 36 * 60)


def test_estimate_without_a_baseline_is_none() -> None:
  assert core.derive_estimate(_entry("login-b", "login-b.example.internal"), datetime.now(UTC)) == {
      "estimated_expires_at": None,
      "remaining_sec": None
  }


def test_expired_estimate_keeps_its_baseline_and_goes_negative() -> None:
  """A direct host past its deadline never re-arms from a later probe."""
  baseline = datetime.now(UTC) - timedelta(days=8)
  entry = _entry("login-b", "login-b.example.internal", status=core.STATUS_OK, enrolled_observed_at=_iso(baseline))
  core.apply_probe_result(entry, status=core.STATUS_OK, detail="exit 0", probed_at=datetime.now(UTC))
  assert entry["enrolled_observed_at"] == _iso(baseline)
  assert core.derive_estimate(entry, datetime.now(UTC))["remaining_sec"] < 0


SSH_CONFIG = """# a comment line
Host login-a
  HostName login-a.example.internal
  User ops

Host login-b login-c
  HostName pool.example.internal

Host *
  Compression yes

Host gpu-box-1
  hostname gpu-box-1.example.internal

Host login-d
  # deliberately no HostName

Host !login-e
  HostName login-e.example.internal

Match host login-f
  HostName login-f.example.internal

Host "login-g"
  HostName login-g.example.internal

Host login-h
  HostName=login-h.example.internal
"""


def test_ssh_config_parses_single_name_blocks_only(tmp_path: Path) -> None:
  config = tmp_path / "config"
  config.write_text(SSH_CONFIG, encoding="utf-8")
  assert core.parse_ssh_config_hosts(config) == [
      ("login-a", "login-a.example.internal"),
      ("gpu-box-1", "gpu-box-1.example.internal"),
      ("login-g", "login-g.example.internal"),
      ("login-h", "login-h.example.internal"),
  ]


def test_missing_ssh_config_yields_no_hosts(tmp_path: Path) -> None:
  assert core.parse_ssh_config_hosts(tmp_path / "absent") == []


def test_state_file_lives_under_the_charliebot_home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setenv("CHARLIEBOT_HOME", str(tmp_path))
  assert core.default_state_path() == Path(os.path.realpath(tmp_path)) / "host_auth.json"


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


@pytest.mark.asyncio
async def test_round_recovers_from_a_corrupt_state_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  config = tmp_path / "ssh_config"
  config.write_text("Host login-a\n  HostName login-a.example.internal\n", encoding="utf-8")
  state_path = tmp_path / "state.json"
  state_path.write_text("{not json", encoding="utf-8")

  async def fake_probe(alias: str) -> tuple[int | None, str]:
    return (0, "")

  monkeypatch.setattr(core, "probe_host", fake_probe)
  recorder = _LogRecorder()
  monkeypatch.setattr(core, "log", recorder)

  state = await core.run_round(state_path=state_path, ssh_config_path=config)
  assert "host_auth_state_read_failed" in recorder.events
  assert [entry["alias"] for entry in state["hosts"]] == ["login-a"]
  assert json.loads(state_path.read_text(encoding="utf-8"))["hosts"][0]["status"] == core.STATUS_OK


@pytest.mark.asyncio
async def test_state_write_failure_is_logged_and_raised(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  config = tmp_path / "ssh_config"
  config.write_text("Host login-a\n  HostName login-a.example.internal\n", encoding="utf-8")

  def boom(path: Path, value: object, **kwargs: Any) -> None:
    raise OSError("disk full")

  monkeypatch.setattr(core, "write_json_atomically", boom)
  recorder = _LogRecorder()
  monkeypatch.setattr(core, "log", recorder)

  with pytest.raises(OSError):
    await core.run_round(state_path=tmp_path / "state.json", ssh_config_path=config)
  assert "host_auth_state_write_failed" in recorder.events


def _seed_five_hosts(home: Path) -> None:
  """The preview's snapshot: one held, one direct with slack, one direct under a day, one interactive, one down."""
  now = datetime.now(UTC)
  state = core.empty_state()
  state["probed_at"] = _iso(now)
  state["hosts"] = [
      _entry(
          "login-a",
          "login-a.example.internal",
          status=core.STATUS_NEEDS_OKTA,
          detail=
          "Okta verification required\nTo authenticate, visit: https://login-a.example.internal/activate?user_code=7f3c9d21",
          last_probe_at=_iso(now - timedelta(minutes=1)),
          last_change_at=_iso(now - timedelta(minutes=1))),
      _entry(
          "login-b",
          "login-b.example.internal",
          status=core.STATUS_OK,
          detail="exit 0",
          last_probe_at=_iso(now),
          last_ok_at=_iso(now),
          enrolled_observed_at=_iso(now - timedelta(days=2, hours=3))),
      _entry(
          "login-c",
          "login-c.example.internal",
          status=core.STATUS_OK,
          detail="exit 0",
          last_probe_at=_iso(now),
          last_ok_at=_iso(now),
          enrolled_observed_at=_iso(now - timedelta(days=6, hours=11, minutes=24))),
      _entry(
          "gpu-box-1",
          "gpu-box-1.example.internal",
          status=core.STATUS_NEEDS_INTERACTIVE_AUTH,
          detail=
          "Tailscale SSH requires an additional check.\nTo authenticate, visit: https://login.example.net/a/7f3c9d21",
          last_probe_at=_iso(now)),
      _entry(
          "gpu-box-2",
          "gpu-box-2.example.internal",
          status=core.STATUS_UNREACHABLE,
          detail="ssh: connect to host port 22: Connection timed out",
          last_probe_at=_iso(now)),
  ]
  _write_state(home, state)


@pytest.mark.asyncio
async def test_status_route_returns_the_state_with_derived_fields(home_env: Path) -> None:
  _seed_five_hosts(home_env)
  payload = json.loads((await api.host_auth_status()).body)
  assert payload["interval_sec"] == 1800
  assert payload["ttl_sec"] == 604800
  assert payload["probe_running"] is False
  login_b, login_c = payload["hosts"][1], payload["hosts"][2]
  assert login_b["estimated_expires_at"] > login_b["enrolled_observed_at"]
  assert login_b["remaining_sec"] == pytest.approx(4 * 86400 + 21 * 3600, abs=60)
  assert login_c["remaining_sec"] == pytest.approx(12 * 3600 + 36 * 60, abs=60)
  assert payload["hosts"][0]["estimated_expires_at"] is None
  assert payload["hosts"][0]["remaining_sec"] is None


@pytest.mark.asyncio
async def test_status_route_without_a_state_file_answers_the_empty_state(home_env: Path) -> None:
  assert json.loads((await api.host_auth_status()).body) == core.empty_state()


@pytest.mark.asyncio
async def test_page_renders_five_rows_with_the_renewal_loop(home_env: Path) -> None:
  _seed_five_hosts(home_env)
  response = await api.host_auth_page(make_page_request("/host-auth"))
  assert response.status_code == 200
  body = response.body.decode("utf-8")
  flat = _flat(body)
  assert body.count('class="alias"') == 5
  assert "Needs Okta" in flat and "Needs interactive auth" in flat and "Unreachable" in flat and "Direct" in flat
  assert '<code>ssh login-a true</code>' in flat
  assert "the countdown starts immediately" in flat
  # Blocked hosts are named first, then the soonest estimable host with its countdown.
  soonest = re.search(r"login-c, in 12 h \d\d m", flat)
  assert soonest, flat[flat.index("Blocked now:"):flat.index("Blocked now:") + 200]
  assert flat.index("Blocked now:") < soonest.start()
  assert "(estimate, upper bound)" in flat
  assert 'class="cd amber"' in body
  assert "Baseline: renewal seen" in flat
  assert "A login pod restart can end it sooner." in flat
  # The standing assumption sits beside the countdown column.
  assert "renewed through the browser enrollment flow" in flat
  assert 'http-equiv="refresh"' not in body


@pytest.mark.asyncio
async def test_page_direct_host_without_a_baseline_is_not_estimated(home_env: Path) -> None:
  now = datetime.now(UTC)
  state = core.empty_state()
  state["hosts"] = [
      _entry(
          "login-b",
          "login-b.example.internal",
          status=core.STATUS_OK,
          detail="exit 0",
          last_probe_at=_iso(now),
          last_ok_at=_iso(now))
  ]
  _write_state(home_env, state)
  flat = _flat((await api.host_auth_page(make_page_request("/host-auth"))).body.decode("utf-8"))
  assert "Not estimated" in flat
  assert "No blocked&rarr;direct transition observed yet" in flat
  assert "or this host is not served by the cache at all" in flat
  assert "No estimate available." in flat


@pytest.mark.asyncio
async def test_page_expired_estimate_stops_counting_and_keeps_the_baseline(home_env: Path) -> None:
  now = datetime.now(UTC)
  baseline = now - timedelta(days=8)
  state = core.empty_state()
  state["hosts"] = [
      _entry(
          "login-b",
          "login-b.example.internal",
          status=core.STATUS_OK,
          detail="exit 0",
          last_probe_at=_iso(now),
          last_ok_at=_iso(now),
          enrolled_observed_at=_iso(baseline))
  ]
  _write_state(home_env, state)
  body = (await api.host_auth_page(make_page_request("/host-auth"))).body.decode("utf-8")
  flat = _flat(body)
  assert "ESTIMATE EXPIRED" in flat
  assert "a renewal probably happened between two probes" in flat
  assert 'class="cd' not in body
  payload = json.loads((await api.host_auth_status()).body)
  assert payload["hosts"][0]["estimated_expires_at"] == _iso(baseline + timedelta(seconds=core.TRUST_CACHE_TTL_SEC))
  assert payload["hosts"][0]["remaining_sec"] < 0


@pytest.mark.asyncio
async def test_page_meta_refreshes_while_a_round_is_in_flight(home_env: Path) -> None:
  state = core.empty_state()
  state["probe_running"] = True
  _write_state(home_env, state)
  body = (await api.host_auth_page(make_page_request("/host-auth"))).body.decode("utf-8")
  assert 'http-equiv="refresh"' in body


@pytest.mark.asyncio
async def test_probe_route_starts_a_forced_round_and_redirects(monkeypatch: pytest.MonkeyPatch) -> None:
  calls: list[bool] = []
  started = asyncio.Event()

  async def fake_round(**kwargs: Any) -> dict:
    calls.append(kwargs.get("force"))
    started.set()
    return core.empty_state()

  monkeypatch.setattr(api, "run_round", fake_round)
  response = await api.host_auth_probe()
  assert response.status_code == 303
  assert response.headers["location"] == "/host-auth"
  await asyncio.wait_for(started.wait(), 1)
  assert calls == [True]


@pytest.mark.asyncio
async def test_probe_route_reuses_a_round_already_in_flight(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(api, "_round_running", True)

  async def fail_round(**kwargs: Any) -> dict:
    raise AssertionError("a round in flight must be reused, not doubled")

  monkeypatch.setattr(api, "run_round", fail_round)
  response = await api.host_auth_probe()
  assert response.status_code == 303


@pytest.mark.asyncio
async def test_poller_round_yields_to_a_manual_round(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(api, "_round_running", True)

  async def fail_round(**kwargs: Any) -> dict:
    raise AssertionError("the poller must not run over a manual round")

  monkeypatch.setattr(api, "run_round", fail_round)
  await api._poller_round()


@pytest.mark.asyncio
async def test_poll_loop_runs_one_round_then_sleeps(monkeypatch: pytest.MonkeyPatch) -> None:
  calls: list[bool] = []

  async def fake_round(**kwargs: Any) -> dict:
    calls.append(kwargs.get("force"))
    return core.empty_state()

  monkeypatch.setattr(api, "run_round", fake_round)
  with pytest.raises(TimeoutError):
    await asyncio.wait_for(api._poll_loop(), timeout=0.05)
  assert calls == [False]


@pytest.mark.asyncio
async def test_start_and_stop_poller(monkeypatch: pytest.MonkeyPatch) -> None:
  calls: list[int] = []

  async def fake_round(**kwargs: Any) -> dict:
    calls.append(1)
    return core.empty_state()

  monkeypatch.setattr(api, "run_round", fake_round)
  await api.start_poller()
  await asyncio.sleep(0.01)
  assert calls
  await api.stop_poller()
  assert api._poller.task is None


def test_leak_scan_list_covers_the_host_auth_files() -> None:
  for relative in ("scripts/check-skills-host-leak.sh", "scripts/git-hooks/pre-commit"):
    text = (ROOT / relative).read_text(encoding="utf-8")
    for scanned in SCAN_LIST:
      assert scanned in text, f"{relative} does not scan {scanned}"


def _run_scanner(patterns_file: Path) -> subprocess.CompletedProcess[str]:
  env = {**os.environ, "EXTRA_PATTERNS_FILE": str(patterns_file)}
  return subprocess.run(
      ["bash", str(ROOT / "scripts/check-skills-host-leak.sh"), *SCAN_LIST],
      cwd=ROOT,
      capture_output=True,
      text=True,
      env=env,
      timeout=120,
  )


def test_leak_scanner_passes_on_the_host_auth_files(tmp_path: Path) -> None:
  patterns = tmp_path / "patterns.txt"
  patterns.write_text("", encoding="utf-8")
  proc = _run_scanner(patterns)
  assert proc.returncode == 0, proc.stdout + proc.stderr


def test_leak_scanner_reaches_the_host_auth_files(tmp_path: Path) -> None:
  """A pattern drawn from the page itself must trip the scan, proving the files are not skipped."""
  patterns = tmp_path / "patterns.txt"
  patterns.write_text("Host login authorization\n", encoding="utf-8")
  proc = _run_scanner(patterns)
  assert proc.returncode == 1, proc.stdout + proc.stderr
  assert "web/templates/host_auth.html" in proc.stdout


def test_trust_cache_constant_is_the_local_derivation_without_source_coordinates() -> None:
  source = (ROOT / "src/core/host_auth.py").read_text(encoding="utf-8")
  match = re.search(r"^TRUST_CACHE_TTL_SEC = 7 \* 86400.*$", source, re.MULTILINE)
  assert match, "the trust-cache constant must be written as the 7 x 86400 derivation"
  line = match.group(0)
  assert "604800" in line
  assert "/" not in line and ".yaml" not in line
