"""Tests for the shared CLI helpers (session resolution, --repo validation, API posting)."""

import argparse
import json
import subprocess
import sys
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
from src.core.constants import SESSION_ID_ENV_VAR


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
  monkeypatch.delenv(SESSION_ID_ENV_VAR, raising=False)

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
  monkeypatch.delenv(SESSION_ID_ENV_VAR, raising=False)

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
  monkeypatch.setenv(SESSION_ID_ENV_VAR, "env-session")

  with patch(CLI_COMMON_SESSIONS_DIR_PATCH_TARGET, return_value=sessions_dir):
    assert common.resolve_session_id(arg_session) == "env-session"

  err = capsys.readouterr().err
  if not expect_note:
    assert err == ""
    return
  note = json.loads(err)["note"]
  assert "other-session" in note
  assert f"{SESSION_ID_ENV_VAR}=env-session" in note


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


def test_port_cache_hit_keeps_the_yaml_stack_out_of_the_verb_process(tmp_path: Path) -> None:
  """A fresh process serving the base-url port cache reads its fingerprints and the cached
  port without importing the credentials module: the cache hit path is every internal-API
  verb's wall, and the credentials import drags PyYAML in for a stat-only read."""
  repo_root = str(Path(__file__).resolve().parents[1])
  home = tmp_path / "home"
  (home / "cache").mkdir(parents=True)
  code = "\n".join(
      [
          "import json, sys",
          f"sys.path.insert(0, {repo_root!r})",
          f"import os; os.environ['CHARLIEBOT_HOME'] = {str(home)!r}",
          "from src.core import home",
          "from src.cli import common",
          "doc = {'fingerprint': [list(home.file_fingerprint('config.yaml')), list(common._config_module_fingerprint())], 'port': 49999}",
          f"(home_doc := {str(home / 'cache' / 'cli_base_url.json')!r}) and open(home_doc, 'w').write(json.dumps(doc))",
          "assert common._cached_server_port() == 49999",
          "assert 'src.core.credentials' not in sys.modules, 'port cache hit pulled the credentials module'",
          "assert 'yaml' not in sys.modules, 'port cache hit pulled PyYAML'",
      ])
  proc = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, check=False)
  assert proc.returncode == 0, proc.stderr
