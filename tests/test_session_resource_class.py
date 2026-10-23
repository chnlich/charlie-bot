"""The repair mark: spawn-time resolution through the task tree, real slice placement and OOM priority,
the operator-only API route, and the CLI verb."""

import asyncio
import contextlib
import json
import os
import pathlib
import shutil
import subprocess
import time
import types
import uuid
from unittest import mock

import conftest
import fastapi
import pytest
from fastapi import testclient

from src.infra import config, models, process
from src.runtime import resource_class, run_token
from src.runtime.agent_process import base
from src.runtime.api import deps
from src.runtime.api import sessions as sessions_api
from src.runtime.cli import session

PARENT_ID = "11112222-3333-4444-5555-666677778888"
CHILD_ID = "99998888-7777-6666-5555-444433332222"


def write_metadata(
    sessions: pathlib.Path,
    session_id: str,
    *,
    parent: str | None = None,
    resource_class_mark: str | None = None) -> None:
  """Write the one file the resolver reads: task_parent_id and the optional repair mark."""
  directory = sessions / session_id
  directory.mkdir(parents=True, exist_ok=True)
  payload = {"name": session_id, "profile": "manager", "task_parent_id": parent}
  if resource_class_mark is not None:
    payload["resource_class"] = resource_class_mark
  (directory / "metadata.json").write_text(json.dumps(payload), encoding="utf-8")


# ---------------------------------------------------------------------------
# Resolution: nearest repair mark on the task-parent chain
# ---------------------------------------------------------------------------


def test_resolution_takes_the_nearest_repair_mark_and_follows_a_cleared_parent(tmp_path: pathlib.Path) -> None:
  sessions = tmp_path / "sessions"
  write_metadata(sessions, "root", resource_class_mark="repair")
  write_metadata(sessions, "mid", parent="root")
  write_metadata(sessions, "leaf", parent="mid")

  assert resource_class.resolve_sync("leaf", sessions) == "repair"
  # Clearing the mark on the ancestor moves the descendant back without writing on it.
  write_metadata(sessions, "root")
  assert resource_class.resolve_sync("leaf", sessions) == "agent"
  write_metadata(sessions, "mid", parent="root", resource_class_mark="repair")
  assert resource_class.resolve_sync("leaf", sessions) == "repair"
  assert resource_class.resolve_sync("root", sessions) == "agent"


def test_resolution_stops_on_a_relationship_cycle(tmp_path: pathlib.Path) -> None:
  sessions = tmp_path / "sessions"
  write_metadata(sessions, "a", parent="b")
  write_metadata(sessions, "b", parent="a")
  with pytest.raises(ValueError, match="task-parent cycle"):
    resource_class.resolve_sync("a", sessions)


def test_resolution_fails_when_a_task_parent_is_missing(tmp_path: pathlib.Path) -> None:
  sessions = tmp_path / "sessions"
  write_metadata(sessions, "leaf", parent="missing-parent")
  with pytest.raises(FileNotFoundError, match="missing-parent"):
    resource_class.resolve_sync("leaf", sessions)


# ---------------------------------------------------------------------------
# Real transient slices: the spawn lands where the mark says, with the OOM priority
# ---------------------------------------------------------------------------


def real_uid() -> int:
  """The host user's uid, read from the cgroup slice names.

  The test sandbox runs in a user namespace without a uid map, so ``os.getuid()`` and every
  stat answer 65534; the ``user-<uid>.slice`` directory names are not mapped.
  """
  for path in sorted(pathlib.Path("/sys/fs/cgroup/user.slice").glob("user-*.slice")):
    uid = int(path.name[len("user-"):-len(".slice")])
    if pathlib.Path(f"/run/user/{uid}/bus").exists():
      return uid
  raise RuntimeError("no user systemd slice with a session bus under /sys/fs/cgroup/user.slice")


def user_service_dir() -> pathlib.Path:
  uid = real_uid()
  return pathlib.Path(f"/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service")


def systemd_env() -> dict[str, str] | None:
  """The invoking environment plus the user bus the test sandbox strips from it."""
  bus = f"/run/user/{real_uid()}/bus"
  if not pathlib.Path(bus).exists():
    return None
  return {**os.environ, "DBUS_SESSION_BUS_ADDRESS": f"unix:path={bus}"}


