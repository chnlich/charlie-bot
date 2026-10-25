"""Process helpers: session cgroups (config schema, naming, slice choice, limit files, the missing-slice
failure, OOM priorities, attribution, the startup sweep) and wait_or_kill_group's no-limit path."""

import asyncio
import os
import pathlib
import subprocess

import pytest
from structlog import testing

from src.backends.opencode import server as opencode_server
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
def fake_slices(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> dict[str, pathlib.Path]:
  """Temp dirs standing in for the two leaf slices and the retired app.slice."""
  base = tmp_path / "user@1001.service"
  slices = {
      "agent": base / "charliebot-agents.slice",
      "repair": base / "charliebot-repair.slice",
      "app": base / "app.slice",
  }
  for path in slices.values():
    path.mkdir(parents=True)
  monkeypatch.setattr(process, "CGROUP_V2_AGENTS_SLICE", str(slices["agent"]))
  monkeypatch.setattr(process, "CGROUP_V2_REPAIR_SLICE", str(slices["repair"]))
  monkeypatch.setattr(process, "CGROUP_V2_APP_SLICE", str(slices["app"]))
  return slices


def test_ensure_session_cgroup_creates_with_limit_files_under_the_chosen_slice(
    fake_slices: dict[str, pathlib.Path]) -> None:
  path = process.ensure_session_cgroup(SESSION_ID, "agent", 64, 32, config.ServerConfig().session_tasks_max)
  assert path == fake_slices["agent"] / "charliebot-sess-abcd1234"
  assert path.is_dir()
  assert (fake_slices["agent"] / "cgroup.subtree_control").read_text() == "+memory +pids"
  assert (path / "memory.max").read_text() == str(64 * 1024 * 1024)
  assert (path / "memory.swap.max").read_text() == str(32 * 1024 * 1024)
  assert (path / "pids.max").read_text() == "4096"


def test_ensure_session_cgroup_puts_a_repair_session_under_the_repair_slice(
    fake_slices: dict[str, pathlib.Path]) -> None:
  path = process.ensure_session_cgroup(SESSION_ID, "repair", 64, 32, 4096)
  assert path == fake_slices["repair"] / "charliebot-sess-abcd1234"


def test_ensure_session_cgroup_rewrites_task_cap_and_zero_lifts_it(fake_slices: dict[str, pathlib.Path]) -> None:
  path = process.ensure_session_cgroup(SESSION_ID, "agent", 64, 32, 50)
  assert (path / "pids.max").read_text() == "50"
  assert process.ensure_session_cgroup(SESSION_ID, "agent", 64, 32, 0) == path
  assert (path / "pids.max").read_text() == "max"


def test_user_slice_dir_nests_under_each_dash_prefix() -> None:
  """systemd places a-b.slice inside a.slice, so charliebot-agents.slice lives in charliebot.slice."""
  base = process.CGROUP_V2_USER_SERVICE
  assert process.user_slice_dir("app.slice") == f"{base}/app.slice"
  assert process.user_slice_dir("charliebot-agents.slice") == f"{base}/charliebot.slice/charliebot-agents.slice"
  assert process.user_slice_dir("charliebot-repair.slice") == f"{base}/charliebot.slice/charliebot-repair.slice"
  assert process.user_slice_dir("a-b-c.slice") == f"{base}/a.slice/a-b.slice/a-b-c.slice"


def test_ensure_session_cgroup_names_the_missing_slice(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  """A missing slice fails the spawn visibly instead of running it unbounded."""
  missing = tmp_path / "user@1001.service" / "charliebot-agents.slice"
  monkeypatch.setattr(process, "CGROUP_V2_AGENTS_SLICE", str(missing))
  with pytest.raises(process.SessionCgroupUnavailableError, match=r"charliebot-agents\.slice"):
    process.ensure_session_cgroup(SESSION_ID, "agent", 64, 32, 4096)


def test_ensure_session_cgroup_names_the_unwritable_slice(fake_slices: dict[str, pathlib.Path]) -> None:
  fake_slices["repair"].chmod(0o500)
  try:
    with pytest.raises(process.SessionCgroupUnavailableError, match=r"charliebot-repair\.slice"):
      process.ensure_session_cgroup(SESSION_ID, "repair", 64, 32, 4096)
  finally:
    fake_slices["repair"].chmod(0o755)


def test_session_cgroup_oom_priorities() -> None:
  assert process.SESSION_CGROUP_OOM_SCORE_ADJ == {"agent": 500, "repair": 100}


def test_opencode_join_script_moves_the_shell_and_sets_its_oom_priority(tmp_path: pathlib.Path) -> None:
  """The opencode shells join their session cgroup through the join script, which sets the slice's OOM priority too."""
  procs = tmp_path / "cgroup.procs"
  procs.write_text("", encoding="utf-8")
  env = {
      **os.environ,
      "BASH_ENV": str(opencode_server.JOIN_SCRIPT),
      "CHARLIEBOT_SESSION_CGROUP": str(tmp_path),
      "CHARLIEBOT_SESSION_OOM_SCORE_ADJ": "100",
  }
  result = subprocess.run(
      ["bash", "-c", "echo $$; cat /proc/$$/oom_score_adj"], env=env, capture_output=True, text=True, check=True)
  shell_pid, oom = result.stdout.split()
  assert procs.read_text(encoding="utf-8").strip() == shell_pid
  assert oom == "100"


# ---------------------------------------------------------------------------
# Startup sweep: only this home's session cgroups
# ---------------------------------------------------------------------------


def test_sweep_removes_only_empty_cgroups_of_this_home_across_the_slices(
    fake_slices: dict[str, pathlib.Path], tmp_path: pathlib.Path) -> None:
  sessions = config.CharlieBotConfig(charliebot_home=tmp_path / "home").sessions_dir
  sessions.mkdir(parents=True)
  populated_id = "11112222-3333-4444-5555-666677778888"
  (sessions / SESSION_ID).mkdir()
  (sessions / populated_id).mkdir()
  # The same home's cgroup in each slice; the pre-split app.slice leftover is cleared too.
  agents_empty = fake_slices["agent"] / process.session_cgroup_name(SESSION_ID)
  agents_empty.mkdir()
  repair_empty = fake_slices["repair"] / process.session_cgroup_name(SESSION_ID)
  repair_empty.mkdir()
  app_empty = fake_slices["app"] / process.session_cgroup_name(SESSION_ID)
  app_empty.mkdir()
  own_populated = fake_slices["agent"] / process.session_cgroup_name(populated_id)
  own_populated.mkdir()
  (own_populated / "cgroup.procs").write_text("4242\n")
  other_home_empty = fake_slices["repair"] / process.session_cgroup_name("99998888-7777-6666-5555-444433332222")
  other_home_empty.mkdir()
  non_session = fake_slices["agent"] / "app-chrome.scope"
  non_session.mkdir()

  with testing.capture_logs() as logs:
    assert process.sweep_stale_session_cgroups(sessions) == 3

  assert not agents_empty.exists()
  assert not repair_empty.exists()
  assert not app_empty.exists()
  assert own_populated.is_dir()
  assert other_home_empty.is_dir()
  assert non_session.is_dir()
  removed = [entry for entry in logs if entry["event"] == "session_cgroup_sweep_removed"]
  assert [(entry["count"], entry["names"]) for entry in removed] == [(3, ["charliebot-sess-abcd1234"] * 3)]


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
