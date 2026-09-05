"""Tests for src/cli/remote_launch.py."""

import contextlib
import json
import os
import shlex
import shutil
import signal
import subprocess
import time
from collections.abc import Iterator
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock, patch

import pytest
from conftest import CLI_COMMON_GET_CONFIG_PATCH_TARGET

from src.cli.remote_launch import main

# Import-path patch targets for the remote_launch seams. src/cli/remote_launch.py binds
# get_config at import scope (`from src.core.config import get_config`) and reaches
# subprocess.run through its module-scope `import subprocess`, so patch() lands each
# stand-in on the src.cli.remote_launch module attribute and main() reads them at call
# time; the src.cli.common helpers bind get_config in their own namespace
# (CLI_COMMON_GET_CONFIG_PATCH_TARGET), and a drifted string copy of either route would
# patch a name nothing reads.
_GET_CONFIG_PATCH_TARGET = "src.cli.remote_launch.get_config"
_SUBPROCESS_RUN_PATCH_TARGET = "src.cli.remote_launch.subprocess.run"


def _has_ssh_localhost() -> bool:
  """Return True if ssh to localhost works without password (BatchMode)."""
  try:
    proc = subprocess.run(
        ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "localhost", "true"],
        capture_output=True,
        check=False,
        timeout=10,
    )
    return proc.returncode == 0
  except (subprocess.TimeoutExpired, FileNotFoundError):
    return False


def _make_session_dir(tmp_path: Path, session: str) -> Path:
  home = tmp_path / "home"
  session_dir = home / ".charliebot" / "sessions" / session
  session_dir.mkdir(parents=True)
  return home


def _mock_config(home: Path) -> MagicMock:
  cfg = MagicMock()
  cfg.sessions_dir = home / ".charliebot" / "sessions"
  return cfg


@contextlib.contextmanager
def _patched_launch(cfg: MagicMock, argv_tail: list[str], run_patch: Any = None) -> Iterator[Any]:
  """Install the patch stack every launch test shares: argv, both get_config bindings, and run_patch.

  Yields the subprocess.run stand-in when run_patch is given, else None. Why two get_config
  bindings are patched: see the patch-target comment above.
  """
  patches = [
      patch("sys.argv", ["remote_launch", *argv_tail]),
      patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg),
      patch(_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ]
  if run_patch is not None:
    patches.append(run_patch)
  with contextlib.ExitStack() as stack:
    handles = [stack.enter_context(p) for p in patches]
    yield handles[-1] if run_patch is not None else None


def _run_e2e(tmp_path: Path, capsys: pytest.CaptureFixture[str], host: str):
  """Drive main() with the supplied ssh argv prefix and return parsed metadata."""
  session = "sess-e2e"
  home = _make_session_dir(tmp_path, session)
  cfg = _mock_config(home)
  cwd = str(tmp_path)

  with _patched_launch(cfg, ["--session", session, "--host", host, "--cwd", cwd, "--cmd", "sleep 2; echo hi"]):
    main()

  meta = json.loads(capsys.readouterr().out.strip())
  return meta, home, session


