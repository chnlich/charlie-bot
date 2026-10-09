"""Tests for src/features/slack/cli.py — argv to request body, readback printing, refusal exit codes."""

import json
import pathlib
from unittest import mock

import conftest
import pytest

from src.features.slack import cli

_READBACK = {"posted": True, "chars": 10, "chunks": 1, "over_budget": False, "answers": "summon-1"}


def test_reply_posts_the_file_text_for_the_cwd_session_and_prints_the_readback(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  conftest.run_reply_file_case(monkeypatch, tmp_path, capsys, "slack", _READBACK)


def test_reply_reads_stdin_when_the_file_is_a_dash(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  conftest.run_reply_stdin_case(monkeypatch, tmp_path, capsys, "slack", _READBACK)


def test_server_refusal_exits_non_zero_with_the_detail_on_stderr(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """A 409 (no Slack thread) surfaces as one JSON error line and a non-zero exit."""
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  reply_file = tmp_path / "reply.md"
  reply_file.write_text("the answer", encoding="utf-8")
  refusal = mock.MagicMock()
  refusal.status_code = 409
  refusal.json.return_value = {"detail": "Session has no Slack thread"}
  with conftest.patched_cli_post(cfg, ["slack", "reply", "--file", str(reply_file)], return_value=refusal), \
       mock.patch(conftest.CLI_COMMON_MAYBE_VERSION_SKEW_HINT_PATCH_TARGET, return_value=None), \
       pytest.raises(SystemExit) as exc_info:
    cli.main()

  assert exc_info.value.code == 1
  captured = capsys.readouterr()
  assert captured.out == ""
  assert json.loads(captured.err)["error"] == "Session has no Slack thread"