def systemctl(env: dict[str, str], *args: str) -> subprocess.CompletedProcess[str]:
  return subprocess.run(["systemctl", "--user", *args], env=env, capture_output=True, text=True, check=False)


@pytest.fixture
def transient_slices(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path) -> dict[str, pathlib.Path]:
  """Two live ``cbtest-`` slices under the user manager, stopped and removed at teardown.

  A host without a reachable user systemd manager skips: the slices are the host-local
  resource this test needs.
  """
  env = systemd_env()
  if env is None or shutil.which("systemd-run") is None:
    pytest.skip("no systemd user manager bus on this host")
  token = uuid.uuid4().hex[:8]
  names = (f"cbtest-{token}-agents.slice", f"cbtest-{token}-repair.slice")
  holders: list[subprocess.Popen[str]] = []
  paths: dict[str, pathlib.Path] = {}
  try:
    for index, name in enumerate(names):
      holders.append(
          subprocess.Popen(
              [
                  "systemd-run", "--user", "--scope", f"--unit=cbtest-{token}-hold{index}", f"--slice={name}", "sleep",
                  "120"
              ],
              stdout=subprocess.DEVNULL,
              stderr=subprocess.PIPE,
              text=True,
              env=env))
    # The top-level name's first dash nests everything below cbtest.slice.
    parent = user_service_dir() / "cbtest.slice" / f"cbtest-{token}.slice"
    deadline = time.monotonic() + 5
    for key, name in zip(("agent", "repair"), names, strict=True):
      path = parent / name
      while not path.is_dir():
        if time.monotonic() > deadline:
          pytest.skip("could not create a transient systemd slice on this host")
        time.sleep(0.02)
      paths[key] = path
    app_slice = tmp_path / "app.slice"
    app_slice.mkdir()
    monkeypatch.setattr(process, "CGROUP_V2_AGENTS_SLICE", str(paths["agent"]))
    monkeypatch.setattr(process, "CGROUP_V2_REPAIR_SLICE", str(paths["repair"]))
    monkeypatch.setattr(process, "CGROUP_V2_APP_SLICE", str(app_slice))
    yield paths
  finally:
    for holder in holders:
      holder.terminate()
      try:
        holder.wait(timeout=5)
      except subprocess.TimeoutExpired:
        holder.kill()
        holder.wait()
    for path in paths.values():
      for child in path.iterdir():
        if child.name.startswith(process.SESSION_CGROUP_PREFIX):
          with contextlib.suppress(OSError):
            child.rmdir()
    if env is not None:
      for name in (*names, f"cbtest-{token}.slice"):
        systemctl(env, "stop", name)
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