@pytest.mark.skipif(not _has_ssh_localhost(), reason="ssh localhost not available without password")
@pytest.mark.local_only
def test_end_to_end_localhost(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  meta, home, session = _run_e2e(tmp_path, capsys, host="localhost")

  assert set(meta.keys()) == {"launch_id", "session_id", "host", "remote_pid", "cwd", "cmd", "started_at"}
  assert meta["session_id"] == session
  assert meta["host"] == "localhost"
  assert meta["cmd"] == "sleep 2; echo hi"
  assert isinstance(meta["remote_pid"], int)
  assert meta["started_at"].endswith("Z") or meta["started_at"].endswith("+00:00")

  launch_dir = home / ".charliebot" / "sessions" / session / "launches" / meta["launch_id"]
  assert (launch_dir / "metadata.json").exists()
  assert json.loads((launch_dir / "metadata.json").read_text()) == meta

  remote_dir = Path(f"/tmp/charliebot_runs/{meta['launch_id']}")
  try:
    time.sleep(0.3)
    os.kill(meta["remote_pid"], 0)
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline and not (remote_dir / "sentinel").exists():
      time.sleep(0.1)
    assert (remote_dir / "log").exists()
    assert (remote_dir / "sentinel").exists()
    assert (remote_dir / "sentinel").read_text().strip() == "0"
  finally:
    if not (remote_dir / "sentinel").exists():
      with contextlib.suppress(ProcessLookupError):
        os.kill(meta["remote_pid"], signal.SIGKILL)
    shutil.rmtree(remote_dir, ignore_errors=True)


def test_ssh_failure_exits_2(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  session = "sess-ssh-fail"
  cfg = _mock_config(_make_session_dir(tmp_path, session))

  with _patched_launch(cfg, [
      "--session", session, "--host", "nonexistent.invalid", "--cwd", str(tmp_path), "--cmd", "echo hi"
  ]), pytest.raises(SystemExit) as exc_info:
    main()

  assert exc_info.value.code == 2
  err = capsys.readouterr().err
  assert "ssh" in err.lower()


def test_missing_session_dir_exits_4(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  # Create a home dir without the requested session subdirectory.
  home = tmp_path / "home"
  (home / ".charliebot" / "sessions").mkdir(parents=True)
  cfg = _mock_config(home)

  fake_proc = subprocess.CompletedProcess(args=[], returncode=0, stdout="12345\n", stderr="")

  with _patched_launch(
      cfg,
      ["--session", "nonexistent-session", "--host", "localhost", "--cwd", str(tmp_path), "--cmd", "echo hi"],
      patch(_SUBPROCESS_RUN_PATCH_TARGET, return_value=fake_proc),
  ), pytest.raises(SystemExit) as exc_info:
    main()

  assert exc_info.value.code == 4
  err = capsys.readouterr().err
  assert "session dir" in err


def test_pid_parse_failure_exits_3(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  session = "sess-bad-pid"
  cfg = _mock_config(_make_session_dir(tmp_path, session))

  fake_proc = subprocess.CompletedProcess(args=[], returncode=0, stdout="not-a-pid\n", stderr="")

  with _patched_launch(
      cfg,
      ["--session", session, "--host", "localhost", "--cwd", str(tmp_path), "--cmd", "echo hi"],
      patch(_SUBPROCESS_RUN_PATCH_TARGET, return_value=fake_proc),
  ), pytest.raises(SystemExit) as exc_info:
    main()

  assert exc_info.value.code == 3


def test_ssh_timeout_exits_2(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  session = "sess-timeout"
  cfg = _mock_config(_make_session_dir(tmp_path, session))

  timeout = subprocess.TimeoutExpired(cmd=["ssh"], timeout=30, stderr="still waiting")

  with _patched_launch(
      cfg,
      ["--session", session, "--host", "localhost", "--cwd", str(tmp_path), "--cmd", "echo hi"],
      patch(_SUBPROCESS_RUN_PATCH_TARGET, side_effect=timeout),
  ), pytest.raises(SystemExit) as exc_info:
    main()

  assert exc_info.value.code == 2
  err = capsys.readouterr().err
  assert "timed out" in err


def test_success_path_with_mocked_ssh(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  session = "sess-mock"
  home = _make_session_dir(tmp_path, session)
  cfg = _mock_config(home)

  fake_proc = subprocess.CompletedProcess(args=[], returncode=0, stdout="98765\n", stderr="")

  # cwd has a space and cmd has a special char so shlex.quote produces visible quoting.
  cwd = "/some where"
  cmd = "make build && echo done"

  with _patched_launch(
      cfg,
      ["--session", session, "--host", "remote.example.com", "--cwd", cwd, "--cmd", cmd],
      patch(_SUBPROCESS_RUN_PATCH_TARGET, return_value=fake_proc),
  ) as mock_run:
    main()

  stdout = capsys.readouterr().out.strip()
  meta = json.loads(stdout)
  assert json.dumps(meta, separators=(",", ":")) == stdout
  assert meta["remote_pid"] == 98765
  assert meta["host"] == "remote.example.com"
  assert meta["cwd"] == cwd
  assert meta["cmd"] == cmd
  assert meta["session_id"] == session
  assert len(meta["launch_id"]) == len("YYYYMMDDTHHMMSS-XXXXXX")

  metadata_file = (home / ".charliebot" / "sessions" / session / "launches" / meta["launch_id"] / "metadata.json")
  assert json.loads(metadata_file.read_text()) == meta

  ssh_argv = mock_run.call_args.args[0]
  assert ssh_argv[0] == "ssh"
  assert "BatchMode=yes" in ssh_argv
  assert "ConnectTimeout=10" in ssh_argv
  assert "remote.example.com" in ssh_argv
  assert ssh_argv[-3:-1] == ["bash", "-c"]
  # OpenSSH joins argv into a remote command string, so the bash -c payload must be quoted.
  wrapper = shlex.split(ssh_argv[-1])[0]
  assert f"/tmp/charliebot_runs/{meta['launch_id']}" in wrapper
  assert "&& { setsid bash -lc" in wrapper
  assert wrapper.endswith("; }")
  # cwd is passed through shlex.quote because it has a space; resulting token is 'some where' quoted.
  assert "'/some where'" in wrapper
  # cmd is embedded inside the inner string which is itself shlex.quote'd.
  assert "make build && echo done" in wrapper


def test_success_path_derives_session_from_cwd(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
  session = "sess-cwd"
  cfg = _mock_config(_make_session_dir(tmp_path, session))
  monkeypatch.chdir(cfg.sessions_dir / session)
  monkeypatch.delenv("CHARLIEBOT_SESSION_ID", raising=False)

  fake_proc = subprocess.CompletedProcess(args=[], returncode=0, stdout="24680\n", stderr="")

  with _patched_launch(
      cfg,
      ["--host", "remote.example.com", "--cwd", str(tmp_path), "--cmd", "echo hi"],
      patch(_SUBPROCESS_RUN_PATCH_TARGET, return_value=fake_proc),
  ):
    main()

  meta = json.loads(capsys.readouterr().out.strip())
  assert meta["session_id"] == session
  assert meta["remote_pid"] == 24680


def test_session_env_is_the_remote_launch_session_source(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
  """From a cwd outside any session dir, the server-written variable supplies the launch's session."""
  session = "sess-env"
  cfg = _mock_config(_make_session_dir(tmp_path, session))
  monkeypatch.chdir(tmp_path)
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", session)

  fake_proc = subprocess.CompletedProcess(args=[], returncode=0, stdout="24680\n", stderr="")

  with _patched_launch(
      cfg,
      ["--host", "remote.example.com", "--cwd", str(tmp_path), "--cmd", "echo hi"],
      patch(_SUBPROCESS_RUN_PATCH_TARGET, return_value=fake_proc),
  ):
    main()

  meta = json.loads(capsys.readouterr().out.strip())
  assert meta["session_id"] == session
  assert meta["remote_pid"] == 24680
