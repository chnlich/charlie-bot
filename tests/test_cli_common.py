"""Tests for the shared CLI helpers (session resolution, --repo validation, API posting)."""

import argparse
import json
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
from conftest import (
    CLI_COMMON_BASE_URL_PATCH_TARGET,
    CLI_COMMON_SESSIONS_DIR_PATCH_TARGET,
    CLI_COMMON_TRANSPORT_POST_PATCH_TARGET,
    make_json_response,
    stub_credentials,
)

from src.cli import common


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
def test_resolve_session_id_sources_without_env(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    arg_session: str | None,
    cwd_session: str | None,
    expected: str,
) -> None:
  """Without the server-written variable, --session and cwd resolve as they always have."""
  sessions_dir = tmp_path / "sessions"
  sessions_dir.mkdir()
  _set_cwd(tmp_path, monkeypatch, sessions_dir, cwd_session)
  monkeypatch.delenv("CHARLIEBOT_SESSION_ID", raising=False)

  with patch(CLI_COMMON_SESSIONS_DIR_PATCH_TARGET, return_value=sessions_dir):
    assert common.resolve_session_id(arg_session) == expected


@pytest.mark.parametrize(
    ("arg_session", "cwd_session"),
    [
        ("arg-session", "cwd-session"),
    ],
)
def test_resolve_session_id_rejects_mismatches_without_env(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    arg_session: str | None,
    cwd_session: str | None,
) -> None:
  """Two disagreeing fallback sources stay a rejection when the variable is absent."""
  sessions_dir = tmp_path / "sessions"
  sessions_dir.mkdir()
  _set_cwd(tmp_path, monkeypatch, sessions_dir, cwd_session)
  monkeypatch.delenv("CHARLIEBOT_SESSION_ID", raising=False)

  with (
      patch(CLI_COMMON_SESSIONS_DIR_PATCH_TARGET, return_value=sessions_dir),
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


@pytest.mark.parametrize(
    ("arg_session", "cwd_session", "expect_note"),
    [
        (None, None, False),
        (None, "env-session", False),
        (None, "other-session", True),
        ("env-session", "other-session", True),
    ],
)
def test_resolve_session_id_env_outranks_cwd(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    arg_session: str | None,
    cwd_session: str | None,
    expect_note: bool,
) -> None:
  """The server-written variable answers from any cwd; a cwd in another session's dir only warns."""
  sessions_dir = tmp_path / "sessions"
  sessions_dir.mkdir()
  _set_cwd(tmp_path, monkeypatch, sessions_dir, cwd_session)
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "env-session")

  with patch(CLI_COMMON_SESSIONS_DIR_PATCH_TARGET, return_value=sessions_dir):
    assert common.resolve_session_id(arg_session) == "env-session"

  err = capsys.readouterr().err
  if not expect_note:
    assert err == ""
    return
  note = json.loads(err)["note"]
  assert "other-session" in note
  assert "CHARLIEBOT_SESSION_ID=env-session" in note


def test_validate_repo_path_accepts_existing_absolute_dir(tmp_path: Path) -> None:
  common.validate_repo_path(argparse.ArgumentParser(), str(tmp_path))


@pytest.mark.parametrize(
    ("access_key", "expect_header"),
    [("secret", True), ("", False)],
)
def test_post_internal_api_bearer_header(access_key: str, expect_header: bool) -> None:
  cfg = MagicMock()
  cfg.server_base_url = "https://server"
  stub_credentials({"charliebot": {"access_key": access_key}})

  with (
      patch(CLI_COMMON_BASE_URL_PATCH_TARGET, return_value=cfg.server_base_url),
      patch(CLI_COMMON_TRANSPORT_POST_PATCH_TARGET, return_value=make_json_response({"ok": True})) as mock_post,
  ):
    assert common.post_internal_api("/api/internal/x", {"a": 1}) == {"ok": True}

  headers = mock_post.call_args.kwargs["headers"]
  if expect_header:
    assert headers["Authorization"] == "Bearer secret"
  else:
    assert "Authorization" not in headers
