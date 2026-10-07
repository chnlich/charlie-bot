"""Tests for src/runtime/cli/session.py's dialog verb — block format, backward paging, bootstrap cursor."""

import contextlib
from collections.abc import Iterator
from unittest import mock

import conftest
import pytest

from src.runtime.cli import session


@contextlib.contextmanager
def patched_cli_gets(cfg: object, argv: list[str], payloads: list[dict]) -> Iterator[mock.MagicMock]:
  """_patched_cli_transport with _request_get answering one payload per call, in call order."""
  yield from conftest._patched_cli_transport(
      conftest.CLI_COMMON_TRANSPORT_GET_PATCH_TARGET,
      cfg,
      argv,
      side_effect=[conftest.make_json_response(payload) for payload in payloads])


def _msg(role: str, content: str, ts: str, idx: int, **extra: object) -> dict:
  """One API message row: the fields the dialog contract reads plus its id."""
  return {"role": role, "content": content, "timestamp": ts, "event_index": idx, "id": f"legacy:{idx}", **extra}


def _bootstrap(messages: list[dict], oldest: int, has_more: bool, event_count: int) -> dict:
  """The bootstrap payload shape the chat UI opens a session with."""
  return {
      "session": {
          "id": "abc",
          "name": "abc"
      },
      "messages": messages,
      "pending_draft": None,
      "event_count": event_count,
      "oldest_message_ordinal": oldest,
      "has_more": has_more,
  }


def test_dialog_formats_blocks_and_skips_empty_messages(
    tmp_path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """One block per message with a body or a tool call; tool output and thinking never print."""
  assistant = {
      **_msg("assistant", "Running the check now.", "2026-07-10T08:00:02Z", 3),
      "tools":
          [
              {
                  "name": "Bash",
                  "input": {
                      "command": "pytest -q\necho done"
                  },
                  "output": "SECRET TOOL OUTPUT",
                  "is_error": False,
              },
              {
                  "name": "Read",
                  "input": {
                      "file_path": "/tmp/a.py",
                      "limit": 5
                  },
                  "output": "",
                  "is_error": False,
              },
          ],
      "thinking": "SECRET THINKING",
  }
  messages = [
      _msg("user", "please check", "2026-07-10T08:00:00Z", 0),
      assistant,
      _msg(
          "child_report",
          "Worker finished the sweep.",
          "2026-07-10T08:00:05Z",
          7,
          child_session_id="child-1",
          outcome="completed"),
      {
          "role": "separator",
          "event_index": 8
      },
  ]
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  with patched_cli_gets(cfg, ["session", "dialog"],
                        [_bootstrap(messages, oldest=0, has_more=False, event_count=9)]) as get_mock:
    session.main()

  assert get_mock.call_count == 1
  assert get_mock.call_args.args[0].endswith("/api/sessions/abc/bootstrap")
  out = capsys.readouterr().out
  assert out == (
      "## user \u00b7 2026-07-10T08:00:00Z \u00b7 event 0\n"
      "please check\n"
      "\n"
      "## assistant \u00b7 2026-07-10T08:00:02Z \u00b7 event 3\n"
      "Running the check now.\n"
      "$ Bash pytest -q\n"
      "$ Read {\"file_path\": \"/tmp/a.py\", \"limit\": 5}\n"
      "\n"
      "## child_report \u00b7 2026-07-10T08:00:05Z \u00b7 event 7\n"
      "Worker finished the sweep.\n")
  assert "SECRET" not in out


def test_dialog_joins_pages_ascending(
    tmp_path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """Two pages arrive newest-first and print oldest-first; the events call follows the cursor."""
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  older = [
      _msg("user", "first ask", "2026-07-10T08:00:00Z", 0),
      _msg("assistant", "first answer", "2026-07-10T08:00:01Z", 1),
  ]
  newer = [
      _msg("user", "second ask", "2026-07-10T08:00:02Z", 2),
      _msg("assistant", "second answer", "2026-07-10T08:00:03Z", 3),
  ]
  payloads = [
      _bootstrap(newer, oldest=2, has_more=True, event_count=9),
      {
          "messages": older,
          "has_more": False,
          "next_before": 0
      },
  ]
  with patched_cli_gets(cfg, ["session", "dialog"], payloads) as get_mock:
    session.main()

  assert get_mock.call_count == 2
  events_call = get_mock.call_args_list[1]
  assert events_call.args[0].endswith("/api/sessions/abc/events")
  assert events_call.kwargs["params"] == {"before": 2, "limit": 200}
  out = capsys.readouterr().out
  assert [line for line in out.splitlines() if line.startswith("## ")] == [
      "## user \u00b7 2026-07-10T08:00:00Z \u00b7 event 0",
      "## assistant \u00b7 2026-07-10T08:00:01Z \u00b7 event 1",
      "## user \u00b7 2026-07-10T08:00:02Z \u00b7 event 2",
      "## assistant \u00b7 2026-07-10T08:00:03Z \u00b7 event 3",
  ]
  assert out.index("first ask") < out.index("second ask")


def test_dialog_ordinary_session_prints_history_without_events_call(
    tmp_path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """An ordinary session whose tail page holds everything prints it and never touches /events."""
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  messages = [
      _msg("user", "the whole ask", "2026-07-10T08:00:00Z", 0),
      _msg("assistant", "the whole answer", "2026-07-10T08:00:01Z", 1),
  ]
  with patched_cli_gets(cfg, ["session", "dialog"],
                        [_bootstrap(messages, oldest=0, has_more=False, event_count=2)]) as get_mock:
    session.main()

  assert get_mock.call_count == 1
  out = capsys.readouterr().out
  assert "the whole ask" in out
  assert "the whole answer" in out


def test_dialog_archived_session_starts_from_bootstrap_cursor(
    tmp_path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """An archived session's first /events call uses the bootstrap cursor (legacy event-index space),
  then follows next_before to the bottom; the complete history prints ascending."""
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  tail = [_msg("user", "newest ask", "2026-07-10T08:00:09Z", 1009)]
  mid = [_msg("assistant", "middle answer", "2026-07-10T08:00:06Z", 1006)]
  old = [
      _msg("user", "oldest ask", "2026-07-10T08:00:03Z", 1003),
      _msg("assistant", "oldest answer", "2026-07-10T08:00:04Z", 1004),
  ]
  payloads = [
      _bootstrap(tail, oldest=1005, has_more=True, event_count=1010),
      {
          "messages": mid,
          "has_more": True,
          "next_before": 805
      },
      {
          "messages": old,
          "has_more": False,
          "next_before": 0
      },
  ]
  argv = ["session", "dialog", "--session", "abc"]
  with patched_cli_gets(cfg, argv, payloads) as get_mock:
    session.main()

  assert get_mock.call_count == 3
  first_events = get_mock.call_args_list[1]
  assert first_events.args[0].endswith("/api/sessions/abc/events")
  assert first_events.kwargs["params"] == {"before": 1005, "limit": 200}
  assert get_mock.call_args_list[2].kwargs["params"] == {"before": 805, "limit": 200}
  out = capsys.readouterr().out
  assert [line for line in out.splitlines() if line.startswith("## ")] == [
      "## user \u00b7 2026-07-10T08:00:03Z \u00b7 event 1003",
      "## assistant \u00b7 2026-07-10T08:00:04Z \u00b7 event 1004",
      "## assistant \u00b7 2026-07-10T08:00:06Z \u00b7 event 1006",
      "## user \u00b7 2026-07-10T08:00:09Z \u00b7 event 1009",
  ]
