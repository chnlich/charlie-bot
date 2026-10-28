"""Process helpers: session cgroups (config schema, naming, the agents' slice, limit files, the missing-slice
      failure, OOM priority, attribution, the startup sweep), real-slice placement, and wait_or_kill_group's
      no-limit path."""

import asyncio
import os
import pathlib
import shutil
import subprocess
import time
import types
import uuid

import pytest
from structlog import testing

from src.backends.opencode import server as opencode_server
from src.infra import config, process
from src.runtime.agent_process import base

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
  """Temp dirs standing in for the agents' slice and the retired app.slice."""
  service = tmp_path / "user@1001.service"
  slices = {
      "agents": service / "charliebot-agents.slice",
      "app": service / "app.slice",
  }
  for path in slices.values():
    path.mkdir(parents=True)
  monkeypatch.setattr(process, "CGROUP_V2_AGENTS_SLICE", str(slices["agents"]))
  monkeypatch.setattr(process, "CGROUP_V2_APP_SLICE", str(slices["app"]))
  return slices


def test_ensure_session_cgroup_creates_with_limit_files_under_the_agents_slice(
    fake_slices: dict[str, pathlib.Path]) -> None:
  path = process.ensure_session_cgroup(SESSION_ID, 64, 32, config.ServerConfig().session_tasks_max)
  assert path == fake_slices["agents"] / "charliebot-sess-abcd1234"
  assert path.is_dir()
  assert (fake_slices["agents"] / "cgroup.subtree_control").read_text() == "+memory +pids"
  assert (path / "memory.max").read_text() == str(64 * 1024 * 1024)
  assert (path / "memory.swap.max").read_text() == str(32 * 1024 * 1024)
  assert (path / "pids.max").read_text() == "4096"


def test_ensure_session_cgroup_rewrites_task_cap_and_zero_lifts_it(fake_slices: dict[str, pathlib.Path]) -> None:
  path = process.ensure_session_cgroup(SESSION_ID, 64, 32, 50)
  assert (path / "pids.max").read_text() == "50"
  assert process.ensure_session_cgroup(SESSION_ID, 64, 32, 0) == path
  assert (path / "pids.max").read_text() == "max"


def test_user_slice_dir_nests_under_each_dash_prefix() -> None:
  """systemd places a-b.slice inside a.slice, so charliebot-agents.slice lives in charliebot.slice."""
  base_dir = process.CGROUP_V2_USER_SERVICE
  assert process.user_slice_dir("app.slice") == f"{base_dir}/app.slice"
  assert process.user_slice_dir("charliebot-agents.slice") == f"{base_dir}/charliebot.slice/charliebot-agents.slice"
  assert process.user_slice_dir("a-b-c.slice") == f"{base_dir}/a.slice/a-b.slice/a-b-c.slice"


