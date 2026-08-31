"""Tests for shared CLI session resolution."""

import json
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
from conftest import (
  CLI_COMMON_GET_CONFIG_PATCH_TARGET,
  CLI_COMMON_REQUESTS_POST_PATCH_TARGET,
  make_json_response,
)

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
    ("arg_session", "cwd_session", "expected"),
    [
        ("explicit", None, "explicit"),
        (None, "cwd-session", "cwd-session"),
        ("same-session", "same-session", "same-session"),
    ],
)
def test_resolve_session_id_sources(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    arg_session: str | None,
    cwd_session: str | None,
    expected: str,
) -> None:
  sessions_dir = tmp_path / "sessions"
  sessions_dir.mkdir()
  _set_cwd(tmp_path, monkeypatch, sessions_dir, cwd_session)
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "ignored-env-session")

  with patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=_mock_config(sessions_dir)):
    assert common.resolve_session_id(arg_session) == expected


@pytest.mark.parametrize(
    ("arg_session", "cwd_session"),
    [
        ("arg-session", "cwd-session"),
    ],
)
def test_resolve_session_id_rejects_mismatches(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    arg_session: str | None,
    cwd_session: str | None,
) -> None:
  sessions_dir = tmp_path / "sessions"
  sessions_dir.mkdir()
  _set_cwd(tmp_path, monkeypatch, sessions_dir, cwd_session)
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "ignored-env-session")

  with (
      patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=_mock_config(sessions_dir)),
      pytest.raises(SystemExit) as exc_info,
  ):
    common.resolve_session_id(arg_session)

  assert exc_info.value.code == 2
  error = json.loads(capsys.readouterr().err)["error"]
  assert "mismatch" in error
  if arg_session is not None:
    assert arg_session in error
  if cwd_session is not None:
    assert cwd_session in error
  assert "ignored-env-session" not in error


def test_resolve_session_id_requires_source_outside_session_dir(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
  sessions_dir = tmp_path / "sessions"
  sessions_dir.mkdir()
  _set_cwd(tmp_path, monkeypatch, sessions_dir, None)
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "ignored-env-session")

  with (
      patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=_mock_config(sessions_dir)),
      pytest.raises(SystemExit) as exc_info,
  ):
    common.resolve_session_id(None)

  assert exc_info.value.code == 2
  error = json.loads(capsys.readouterr().err)["error"]
  assert "--session required" in error


@pytest.mark.parametrize(
    ("access_key", "expect_header"),
    [("secret", True), ("", False)],
)
def test_post_internal_api_bearer_header(access_key: str, expect_header: bool) -> None:
  cfg = MagicMock()
  cfg.server_base_url = "https://server"
  cfg.charliebot_access_key = access_key

  with (
      patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg),
      patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET,
            return_value=make_json_response({"ok": True})) as mock_post,
  ):
    assert common.post_internal_api("/api/internal/x", {"a": 1}) == {"ok": True}

  headers = mock_post.call_args.kwargs["headers"]
  if expect_header:
    assert headers["Authorization"] == "Bearer secret"
  else:
    assert "Authorization" not in headers


def test_resolve_session_id_only_derives_direct_session_child(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
  sessions_dir = tmp_path / "sessions"
  nested_dir = sessions_dir / "session-id" / "nested"
  nested_dir.mkdir(parents=True)
  monkeypatch.chdir(nested_dir)
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "ignored-env-session")

  with (
      patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=_mock_config(sessions_dir)),
      pytest.raises(SystemExit) as exc_info,
  ):
    common.resolve_session_id(None)

  assert exc_info.value.code == 2
  error = json.loads(capsys.readouterr().err)["error"]
  assert "--session required" in error
