"""Tests for src/cli/slack.py — argv to request body, readback printing, refusal exit codes."""

import io
import json
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
import requests
from conftest import make_json_response
from conftest import setup_session_cwd as _setup_session_cwd

from src.cli.slack import main

_READBACK = {"posted": True, "chars": 10, "chunks": 1, "over_budget": False, "answers": "summon-1"}


def test_reply_posts_the_file_text_for_the_cwd_session_and_prints_the_readback(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  reply_file = tmp_path / "reply.md"
  reply_file.write_text("the answer", encoding="utf-8")
  resp = make_json_response(_READBACK)
  with patch("sys.argv", ["slack", "reply", "--file", str(reply_file)]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp) as post_mock:
    main()

  assert post_mock.call_args.args[0].endswith("/api/internal/slack/reply")
  assert post_mock.call_args.kwargs["json"] == {"session_id": "abc", "text": "the answer"}
  out = capsys.readouterr().out
  assert out.count("\n") == 1
  assert json.loads(out) == _READBACK


def test_reply_reads_stdin_when_the_file_is_a_dash(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  monkeypatch.setattr("sys.stdin", io.StringIO("piped reply\n"))
  resp = make_json_response(_READBACK)
  with patch("sys.argv", ["slack", "reply", "--file", "-"]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp) as post_mock:
    main()

  assert post_mock.call_args.kwargs["json"]["text"] == "piped reply\n"
  assert json.loads(capsys.readouterr().out) == _READBACK


@pytest.mark.parametrize("source", ["missing", "empty", "blank-stdin"])
def test_reply_without_text_is_a_usage_error_before_any_request(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], source: str) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  if source == "missing":
    file_arg = str(tmp_path / "absent.md")
  elif source == "empty":
    empty = tmp_path / "empty.md"
    empty.write_text("  \n", encoding="utf-8")
    file_arg = str(empty)
  else:
    monkeypatch.setattr("sys.stdin", io.StringIO("  \n"))
    file_arg = "-"
  with patch("sys.argv", ["slack", "reply", "--file", file_arg]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock, \
       pytest.raises(SystemExit) as exc_info:
    main()

  assert exc_info.value.code == 2
  assert post_mock.call_count == 0
  assert "error" in json.loads(capsys.readouterr().err)


def test_server_refusal_exits_non_zero_with_the_detail_on_stderr(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """A 409 (no Slack thread) surfaces as one JSON error line and a non-zero exit."""
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  reply_file = tmp_path / "reply.md"
  reply_file.write_text("the answer", encoding="utf-8")
  refusal = MagicMock()
  refusal.status_code = 409
  refusal.json.return_value = {"detail": "Session has no Slack thread"}
  refusal.raise_for_status.side_effect = requests.HTTPError(response=refusal)
  with patch("sys.argv", ["slack", "reply", "--file", str(reply_file)]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common._maybe_version_skew_hint", return_value=None), \
       patch("src.cli.common.requests.post", return_value=refusal), \
       pytest.raises(SystemExit) as exc_info:
    main()

  assert exc_info.value.code == 1
  captured = capsys.readouterr()
  assert captured.out == ""
  assert json.loads(captured.err)["error"] == "Session has no Slack thread"
