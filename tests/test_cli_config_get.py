"""``charliebot config get <key>``: one reader that works wherever the key lives.

Stdout is the value and nothing else (callers substitute it into commands);
diagnostics go to stderr. Unknown keys exit 2, known-but-unset keys exit 1.
"""

import json
from pathlib import Path

import pytest
from conftest import reset_config_caches

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


def test_same_stdout_from_config_yaml_and_fragment(
    profile_home: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """The output does not depend on which file holds the key."""
  save_yaml(profile_home / "config.yaml", {"charliebot_access_key": "top-secret-token"})
  code, out_base, err_base = _run_get(monkeypatch, capsys, "charliebot_access_key")
  assert code == 0
  assert err_base == ""

  save_yaml(profile_home / "config.yaml", {"server_port": 18498})
  fragment = profile_home / "config.d" / "auth.yaml"
  fragment.parent.mkdir()
  save_yaml(fragment, {"charliebot_access_key": "top-secret-token"})
  reset_config_caches()
  code, out_frag, err_frag = _run_get(monkeypatch, capsys, "charliebot_access_key")
  assert code == 0
  assert err_frag == ""

  assert out_base == out_frag


def test_scalar_prints_bare(
    profile_home: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """No quotes, no Python repr: $(charliebot config get key) yields exactly the value."""
  save_yaml(profile_home / "config.yaml", {"charliebot_access_key": "tok abc"})
  code, out, err = _run_get(monkeypatch, capsys, "charliebot_access_key")
  assert code == 0
  assert out == "tok abc\n"
  assert err == ""


def test_list_prints_as_round_trippable_json(
    profile_home: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  save_yaml(profile_home / "config.yaml", {"slack_allowed_user_ids": ["U01", "U02"]})
  code, out, err = _run_get(monkeypatch, capsys, "slack_allowed_user_ids")
  assert code == 0
  assert json.loads(out) == ["U01", "U02"]
  assert err == ""


def test_unknown_key_exits_2(
    profile_home: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  save_yaml(profile_home / "config.yaml", {"server_port": 18498})
  code, out, err = _run_get(monkeypatch, capsys, "no_such_key")
  assert code == 2
  assert "no_such_key" in err
  assert out == ""


def test_unset_key_exits_1(
    profile_home: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """A field whose value is None fails instead of printing 'None' into a caller's variable."""
  save_yaml(profile_home / "config.yaml", {"server_port": 18498})
  code, out, err = _run_get(monkeypatch, capsys, "telegram_bot_token")
  assert code == 1
  assert "telegram_bot_token" in err
  assert "unset" in err
  assert out == ""
