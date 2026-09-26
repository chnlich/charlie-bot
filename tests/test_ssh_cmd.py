"""Tests for src/core/ssh.py: the one argv builder every remote ssh subprocess takes."""

import os
import stat
from pathlib import Path

import pytest
from conftest import SSH_CONTROL_DIR_PATCH_TARGET

from src.core.ssh import ssh_cmd


def test_argv_carries_the_policy_options(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  control_dir = tmp_path / "controlmasters"
  monkeypatch.setattr(SSH_CONTROL_DIR_PATCH_TARGET, str(control_dir))

  argv = ssh_cmd("host2", "sacct -j 1 -X -n -P")

  assert argv[0] == "ssh"
  assert "BatchMode=yes" in argv
  assert "ConnectTimeout=10" in argv
  assert "ControlMaster=auto" in argv
  assert f"ControlPath={control_dir}/%C" in argv
  assert "ControlPersist=1200" in argv
  # The host is the first bare word after the option pairs; the remote command follows.
  assert argv[-2] == "host2"
  assert argv[-1] == "sacct -j 1 -X -n -P"


def test_control_dir_created_owner_only(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  control_dir = tmp_path / "nested" / "controlmasters"
  monkeypatch.setattr(SSH_CONTROL_DIR_PATCH_TARGET, str(control_dir))

  ssh_cmd("host2", "true")

  assert control_dir.is_dir()
  assert stat.S_IMODE(control_dir.stat().st_mode) & 0o077 == 0
  # An existing directory is reused, not replaced.
  os.makedirs(control_dir, exist_ok=True)
  assert ssh_cmd("host2", "true")[0] == "ssh"
