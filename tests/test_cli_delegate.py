"""Tests for src/cli/delegate.py."""

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
import requests

from src.cli.delegate import main


def _mock_config(tmp_path: Path):
  cfg = MagicMock()
  cfg.server_port = 9443
  cfg.sessions_dir = tmp_path / "fake_sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  return cfg


def test_main_posts_to_delegate_endpoint(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  resp_mock = MagicMock()
  resp_mock.json.return_value = {"thread_id": "t1", "description": "do work"}
  resp_mock.raise_for_status = MagicMock()

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--backend",
          "codex-o3",
          "--description",
          "do work",
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp_mock) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "s1"
  assert payload["repo_path"] == "/tmp/repo"
  assert payload["base_branch"] == "main"
  assert payload["backend"] == "codex-o3"
  assert payload["description"] == "do work"
  assert payload["task_type"] == "implement"


@pytest.mark.parametrize("task_type", ["implement", "quick-edit", "script-run"])
def test_main_task_type_lands_in_payload(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, task_type: str) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  resp_mock = MagicMock()
  resp_mock.json.return_value = {"thread_id": "t2", "description": "task"}
  resp_mock.raise_for_status = MagicMock()

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--description",
          "task",
          "--keep-worktree",
          "0",
          "--task-type",
          task_type,
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp_mock) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["task_type"] == task_type


def test_main_rejects_invalid_task_type(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--description",
          "task",
          "--keep-worktree",
          "0",
          "--task-type",
          "bogus",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  err = capsys.readouterr().err
  assert "--task-type" in err


def test_main_rejects_legacy_require_review_flag(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--description",
          "task",
          "--keep-worktree",
          "0",
          "--require-review",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  err = capsys.readouterr().err
  assert "--require-review" in err or "unrecognized" in err


def test_main_uses_error_detail_from_response(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)

  class FakeRequestException(requests.RequestException):
    def __init__(self) -> None:
      super().__init__("bad request")
      self.response = MagicMock()
      self.response.json.return_value = {"detail": "requested backend 'missing' is not in backend_options"}

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--backend",
          "missing",
          "--description",
          "do work",
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", side_effect=FakeRequestException()):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 1


def test_main_requires_keep_worktree_flag(capsys: pytest.CaptureFixture[str]) -> None:
  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--description",
          "do work",
      ]):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  err = capsys.readouterr().err
  assert "--keep-worktree" in err


def _setup_session_cwd(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, sid: str) -> MagicMock:
  """Build a session dir tree at <tmp_path>/sessions/<sid> and chdir into it."""
  cfg = MagicMock()
  cfg.server_port = 9443
  cfg.sessions_dir = tmp_path / "sessions"
  session_dir = cfg.sessions_dir / sid
  session_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.chdir(session_dir)
  return cfg


def test_session_auto_derived_from_cwd(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp_mock = MagicMock()
  resp_mock.json.return_value = {"thread_id": "t1"}
  resp_mock.raise_for_status = MagicMock()

  with patch(
      "sys.argv",
      [
          "delegate",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--description",
          "do work",
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp_mock) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "abc"


def test_session_matches_cwd(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp_mock = MagicMock()
  resp_mock.json.return_value = {"thread_id": "t1"}
  resp_mock.raise_for_status = MagicMock()

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "abc",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--description",
          "do work",
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp_mock) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "abc"


def test_session_mismatch_with_cwd_rejected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "xyz",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--description",
          "do work",
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 2
  err = capsys.readouterr().err
  assert "mismatch" in err
  assert "abc" in err
  assert "xyz" in err


def test_no_session_outside_session_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = MagicMock()
  cfg.server_port = 9443
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.chdir(tmp_path)

  with patch(
      "sys.argv",
      [
          "delegate",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--description",
          "do work",
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 2
  err = capsys.readouterr().err
  assert "session dir" in err or "--session required" in err
