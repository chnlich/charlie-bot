"""``charliebot config get <key>``: one reader for a top-level config key.

The key names a CharlieBotConfig top-level field — a section (``server``,
``paths``, ...) or the scalar ``headless_chrome_bin`` — and the value is read
from config.yaml alone. Stdout is the value and nothing else (callers
substitute it into commands); diagnostics go to stderr. Unknown keys exit 2.
"""

import json
from pathlib import Path

import pytest

import src.cli.config as cli
from src.core.yaml_utils import save_yaml


def _run_get(monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], key: str) -> tuple[int, str, str]:
  monkeypatch.setattr("sys.argv", ["charliebot config", "get", key])
  exit_code = 0
  try:
    cli.main()
  except SystemExit as e:
    exit_code = e.code if isinstance(e.code, int) else 1
  captured = capsys.readouterr()
  return exit_code, captured.out, captured.err


def test_server_section_prints_exact_json(
    profile_home: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """A section key prints the section as JSON and nothing else."""
  save_yaml(profile_home / "config.yaml", {"server": {"port": 18498}})
  code, out, err = _run_get(monkeypatch, capsys, "server")
  assert code == 0
  assert json.loads(out) == {"host": "127.0.0.1", "port": 18498, "subprocess_buffer_limit_mb": 1024}
  assert err == ""


def test_paths_section_prints_configured_values(
    profile_home: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """The paths section round-trips: the JSON carries exactly the configured values."""
  paths = {
      "workspace_dirs": [str(profile_home / "workspace")],
      "worktree_dir": str(profile_home / "worktrees"),
  }
  save_yaml(profile_home / "config.yaml", {"paths": paths})
  code, out, err = _run_get(monkeypatch, capsys, "paths")
  assert code == 0
  assert json.loads(out) == paths
  assert err == ""


def test_scalar_prints_bare(
    profile_home: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """No quotes, no Python repr: $(charliebot config get key) yields exactly the value."""
  save_yaml(profile_home / "config.yaml", {"headless_chrome_bin": "/usr/bin/chromium"})
  code, out, err = _run_get(monkeypatch, capsys, "headless_chrome_bin")
  assert code == 0
  assert out == "/usr/bin/chromium\n"
  assert err == ""


def test_unknown_key_exits_2(
    profile_home: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  save_yaml(profile_home / "config.yaml", {"server": {"port": 18498}})
  code, out, err = _run_get(monkeypatch, capsys, "no_such_key")
  assert code == 2
  assert "no_such_key" in err
  assert out == ""
