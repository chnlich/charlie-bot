"""Tests for `charliebot schedule-trigger --cmd`: start detached + register the wake in one step.

post_internal_api is doubled (no server); the local launcher starts real short-lived wrappers.
The registering double persists the trigger file the way the server would, so the CLI's own
readback can find it.
"""

from __future__ import annotations

import contextlib
import json
import os
import pathlib
import subprocess
import sys
import types
from typing import Any
from unittest import mock

import conftest
import pytest

from src.runtime.cli import schedule_trigger

POST_PATCH_TARGET = "src.runtime.cli.common.post_internal_api"


def _cfg(tmp_path: pathlib.Path) -> types.SimpleNamespace:
  return types.SimpleNamespace(sessions_dir=tmp_path / "sessions")


def _argv(*extra: str) -> list[str]:
  return ["schedule_trigger", "--session", "s1", "--max-wait", "60", "--message", "m", *extra]


def _patch_cli(monkeypatch: pytest.MonkeyPatch, cfg: types.SimpleNamespace, argv: list[str]) -> None:
  monkeypatch.setattr("sys.argv", argv)
  monkeypatch.setattr(conftest.CLI_COMMON_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.delenv("CHARLIEBOT_SESSION_ID", raising=False)


def _metadata(cfg: types.SimpleNamespace, session_id: str) -> dict:
  metas = list((cfg.sessions_dir / session_id / "launches").glob("*/metadata.json"))
  assert len(metas) == 1
  return json.loads(metas[0].read_text())


# ---------------------------------------------------------------------------
# Success: the launch record, the registered target, the exit file
# ---------------------------------------------------------------------------


def test_cmd_exit_3_writes_the_launch_record_and_the_exit_file(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _cfg(tmp_path)
  _patch_cli(monkeypatch, cfg, _argv("--cmd", "exit 3"))
  calls: list[dict] = []
  monkeypatch.setattr(POST_PATCH_TARGET, conftest.registering_post(cfg, "s1", calls))

  schedule_trigger.main()

  out = json.loads(capsys.readouterr().out)
  meta = _metadata(cfg, "s1")
  assert set(meta) == {
      "launch_id", "session_id", "host", "pid", "cwd", "cmd", "started_at", "log", "exit_file", "trigger_id"
  }
  assert meta["session_id"] == "s1"
  assert meta["host"] is None
  assert isinstance(meta["pid"], int)
  assert meta["cwd"] == os.getcwd()
  assert meta["cmd"] == "exit 3"
  assert meta["started_at"].endswith("Z")
  assert meta["trigger_id"] == "trig-1"
  launch_dir = cfg.sessions_dir / "s1" / "launches" / meta["launch_id"]
  assert meta["log"] == str(launch_dir / "log")
  assert meta["exit_file"] == str(launch_dir / "exit")
  # Stdout prints trigger_id, fire_at and the launch record fields as one object.
  assert out == {"trigger_id": "trig-1", "fire_at": "2026-10-08T00:11:00+00:00", **meta}

  try:
    conftest._wait_for(lambda: pathlib.Path(meta["exit_file"]).exists(), 5, "the exit file never appeared")
    assert pathlib.Path(meta["exit_file"]).read_text().strip() == "3"
    conftest.reap_child(meta["pid"])
  finally:
    with contextlib.suppress(ProcessLookupError):
      os.killpg(meta["pid"], 9)


def test_registered_target_carries_exit_file_and_log(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _cfg(tmp_path)
  _patch_cli(monkeypatch, cfg, _argv("--cmd", "exit 0"))
  calls: list[dict] = []
  monkeypatch.setattr(POST_PATCH_TARGET, conftest.registering_post(cfg, "s1", calls))

  schedule_trigger.main()

  meta = _metadata(cfg, "s1")
  try:
    (target,) = calls[0]["payload"]["watch_targets"]
    assert target == {
        "kind": "local_pid",
        "pid": meta["pid"],
        "exit_file": meta["exit_file"],
        "log": meta["log"],
    }
  finally:
    with contextlib.suppress(ProcessLookupError):
      os.killpg(meta["pid"], 9)
    conftest.reap_child(meta["pid"])


# ---------------------------------------------------------------------------
# Usage errors
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("extra", "fragment"),
    [
        (("--cmd", "true", "--watch", "1234"), "mutually exclusive"),
        (("--host", "neptune"), "--host requires --cmd"),
        (("--cwd", "/tmp"), "--cwd requires --cmd"),
    ],
    ids=["cmd-with-watch", "host-without-cmd", "cwd-without-cmd"],
)
def test_usage_errors(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    extra: tuple[str, ...],
    fragment: str,
) -> None:
  cfg = _cfg(tmp_path)
  _patch_cli(monkeypatch, cfg, _argv(*extra))
  post = mock.MagicMock()
  monkeypatch.setattr(POST_PATCH_TARGET, post)

  with pytest.raises(SystemExit) as exc_info:
    schedule_trigger.main()

  assert exc_info.value.code == 2
  assert fragment in capsys.readouterr().err
  post.assert_not_called()


# ---------------------------------------------------------------------------
# Rejection: the started process group dies, exit 2
# ---------------------------------------------------------------------------


def test_rejected_registration_kills_the_process_group(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _cfg(tmp_path)
  _patch_cli(monkeypatch, cfg, _argv("--cmd", "sleep 600"))
  reason = "session s1 has 5 pending triggers (limit 5); trigger rejected"
  pids: list[int] = []

  def rejecting_post(endpoint: str, payload: dict, *, readback: Any = None, rejection_exit_codes: Any = None) -> dict:
    pids.append(payload["watch_targets"][0]["pid"])
    print(json.dumps({"error": reason}), file=sys.stderr)
    raise SystemExit(2)

  monkeypatch.setattr(POST_PATCH_TARGET, rejecting_post)

  with pytest.raises(SystemExit) as exc_info:
    schedule_trigger.main()

  assert exc_info.value.code == 2
  assert reason in capsys.readouterr().err
  (pid,) = pids
  conftest.reap_child(pid)
  with pytest.raises(ProcessLookupError):
    os.kill(pid, 0)
  # No launch record: the registration never landed (the killed process's log stays behind).
  assert not list((cfg.sessions_dir / "s1" / "launches").glob("*/metadata.json"))


def test_failed_readback_kills_the_process_group(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _cfg(tmp_path)
  _patch_cli(monkeypatch, cfg, _argv("--cmd", "sleep 600"))
  pids: list[int] = []

  def silent_post(endpoint: str, payload: dict, *, readback: Any = None, rejection_exit_codes: Any = None) -> dict:
    pids.append(payload["watch_targets"][0]["pid"])
    return {"trigger_id": "trig-1", "fire_at": "2026-10-08T00:11:00+00:00"}  # persists nothing

  monkeypatch.setattr(POST_PATCH_TARGET, silent_post)

  with pytest.raises(SystemExit) as exc_info:
    schedule_trigger.main()

  assert exc_info.value.code == 2
  assert "readback" in capsys.readouterr().err
  (pid,) = pids
  conftest.reap_child(pid)
  with pytest.raises(ProcessLookupError):
    os.kill(pid, 0)


# ---------------------------------------------------------------------------
# ssh start failures: exit 3, no registration
# ---------------------------------------------------------------------------


def test_ssh_failure_exits_3_and_posts_nothing(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _cfg(tmp_path)
  _patch_cli(monkeypatch, cfg, _argv("--cmd", "echo hi", "--host", "nonexistent.invalid"))
  post = mock.MagicMock()
  monkeypatch.setattr(POST_PATCH_TARGET, post)

  with pytest.raises(SystemExit) as exc_info:
    schedule_trigger.main()

  assert exc_info.value.code == 3
  assert "ssh" in capsys.readouterr().err.lower()
  post.assert_not_called()


def test_unparseable_remote_pid_exits_3_and_posts_nothing(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _cfg(tmp_path)
  _patch_cli(monkeypatch, cfg, _argv("--cmd", "echo hi", "--host", "remote.example.com"))
  post = mock.MagicMock()
  monkeypatch.setattr(POST_PATCH_TARGET, post)
  fake_proc = subprocess.CompletedProcess(args=[], returncode=0, stdout="not-a-pid\n", stderr="")

  with mock.patch("subprocess.run", return_value=fake_proc), pytest.raises(SystemExit) as exc_info:
    schedule_trigger.main()

  assert exc_info.value.code == 3
  post.assert_not_called()
