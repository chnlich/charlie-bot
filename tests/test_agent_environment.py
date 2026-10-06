"""The environment the server hands every agent process (src/core/agent_environment.py).

The server runs under `uv run`, whose venv activation every agent process
would copy; uv then installs into the server venv from any checkout. These
tests pin the mapping that removes the activation, the entry-point shims that
keep `charliebot` and `claude-sub` resolvable, and the call in server.main().
"""

import os
import pathlib
import subprocess
import sys
import tomllib
import types

import pytest
import uvicorn

import server
from src.core import agent_environment, constants


def _launcher_env(tmp_path: pathlib.Path) -> tuple[dict[str, str], pathlib.Path, list[str]]:
  """The environment `uv run` gives the server: the activation variables and the
  venv bin on PATH twice, plus one spelling of it through a symlink. Returns the
  environment, the venv bin, and the PATH entries that are not the venv bin."""
  venv = tmp_path / "venv"
  venv_bin = venv / "bin"
  venv_bin.mkdir(parents=True)
  (tmp_path / "venv-link").symlink_to(venv)
  others = [str(tmp_path / "tools"), str(tmp_path / "system")]
  path = [str(venv_bin), others[0], str(venv_bin), str(tmp_path / "venv-link" / "bin"), others[1]]
  env = {
      "VIRTUAL_ENV": str(venv),
      "UV_RUN_RECURSION_DEPTH": "1",
      "PATH": os.pathsep.join(path),
      "UNRELATED": "kept",
  }
  return env, venv_bin, others


def test_agent_environment_drops_the_venv_activation(tmp_path: pathlib.Path) -> None:
  env, venv_bin, others = _launcher_env(tmp_path)
  original = dict(env)

  result = agent_environment.agent_environment(env, venv_bin)

  assert "VIRTUAL_ENV" not in result
  assert "UV_RUN_RECURSION_DEPTH" not in result
  assert result["PATH"].split(os.pathsep) == [str(agent_environment.ENTRY_POINT_DIR), *others]
  assert result[agent_environment.VENV_BIN_ENV_VAR] == str(venv_bin)
  assert result["UNRELATED"] == "kept"
  assert env == original


def test_entry_point_dir_holds_one_executable_shim_per_project_script() -> None:
  scripts = tomllib.loads((constants.REPO_ROOT / "pyproject.toml").read_text())["project"]["scripts"]
  shims = sorted(agent_environment.ENTRY_POINT_DIR.iterdir())

  assert [shim.name for shim in shims] == sorted(scripts)
  assert all(os.access(shim, os.X_OK) for shim in shims)


@pytest.mark.parametrize("name", ["charliebot", "claude-sub"])
def test_shim_execs_the_same_named_venv_script(tmp_path: pathlib.Path, name: str) -> None:
  venv_bin = tmp_path / "bin"
  venv_bin.mkdir()
  stub = venv_bin / name
  stub.write_text("#!/bin/sh\nprintf '%s\\n' \"$(basename \"$0\")\"\nprintf '[%s]\\n' \"$@\"\n")
  stub.chmod(0o755)
  env = {"PATH": os.environ["PATH"], agent_environment.VENV_BIN_ENV_VAR: str(venv_bin)}

  proc = subprocess.run(
      [str(agent_environment.ENTRY_POINT_DIR / name), "plain", "with space"],
      env=env,
      capture_output=True,
      text=True,
      check=True,
  )

  assert proc.stdout == f"{name}\n[plain]\n[with space]\n"


def test_shim_fails_loud_without_the_venv_bin_variable() -> None:
  proc = subprocess.run(
      [str(agent_environment.ENTRY_POINT_DIR / "charliebot"), "--help"],
      env={"PATH": os.environ["PATH"]},
      capture_output=True,
      text=True,
      check=False,
  )

  assert proc.returncode != 0
  assert agent_environment.VENV_BIN_ENV_VAR in proc.stderr
  assert proc.stdout == ""


def test_apply_refuses_an_interpreter_outside_a_venv(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  prefix = str(tmp_path / "python")
  monkeypatch.setattr(sys, "prefix", prefix)
  monkeypatch.setattr(sys, "base_prefix", prefix)
  monkeypatch.setenv("VIRTUAL_ENV", str(tmp_path / "venv"))

  with pytest.raises(RuntimeError, match=prefix):
    agent_environment.apply_agent_environment()

  assert os.environ["VIRTUAL_ENV"] == str(tmp_path / "venv")


def _patch_launcher_process(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> dict[str, str]:
  """Run this process as the server launched from a tmp_path venv; returns the
  environment the apply function must leave. monkeypatch touches every variable
  the apply function changes, so teardown restores each one."""
  env, venv_bin, _ = _launcher_env(tmp_path)
  monkeypatch.setattr(sys, "prefix", str(venv_bin.parent))
  monkeypatch.setattr(sys, "base_prefix", str(tmp_path / "python"))
  monkeypatch.delenv(agent_environment.VENV_BIN_ENV_VAR, raising=False)
  for name, value in env.items():
    monkeypatch.setenv(name, value)
  return agent_environment.agent_environment(os.environ, venv_bin)


def test_apply_rewrites_os_environ(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  expected = _patch_launcher_process(tmp_path, monkeypatch)

  agent_environment.apply_agent_environment()

  assert dict(os.environ) == expected


def test_server_main_applies_the_environment_before_config_and_uvicorn(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  _patch_launcher_process(tmp_path, monkeypatch)
  seen: dict[str, tuple[bool, str]] = {}

  def environment_state() -> tuple[bool, str]:
    return "VIRTUAL_ENV" in os.environ, os.environ["PATH"].split(os.pathsep)[0]

  def fake_get_config() -> types.SimpleNamespace:
    seen["get_config"] = environment_state()
    return types.SimpleNamespace(server=types.SimpleNamespace(host="127.0.0.1", port=0))

  def fake_run(*_args: object, **_kwargs: object) -> None:
    seen["uvicorn.run"] = environment_state()

  monkeypatch.setattr(server, "get_config", fake_get_config)
  monkeypatch.setattr(server, "get_scheduled_tasks", list)
  monkeypatch.setattr(server, "require_backends", lambda _cfg, _tasks: None)
  monkeypatch.setattr(uvicorn, "run", fake_run)

  server.main()

  assert seen == {
      "get_config": (False, str(agent_environment.ENTRY_POINT_DIR)),
      "uvicorn.run": (False, str(agent_environment.ENTRY_POINT_DIR)),
  }
