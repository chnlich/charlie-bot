"""Process helpers: session cgroups (config schema, naming, limit files, degradation, attribution, the startup sweep)
and wait_or_kill_group's no-limit path."""

import asyncio
import pathlib

import pytest
from structlog import testing

from src.infra import config, process

SESSION_ID = "abcd1234-ef56-7890-abcd-ef1234567890"

# ---------------------------------------------------------------------------
# Config schema
# ---------------------------------------------------------------------------


def test_server_config_zero_disables_cgroup() -> None:
  cfg = config.ServerConfig(session_memory_max_mb=0, session_swap_max_mb=0)
  assert cfg.session_memory_max_mb == 0
  assert cfg.session_swap_max_mb == 0


# ---------------------------------------------------------------------------
# Ensure: creation, refresh, degradation
# ---------------------------------------------------------------------------


@pytest.fixture
def fake_app_slice(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> pathlib.Path:
  """A temp dir standing in for the delegated app.slice base."""
  base = tmp_path / "app.slice"
  base.mkdir()
  monkeypatch.setattr(process, "CGROUP_V2_APP_SLICE", str(base))
  return base


def test_ensure_session_cgroup_creates_with_limit_files(fake_app_slice: pathlib.Path) -> None:
  path = process.ensure_session_cgroup(SESSION_ID, 64, 32, config.ServerConfig().session_tasks_max)
  assert path == fake_app_slice / "charliebot-sess-abcd1234"
  assert path.is_dir()
  assert (path / "memory.max").read_text() == str(64 * 1024 * 1024)
  assert (path / "memory.swap.max").read_text() == str(32 * 1024 * 1024)
  assert (path / "pids.max").read_text() == "4096"


def test_ensure_session_cgroup_rewrites_task_cap_and_zero_lifts_it(fake_app_slice: pathlib.Path) -> None:
  path = process.ensure_session_cgroup(SESSION_ID, 64, 32, 50)
  assert path is not None
  assert (path / "pids.max").read_text() == "50"
  assert process.ensure_session_cgroup(SESSION_ID, 64, 32, 0) == path
  assert (path / "pids.max").read_text() == "max"


def test_ensure_session_cgroup_degrades_when_base_missing(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  monkeypatch.setattr(process, "CGROUP_V2_APP_SLICE", str(tmp_path / "missing" / "app.slice"))
  assert process.ensure_session_cgroup(SESSION_ID, 64, 32, 4096) is None


# ---------------------------------------------------------------------------
# Startup sweep: only this home's session cgroups
# ---------------------------------------------------------------------------


def test_sweep_removes_only_empty_cgroups_of_this_home(fake_app_slice: pathlib.Path, tmp_path: pathlib.Path) -> None:
  sessions = config.CharlieBotConfig(charliebot_home=tmp_path / "home").sessions_dir
  sessions.mkdir(parents=True)
  populated_id = "11112222-3333-4444-5555-666677778888"
  (sessions / SESSION_ID).mkdir()
  (sessions / populated_id).mkdir()
  own_empty = fake_app_slice / process.session_cgroup_name(SESSION_ID)
  own_empty.mkdir()
  own_populated = fake_app_slice / process.session_cgroup_name(populated_id)
  own_populated.mkdir()
  (own_populated / "cgroup.procs").write_text("4242\n")
  other_home_empty = fake_app_slice / process.session_cgroup_name("99998888-7777-6666-5555-444433332222")
  other_home_empty.mkdir()
  non_session = fake_app_slice / "app-chrome.scope"
  non_session.mkdir()

  with testing.capture_logs() as logs:
    assert process.sweep_stale_session_cgroups(sessions) == 1

  assert not own_empty.exists()
  assert own_populated.is_dir()
  assert other_home_empty.is_dir()
  assert non_session.is_dir()
  removed = [entry for entry in logs if entry["event"] == "session_cgroup_sweep_removed"]
  assert [(entry["count"], entry["names"]) for entry in removed] == [(1, ["charliebot-sess-abcd1234"])]


# ---------------------------------------------------------------------------
# memory.events reading and exit attribution
# ---------------------------------------------------------------------------


def test_classify_cap_kill_on_max_growth() -> None:
  msg = process.classify_cgroup_exit(-9, (1, 0), (2, 0), 12288)
  assert msg is not None
  assert "session 内存上限触发" in msg
  assert "12288" in msg
  assert "gpuq" in msg


# ---------------------------------------------------------------------------
# wait_or_kill_group: timeout=None waits without a limit
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_wait_or_kill_group_without_a_limit_returns_the_result() -> None:

  async def answer() -> str:
    await asyncio.sleep(0.05)
    return "done"

  stderr_task = asyncio.create_task(asyncio.sleep(3600))
  assert await process.wait_or_kill_group(answer(), None, -1, stderr_task) == "done"
  assert stderr_task.cancelled()
