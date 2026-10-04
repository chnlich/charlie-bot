"""Tests for src/cli/remote_launch.py."""

import contextlib
import json
import os
import shutil
import signal
import subprocess
import time
from collections.abc import Iterator
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock, patch

import pytest
from conftest import CLI_COMMON_SESSIONS_DIR_PATCH_TARGET, CONFIG_GET_CONFIG_PATCH_TARGET, _wait_for

from src.cli.remote_launch import main
from src.core.constants import SESSION_ID_ENV_VAR

# Import-path patch target for remote_launch's subprocess seam. subprocess.run is reached
# through the launch path's function-local `import subprocess`, which resolves the same
# shared subprocess module, so its stand-in lands on that module's run attribute. The
# config and sessions-root routes (CONFIG_GET_CONFIG_PATCH_TARGET and
# CLI_COMMON_SESSIONS_DIR_PATCH_TARGET in conftest) carry their deferred-import mechanism
# at their definition.
_SUBPROCESS_RUN_PATCH_TARGET = "subprocess.run"


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
  """Install the patch stack every launch test shares: argv, the config read, the sessions root, and run_patch.

  Yields the subprocess.run stand-in when run_patch is given, else None. Why each stand-in
  lands on its module attribute: the patch-target comments at each target's definition.
  """
  patches = [
      patch("sys.argv", ["remote_launch", *argv_tail]),
      patch(CLI_COMMON_SESSIONS_DIR_PATCH_TARGET, return_value=cfg.sessions_dir),
      patch(CONFIG_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ]
  if run_patch is not None:
    patches.append(run_patch)
  with contextlib.ExitStack() as stack:
    handles = [stack.enter_context(p) for p in patches]
    yield handles[-1] if run_patch is not None else None


def _run_e2e(tmp_path: Path, capsys: pytest.CaptureFixture[str], host: str) -> tuple[dict, Path, str]:
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
    _wait_for(lambda: (remote_dir / "sentinel").exists(), 5, "the remote launch sentinel never appeared")
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

  with _patched_launch(cfg, ["--session", session, "--host", "nonexistent.invalid", "--cwd", str(tmp_path), "--cmd",
                             "echo hi"]), pytest.raises(SystemExit) as exc_info:
    main()

  assert exc_info.value.code == 2
  err = capsys.readouterr().err
  assert "ssh" in err.lower()


def test_pid_parse_failure_exits_3(tmp_path: Path) -> None:
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


@pytest.mark.parametrize(
    ("session_source", "session"),
    [
        ("cwd", "sess-cwd"),
        ("env", "sess-env"),
    ],
    ids=["cwd-derived", "env-outranking-cwd"],
)
def test_remote_launch_resolves_session_id(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    session_source: str,
    session: str,
) -> None:
  """The launch metadata's session id comes from the shared source ladder: a cwd
  inside the session dir derives it; from a cwd outside any session dir, the
  server-written variable supplies it."""
  cfg = _mock_config(_make_session_dir(tmp_path, session))
  if session_source == "cwd":
    monkeypatch.chdir(cfg.sessions_dir / session)
    monkeypatch.delenv(SESSION_ID_ENV_VAR, raising=False)
  else:
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv(SESSION_ID_ENV_VAR, session)

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