def test_ensure_session_cgroup_names_the_missing_slice(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> None:
  """A missing slice fails the spawn visibly instead of running it unbounded."""
  missing = tmp_path / "user@1001.service" / "charliebot-agents.slice"
  monkeypatch.setattr(process, "CGROUP_V2_AGENTS_SLICE", str(missing))
  with pytest.raises(process.SessionCgroupUnavailableError, match=r"charliebot-agents\.slice"):
    process.ensure_session_cgroup(SESSION_ID, 64, 32, 4096)


def test_ensure_session_cgroup_names_the_unwritable_slice(fake_slices: dict[str, pathlib.Path]) -> None:
  fake_slices["agents"].chmod(0o500)
  try:
    with pytest.raises(process.SessionCgroupUnavailableError, match=r"charliebot-agents\.slice"):
      process.ensure_session_cgroup(SESSION_ID, 64, 32, 4096)
  finally:
    fake_slices["agents"].chmod(0o755)


def test_session_cgroup_oom_priority_is_the_same_for_every_agent_process() -> None:
  assert process.SESSION_CGROUP_OOM_SCORE_ADJ == 500


def test_opencode_join_script_moves_the_shell_and_sets_its_oom_priority(tmp_path: pathlib.Path) -> None:
  """The opencode shells join their session cgroup through the join script, which sets the OOM priority too."""
  procs = tmp_path / "cgroup.procs"
  procs.write_text("", encoding="utf-8")
  # A private copy carries the checkout's script bytes at a mode bash accepts as BASH_ENV: a
  # group-writable file is ignored when the run sits in a user namespace.
  join_script = tmp_path / "join.sh"
  join_script.write_text(opencode_server.JOIN_SCRIPT.read_text(encoding="utf-8"), encoding="utf-8")
  join_script.chmod(0o755)
  env = {
      **os.environ,
      "BASH_ENV": str(join_script),
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
  # The same home's cgroup in the agents' slice and in the pre-split app.slice, both cleared.
  agents_empty = fake_slices["agents"] / process.session_cgroup_name(SESSION_ID)
  agents_empty.mkdir()
  app_empty = fake_slices["app"] / process.session_cgroup_name(SESSION_ID)
  app_empty.mkdir()
  own_populated = fake_slices["agents"] / process.session_cgroup_name(populated_id)
  own_populated.mkdir()
  (own_populated / "cgroup.procs").write_text("4242\n")
  other_home_empty = fake_slices["app"] / process.session_cgroup_name("99998888-7777-6666-5555-444433332222")
  other_home_empty.mkdir()
  non_session = fake_slices["agents"] / "app-chrome.scope"
  non_session.mkdir()

  with testing.capture_logs() as logs:
    assert process.sweep_stale_session_cgroups(sessions) == 2

  assert not agents_empty.exists()
  assert not app_empty.exists()
  assert own_populated.is_dir()
  assert other_home_empty.is_dir()
  assert non_session.is_dir()
  removed = [entry for entry in logs if entry["event"] == "session_cgroup_sweep_removed"]
  assert [(entry["count"], entry["names"]) for entry in removed] == [(2, ["charliebot-sess-abcd1234"] * 2)]


# ---------------------------------------------------------------------------
# memory.events reading and exit attribution
# ---------------------------------------------------------------------------


def test_classify_cap_kill_on_max_growth() -> None:
  """A session-cap kill grows the session cgroup's own ``max`` counter."""
  msg = process.classify_cgroup_exit(-9, (0, 0), (1, 0), 12288)
  assert msg is not None
  assert "session 内存上限触发" in msg
  assert "12288" in msg
  assert "gpuq" in msg


def test_classify_group_limit_kill_on_oom_kill_growth_with_a_flat_max() -> None:
  """A kill by the agents' share (or a host-wide OOM) leaves the session's ``max`` at 0 and grows oom_kill."""
  msg = process.classify_cgroup_exit(-9, (0, 0), (0, 1), 12288)
  assert msg == "进程因内存不足被内核结束（agent 组或整机内存用满；session 内存上限 12288 MB 未触发）"


# ---------------------------------------------------------------------------
# Real transient slice: the spawn lands under the agents' slice with its OOM priority
# ---------------------------------------------------------------------------


def user_service_dir(uid: int) -> pathlib.Path:
  """The host user's delegated service cgroup, using its host uid from the slice name."""
  return pathlib.Path(f"/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service")


def systemd_env() -> tuple[int, dict[str, str]] | None:
  """Find the host user's bus and environment; None when no user manager is available."""
  for path in sorted(pathlib.Path("/sys/fs/cgroup/user.slice").glob("user-*.slice")):
    uid = int(path.name[len("user-"):-len(".slice")])
    bus = pathlib.Path(f"/run/user/{uid}/bus")
    if bus.exists():
      return uid, {**os.environ, "DBUS_SESSION_BUS_ADDRESS": f"unix:path={bus}"}
  return None


def systemctl(env: dict[str, str], *args: str) -> subprocess.CompletedProcess[str]:
  return subprocess.run(["systemctl", "--user", *args], env=env, capture_output=True, text=True, check=False)


def own_cgroup() -> pathlib.PurePosixPath:
  """This process's cgroup, as the unified hierarchy's own /proc/self/cgroup line reports it."""
  lines = pathlib.Path("/proc/self/cgroup").read_text(encoding="ascii").splitlines()
  return pathlib.PurePosixPath(next(line for line in lines if line.startswith("0::")).partition("::")[2])


def may_move_into(destination: pathlib.Path) -> bool:
  """Whether cgroup v2 lets this process move a child into *destination*.

  The kernel checks write access on the ``cgroup.procs`` of the source's and the
  destination's common ancestor. A process in a root-owned cgroup shares only the
  root with the user manager's slices, and the root's ``cgroup.procs`` is
  root-owned, so the move there fails with EACCES.
  """
  root = pathlib.Path("/sys/fs/cgroup")
  shared = pathlib.PurePosixPath("/")
  for source_part, target_part in zip(own_cgroup().relative_to("/").parts, destination.relative_to(root).parts,
                                      strict=False):
    if source_part != target_part:
      break
    shared /= source_part
  return os.access(root / shared.relative_to("/") / "cgroup.procs", os.W_OK)


@pytest.fixture
def transient_agents_slice(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> pathlib.Path:
  """A live ``cbtest-`` agents slice under the user manager, stopped and removed at teardown.

  A host without a reachable user systemd manager skips: the slice is the host-local
  resource this test needs. A process running outside that manager's subtree skips too:
  the slice is useless to it, because cgroup v2 refuses the move out of a root-owned
  cgroup such as the system-service cgroup a CI job runs in.
  """
  manager = systemd_env()
  if manager is None or shutil.which("systemd-run") is None:
    pytest.skip("no systemd user manager bus on this host")
  uid, env = manager
  if not may_move_into(user_service_dir(uid)):
    pytest.skip("this process's cgroup is outside the user manager's subtree, so cgroup v2 refuses the move")
  token = uuid.uuid4().hex[:8]
  name = f"cbtest-{token}-agents.slice"
  holder = subprocess.Popen(
      ["systemd-run", "--user", "--scope", f"--unit=cbtest-{token}-hold", f"--slice={name}", "sleep", "120"],
      stdout=subprocess.DEVNULL,
      stderr=subprocess.PIPE,
      text=True,
      env=env)
  # The top-level name's first dash nests everything below cbtest.slice.
  path = user_service_dir(uid) / "cbtest.slice" / f"cbtest-{token}.slice" / name
  try:
    deadline = time.monotonic() + 5
    while not path.is_dir():
      if time.monotonic() > deadline:
        pytest.skip("could not create a transient systemd slice on this host")
      time.sleep(0.02)
    app_slice = tmp_path / "app.slice"
    app_slice.mkdir()
    monkeypatch.setattr(process, "CGROUP_V2_AGENTS_SLICE", str(path))
    monkeypatch.setattr(process, "CGROUP_V2_APP_SLICE", str(app_slice))
    yield path
  finally:
    holder.terminate()
    try:
      holder.wait(timeout=5)
    except subprocess.TimeoutExpired:
      holder.kill()
      holder.wait()
    if path.is_dir():
      for child in path.iterdir():
        if child.name.startswith(process.SESSION_CGROUP_PREFIX):
          child.rmdir()
    for unit in (name, f"cbtest-{token}.slice"):
      systemctl(env, "stop", unit)
    # systemd loads the shared cbtest.slice parent with the first cbtest- slice; stop it once none is left.
    if not systemctl(env, "list-units", "--all", "--no-legend", "--plain", "cbtest-*").stdout.strip():
      systemctl(env, "stop", "cbtest.slice")


class SpawnBackend(base.AgentBackend):
  """A backend that only needs the spawn-time cgroup decision; it launches nothing itself."""

  def _build_command(self, prompt: str) -> list[str]:
    return ["true"]


async def spawn_and_read(session_id: str) -> tuple[process.SessionCgroup, str, str]:
  """Prepare the backend's session cgroup, spawn a child through the preexec, and read where it landed."""
  backend = SpawnBackend(cgroup_session_id=session_id)
  cgroup = await backend._prepare_session_cgroup()
  assert cgroup is not None
  proc = await asyncio.create_subprocess_exec(
      "sleep", "5", start_new_session=True, preexec_fn=process.make_session_cgroup_preexec(cgroup))
  try:
    await asyncio.sleep(0.05)
    cgroup_line = pathlib.Path(f"/proc/{proc.pid}/cgroup").read_text(encoding="utf-8")
    oom = pathlib.Path(f"/proc/{proc.pid}/oom_score_adj").read_text(encoding="ascii").strip()
  finally:
    proc.kill()
    await proc.wait()
  return cgroup, cgroup_line, oom


def test_spawn_lands_in_the_agents_slice_with_its_oom_priority(
    transient_agents_slice: pathlib.Path, tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = types.SimpleNamespace(
      sessions_dir=tmp_path / "sessions",
      server=types.SimpleNamespace(session_memory_max_mb=64, session_swap_max_mb=0, session_tasks_max=64))
  monkeypatch.setattr(config, "get_config", lambda: cfg)
  name = f"{process.SESSION_CGROUP_PREFIX}{SESSION_ID[:8]}"
  # /proc/<pid>/cgroup names the path from the cgroup mount root, which drops /sys/fs/cgroup.
  suffix = f"{transient_agents_slice.parent.name}/{transient_agents_slice.name}/{name}"
  try:
    _, cgroup_line, oom = asyncio.run(spawn_and_read(SESSION_ID))
    assert suffix in cgroup_line
    assert oom == "500"
  finally:
    process.cleanup_session_cgroup(SESSION_ID)


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