def test_spawn_follows_the_nearest_repair_mark(
    transient_slices: dict[str, pathlib.Path], tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The acceptance case: agents share, then repair under a marked parent, then agents after the mark clears."""
  sessions = tmp_path / "sessions"
  write_metadata(sessions, PARENT_ID)
  write_metadata(sessions, CHILD_ID, parent=PARENT_ID)
  cfg = types.SimpleNamespace(
      sessions_dir=sessions,
      server=types.SimpleNamespace(session_memory_max_mb=64, session_swap_max_mb=0, session_tasks_max=64))
  monkeypatch.setattr(config, "get_config", lambda: cfg)
  name = f"{process.SESSION_CGROUP_PREFIX}{CHILD_ID[:8]}"
  # /proc/<pid>/cgroup names the path from the cgroup mount root, which drops /sys/fs/cgroup.
  agent_suffix = f"{transient_slices['agent'].parent.name}/{transient_slices['agent'].name}/{name}"
  repair_suffix = f"{transient_slices['repair'].parent.name}/{transient_slices['repair'].name}/{name}"
  try:
    _, cgroup_line, oom = asyncio.run(spawn_and_read(CHILD_ID))
    assert agent_suffix in cgroup_line
    assert oom == "500"

    write_metadata(sessions, PARENT_ID, resource_class_mark="repair")
    _, cgroup_line, oom = asyncio.run(spawn_and_read(CHILD_ID))
    assert repair_suffix in cgroup_line
    assert oom == "100"

    write_metadata(sessions, PARENT_ID)
    _, cgroup_line, oom = asyncio.run(spawn_and_read(CHILD_ID))
    assert agent_suffix in cgroup_line
    assert oom == "500"
  finally:
    process.cleanup_session_cgroup(CHILD_ID)


@pytest.mark.asyncio
async def test_spawn_fails_with_the_slice_name_when_the_selected_slice_is_missing(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  sessions = tmp_path / "sessions"
  write_metadata(sessions, PARENT_ID)
  cfg = types.SimpleNamespace(
      sessions_dir=sessions,
      server=types.SimpleNamespace(session_memory_max_mb=64, session_swap_max_mb=0, session_tasks_max=64))
  monkeypatch.setattr(config, "get_config", lambda: cfg)
  missing = tmp_path / "missing" / "charliebot-agents.slice"
  monkeypatch.setattr(process, "CGROUP_V2_AGENTS_SLICE", str(missing))

  backend = SpawnBackend(cgroup_session_id=PARENT_ID)
  with pytest.raises(process.SessionCgroupUnavailableError, match=r"charliebot-agents\.slice"):
    await backend._spawn_one_shot_subprocess(["true"], {}, pdeathsig=False)


# ---------------------------------------------------------------------------
# API: operator only
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_resource_class_route_refuses_an_agent_run_and_accepts_the_operator(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root")
  key = "op-secret"
  conftest.stub_credentials({"charliebot": {"access_key": key}})
  await tree.runs.register_run_locked(models.RunRecord(id="run-agent", session_id=root.id, kind="manager_turn"))
  await tree.runs.record_launch(root.id, "run-agent", pid=424242, pid_start="ps-1")

  app = fastapi.FastAPI()
  app.include_router(sessions_api.router, prefix="/api/sessions")
  conftest.apply_config_overrides(app, cfg)
  conftest.override_session_blocks(app, session_blocks)
  app.dependency_overrides[deps.get_task_manager] = lambda: tree
  app.dependency_overrides[deps.get_run_store] = lambda: tree.runs
  agent_headers = {
      "Authorization":
          "Bearer " +
          run_token.sign_run_token(run_token.RunTokenClaims(run_id="run-agent", session_id=root.id, agent="m"), key)
  }
  with testclient.TestClient(app) as client:
    refused = client.put(
        f"/api/sessions/{root.id}/resource-class", json={"resource_class": "repair"}, headers=agent_headers)
    assert refused.status_code == 403
    assert (await session_blocks.store.get_session(root.id)).resource_class == "agent"

    accepted = client.put(f"/api/sessions/{root.id}/resource-class", json={"resource_class": "repair"})
    assert accepted.status_code == 200
    assert accepted.json() == {"id": root.id, "resource_class": "repair"}
    assert (await session_blocks.store.get_session(root.id)).resource_class == "repair"


# ---------------------------------------------------------------------------
# CLI: prints the API response
# ---------------------------------------------------------------------------


def _mock_cli_config(tmp_path: pathlib.Path) -> mock.MagicMock:
  cfg = mock.MagicMock()
  cfg.server.port = 9443
  cfg.server_base_url = "http://localhost:9443"
  conftest.stub_credentials({"charliebot": {"access_key": ""}})
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  return cfg


@contextlib.contextmanager
def patched_cli_put(cfg: object, argv: list[str], **transport_kw: object):
  """conftest._patched_cli_transport over the PUT verb, mirroring patched_cli_post."""
  yield from conftest._patched_cli_transport("src.runtime.cli.common._request_put", cfg, argv, **transport_kw)


def test_cli_resource_class_prints_the_response(tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _mock_cli_config(tmp_path)
  response = conftest.make_json_response({"id": "sess-1", "resource_class": "repair"})
  with patched_cli_put(cfg, ["session", "resource-class", "sess-1", "repair"], return_value=response) as put_mock:
    session.main()

  assert put_mock.call_count == 1
  assert put_mock.call_args[0][0].endswith("/api/sessions/sess-1/resource-class")
  assert put_mock.call_args[1]["json"] == {"resource_class": "repair"}
  assert json.loads(capsys.readouterr().out) == {"id": "sess-1", "resource_class": "repair"}
