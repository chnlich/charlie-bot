"""Tests for shared CLI session resolution."""

import json
from pathlib import Path
from unittest.mock import MagicMock
from unittest.mock import patch

import pytest

from src.cli import common


def _mock_config(sessions_dir: Path) -> MagicMock:
  cfg = MagicMock()
  cfg.sessions_dir = sessions_dir
  return cfg


def _set_cwd(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    sessions_dir: Path,
    session_id: str | None,
) -> None:
  if session_id is None:
    outside_dir = tmp_path / "outside"
    outside_dir.mkdir()
    monkeypatch.chdir(outside_dir)
    return
  session_dir = sessions_dir / session_id
  session_dir.mkdir(parents=True)
  monkeypatch.chdir(session_dir)


@pytest.mark.parametrize(
    ("arg_session", "cwd_session", "env_session", "expected"),
    [
        ("explicit", None, None, "explicit"),
        (None, "cwd-session", None, "cwd-session"),
        (None, None, "env-session", "env-session"),
        ("same-session", "same-session", "same-session", "same-session"),
    ],
)
def test_resolve_session_id_sources(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    arg_session: str | None,
    cwd_session: str | None,
    env_session: str | None,
    expected: str,
) -> None:
  sessions_dir = tmp_path / "sessions"
  sessions_dir.mkdir()
  _set_cwd(tmp_path, monkeypatch, sessions_dir, cwd_session)
  if env_session is None:
    monkeypatch.delenv("CHARLIEBOT_SESSION_ID", raising=False)
  else:
    monkeypatch.setenv("CHARLIEBOT_SESSION_ID", env_session)

  with patch("src.cli.common.get_config", return_value=_mock_config(sessions_dir)):
    assert common.resolve_session_id(arg_session) == expected


@pytest.mark.parametrize(
    ("arg_session", "cwd_session", "env_session"),
    [
        ("arg-session", "cwd-session", None),
        ("arg-session", None, "env-session"),
        (None, "cwd-session", "env-session"),
        ("arg-session", "arg-session", "env-session"),
    ],
)
def test_resolve_session_id_rejects_mismatches(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    arg_session: str | None,
    cwd_session: str | None,
    env_session: str | None,
) -> None:
  sessions_dir = tmp_path / "sessions"
  sessions_dir.mkdir()
  _set_cwd(tmp_path, monkeypatch, sessions_dir, cwd_session)
  if env_session is None:
    monkeypatch.delenv("CHARLIEBOT_SESSION_ID", raising=False)
  else:
    monkeypatch.setenv("CHARLIEBOT_SESSION_ID", env_session)

  with patch("src.cli.common.get_config", return_value=_mock_config(sessions_dir)):
    with pytest.raises(SystemExit) as exc_info:
      common.resolve_session_id(arg_session)

  assert exc_info.value.code == 2
  error = json.loads(capsys.readouterr().err)["error"]
  assert "mismatch" in error
  if arg_session is not None:
    assert arg_session in error
  if cwd_session is not None:
    assert cwd_session in error
  if env_session is not None:
    assert env_session in error


def test_resolve_session_id_requires_source_outside_session_dir(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
  sessions_dir = tmp_path / "sessions"
  sessions_dir.mkdir()
  _set_cwd(tmp_path, monkeypatch, sessions_dir, None)
  monkeypatch.delenv("CHARLIEBOT_SESSION_ID", raising=False)

  with patch("src.cli.common.get_config", return_value=_mock_config(sessions_dir)):
    with pytest.raises(SystemExit) as exc_info:
      common.resolve_session_id(None)

  assert exc_info.value.code == 2
  error = json.loads(capsys.readouterr().err)["error"]
  assert "--session required" in error


def test_resolve_session_id_only_derives_direct_session_child(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
  sessions_dir = tmp_path / "sessions"
  nested_dir = sessions_dir / "session-id" / "nested"
  nested_dir.mkdir(parents=True)
  monkeypatch.chdir(nested_dir)
  monkeypatch.delenv("CHARLIEBOT_SESSION_ID", raising=False)

  with patch("src.cli.common.get_config", return_value=_mock_config(sessions_dir)):
    with pytest.raises(SystemExit) as exc_info:
      common.resolve_session_id(None)

  assert exc_info.value.code == 2
  error = json.loads(capsys.readouterr().err)["error"]
  assert "--session required" in error
