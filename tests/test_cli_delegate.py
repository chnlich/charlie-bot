"""Tests for src/cli/delegate.py."""

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
import requests

from src.cli.delegate import main


def _mock_config(tmp_path: Path):
  cfg = MagicMock()
  cfg.server_port = 9443
  return cfg


def test_main_posts_to_delegate_endpoint(tmp_path: Path) -> None:
  cfg = _mock_config(tmp_path)
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
       patch("src.cli.delegate.get_config", return_value=cfg), \
       patch("src.cli.delegate.requests.post", return_value=resp_mock) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "s1"
  assert payload["repo_path"] == "/tmp/repo"
  assert payload["base_branch"] == "main"
  assert payload["backend"] == "codex-o3"
  assert payload["description"] == "do work"


def test_main_uses_error_detail_from_response(tmp_path: Path) -> None:
  cfg = _mock_config(tmp_path)

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
       patch("src.cli.delegate.get_config", return_value=cfg), \
       patch("src.cli.delegate.requests.post", side_effect=FakeRequestException()):
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
