"""Tests for src/features/discord/cli.py — argv to request body, readback printing, refusal exit codes."""

import json
import pathlib
from unittest import mock

import conftest
import pytest

from src.features.discord import cli

_REPLY_READBACK = {
    "posted": True,
    "text": "the answer",
    "operator_only_note": None,
    "chars": 10,
    "chunks": 1,
    "over_budget": False,
    "answers": "summon-1",
    "attachments": ["answer.html"],
}
_READ_READBACK = {
    "messages":
        [
            {
                "id": "m1",
                "author_id": "u1",
                "author": "someone",
                "timestamp": "2026-01-01T00:00:00+00:00",
                "content": "hello",
                "attachments": [],
                "unread": False,
            }
        ],
    "watermark_id": "m1",
    "more_unread": False,
}


def _write_reply_file(tmp_path: pathlib.Path) -> pathlib.Path:
  reply_file = tmp_path / "reply.md"
  reply_file.write_text("the answer", encoding="utf-8")
  return reply_file


def test_reply_posts_the_file_text_for_the_cwd_session_and_prints_the_readback(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  conftest.run_reply_file_case(monkeypatch, tmp_path, capsys, "discord", _REPLY_READBACK)


def test_reply_reads_stdin_when_the_file_is_a_dash(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  conftest.run_reply_stdin_case(monkeypatch, tmp_path, capsys, "discord", _REPLY_READBACK)


def test_stale_thread_refusal_exits_non_zero_with_the_detail_on_stderr(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """A 412 (unread thread messages) surfaces as one JSON error line and a non-zero exit."""
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  reply_file = _write_reply_file(tmp_path)
  refusal = mock.MagicMock()
  refusal.status_code = 412
  refusal.json.return_value = {
      "detail": "Session has unread Discord thread messages; run charliebot discord read first"
  }
  with conftest.patched_cli_post(cfg, ["discord", "reply", "--file", str(reply_file)], return_value=refusal), \
       mock.patch(conftest.CLI_COMMON_MAYBE_VERSION_SKEW_HINT_PATCH_TARGET, return_value=None), \
       pytest.raises(SystemExit) as exc_info:
    cli.main()

  assert exc_info.value.code == 1
  captured = capsys.readouterr()
  assert captured.out == ""
  assert json.loads(captured.err)["error"] == \
      "Session has unread Discord thread messages; run charliebot discord read first"


def test_read_defaults_to_the_session_thread_with_url_null_and_limit_50(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = conftest.make_json_response(_READ_READBACK)
  with conftest.patched_cli_post(cfg, ["discord", "read"], return_value=resp) as post_mock:
    cli.main()

  assert post_mock.call_args.args[0].endswith("/api/internal/discord/read")
  assert post_mock.call_args.kwargs["json"] == {"session_id": "abc", "url": None, "limit": 50}
  out = capsys.readouterr().out
  assert out.count("\n") == 1
  assert json.loads(out) == _READ_READBACK


def test_read_passes_the_url_and_limit_through(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = conftest.make_json_response(_READ_READBACK)
  with conftest.patched_cli_post(cfg,
                                 ["discord", "read", "--url", "https://discord.com/channels/1/2/3", "--limit", "5"],
                                 return_value=resp) as post_mock:
    cli.main()

  assert post_mock.call_args.kwargs["json"] == {
      "session_id": "abc",
      "url": "https://discord.com/channels/1/2/3",
      "limit": 5
  }
  assert json.loads(capsys.readouterr().out) == _READ_READBACK


@pytest.mark.parametrize("limit", [0, 101])
def test_limit_out_of_range_is_a_usage_error(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], limit: int) -> None:
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  with conftest.patched_cli_post(cfg, ["discord", "read", "--limit", str(limit)]) as post_mock, \
       pytest.raises(SystemExit) as exc_info:
    cli.main()

  assert exc_info.value.code == 2
  assert "--limit" in json.loads(capsys.readouterr().err)["error"]
  assert post_mock.call_count == 0


def test_check_posts_an_empty_body_prints_the_readback_and_exits_zero_when_ok(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  readback = {
      "ok": True,
      "bot_user": "charliebot",
      "application_id": "app-1",
      "message_content_intent": True,
      "guilds": [{
          "id": "g1",
          "name": "Guild",
          "missing_permissions": []
      }],
  }
  resp = conftest.make_json_response(readback)
  with conftest.patched_cli_post(cfg, ["discord", "check"], return_value=resp) as post_mock:
    cli.main()

  assert post_mock.call_args.args[0].endswith("/api/internal/discord/check")
  assert post_mock.call_args.kwargs["json"] == {}
  out = capsys.readouterr().out
  assert out.count("\n") == 1
  assert json.loads(out) == readback


def test_check_exits_one_after_printing_when_ok_is_false(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  readback = {
      "ok": False,
      "bot_user": "charliebot",
      "application_id": "app-1",
      "message_content_intent": False,
      "guilds": [{
          "id": "g1",
          "name": "Guild",
          "missing_permissions": ["VIEW_CHANNEL"]
      }],
  }
  resp = conftest.make_json_response(readback)
  with conftest.patched_cli_post(cfg, ["discord", "check"], return_value=resp), \
       pytest.raises(SystemExit) as exc_info:
    cli.main()

  assert exc_info.value.code == 1
  assert json.loads(capsys.readouterr().out) == readback


def test_help_lists_discord(capsys: pytest.CaptureFixture[str]) -> None:
  from src.app import main

  main.main(["--help"])

  assert "discord" in capsys.readouterr().out
