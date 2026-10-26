"""Tests for src/runtime/cli/session.py's dialog verb — block format, backward paging, bootstrap cursor."""

import contextlib
import json
import sys
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


def _bootstrap(messages: list[dict], oldest: int, has_more: bool, event_count: int, archive_offset: int = 0) -> dict:
  """The bootstrap payload shape the chat UI opens a session with; archive_offset rides the
  session object the payload carries, so the dialog can tell messages from events."""
  return {
      "session": {
          "id": "abc",
          "name": "abc",
          "archive_offset": archive_offset
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


def test_cancel_with_unobserved_run_prints_pid_and_exits_nonzero(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  response = {"id": "worker-1", "stopped_runs": [{"run_id": "run-1", "outcome": None}]}
  monkeypatch.setattr(sys, "argv", ["charliebot", "cancel", "worker-1", "--reason", "stop", "--request-id", "cancel-1"])
  monkeypatch.setattr(session.common, "post_internal_api", lambda *args, **kwargs: response)
  monkeypatch.setattr(
      session.common, "get_api", lambda endpoint, params: {
          "items": [{
              "id": "run-1",
              "pid": 12345
          }],
          "next_cursor": None
      })

  with pytest.raises(SystemExit) as exit_info:
    session.main()

  captured = capsys.readouterr()
  assert exit_info.value.code == 1
  assert json.loads(captured.out) == response
  assert "run run-1 pid 12345" in captured.err


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


def test_dialog_turns_prints_last_complete_turns_and_the_turn_in_progress(
    tmp_path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """--turns 2 stops once the fetched pages hold three separators and prints everything after the
  third-last one: the two complete turns plus the turn in progress, under one header line."""
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  older = [
      _msg("user", "first ask", "2026-07-10T08:00:00Z", 1000),
      _msg("assistant", "first answer", "2026-07-10T08:00:01Z", 1001),
      {
          "role": "separator",
          "event_index": 1002
      },
      _msg("user", "second ask", "2026-07-10T08:00:02Z", 1003),
      _msg("assistant", "second answer", "2026-07-10T08:00:03Z", 1004),
      {
          "role": "separator",
          "event_index": 1005
      },
  ]
  newer = [
      _msg("user", "third ask", "2026-07-10T08:00:04Z", 1006),
      _msg("assistant", "third answer", "2026-07-10T08:00:05Z", 1007),
      {
          "role": "separator",
          "event_index": 1008
      },
      _msg("user", "fourth ask", "2026-07-10T08:00:06Z", 1009),
      _msg("assistant", "fourth answer", "2026-07-10T08:00:07Z", 1010),
  ]
  payloads = [
      _bootstrap(newer, oldest=6, has_more=True, event_count=11),
      {
          "messages": older,
          "has_more": False,
          "next_before": 0
      },
  ]
  with patched_cli_gets(cfg, ["session", "dialog", "--turns", "2"], payloads) as get_mock:
    session.main()

  assert get_mock.call_count == 2
  events_call = get_mock.call_args_list[1]
  assert events_call.kwargs["params"] == {"before": 6, "limit": 200}
  out = capsys.readouterr().out
  assert out == (
      "last 2 turns \u00b7 from event 1003 \u00b7 3 earlier messages\n"
      "\n"
      "## user \u00b7 2026-07-10T08:00:02Z \u00b7 event 1003\n"
      "second ask\n"
      "\n"
      "## assistant \u00b7 2026-07-10T08:00:03Z \u00b7 event 1004\n"
      "second answer\n"
      "\n"
      "## user \u00b7 2026-07-10T08:00:04Z \u00b7 event 1006\n"
      "third ask\n"
      "\n"
      "## assistant \u00b7 2026-07-10T08:00:05Z \u00b7 event 1007\n"
      "third answer\n"
      "\n"
      "## user \u00b7 2026-07-10T08:00:06Z \u00b7 event 1009\n"
      "fourth ask\n"
      "\n"
      "## assistant \u00b7 2026-07-10T08:00:07Z \u00b7 event 1010\n"
      "fourth answer\n")


def test_dialog_turns_prints_whole_history_when_it_holds_fewer_turns(
    tmp_path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """A history with fewer separators than --turns needs prints whole \u2014 here with no separator at
  all \u2014 and the header counts 0 earlier messages."""
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  messages = [
      _msg("user", "the only ask", "2026-07-10T08:00:00Z", 1000),
      _msg("assistant", "the only answer", "2026-07-10T08:00:01Z", 1001),
  ]
  with patched_cli_gets(cfg, ["session", "dialog", "--turns", "3"],
                        [_bootstrap(messages, oldest=0, has_more=False, event_count=2)]) as get_mock:
    session.main()

  assert get_mock.call_count == 1  # the bootstrap page alone already holds the whole history
  out = capsys.readouterr().out
  assert out == (
      "last 3 turns \u00b7 from event 1000 \u00b7 0 earlier messages\n"
      "\n"
      "## user \u00b7 2026-07-10T08:00:00Z \u00b7 event 1000\n"
      "the only ask\n"
      "\n"
      "## assistant \u00b7 2026-07-10T08:00:01Z \u00b7 event 1001\n"
      "the only answer\n")


def test_dialog_turns_rejects_values_below_one(capsys: pytest.CaptureFixture[str]) -> None:
  """--turns 0 is a usage error before any request; the whole-log default stays the only escape."""
  with mock.patch("sys.argv", ["session", "dialog", "--turns", "0"]), pytest.raises(SystemExit) as exc_info:
    session.main()

  conftest.assert_cli_reject_exit2(exc_info, capsys, "--turns must be an integer of at least 1")


def test_dialog_turns_archived_session_header_counts_events(
    tmp_path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """An archived session (archive_offset > 0) pages in the legacy event-index space, so its header
  says `earlier events` and M comes from the event-index cursor plus the position in its page."""
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  newer = [
      {
          "role": "separator",
          "event_index": 1008
      },
      _msg("user", "newest ask", "2026-07-10T08:00:09Z", 1009),
  ]
  older = [
      _msg("user", "oldest ask", "2026-07-10T08:00:03Z", 1003),
      _msg("assistant", "oldest answer", "2026-07-10T08:00:04Z", 1004),
      {
          "role": "separator",
          "event_index": 1005
      },
  ]
  payloads = [
      _bootstrap(newer, oldest=1005, has_more=True, event_count=1010, archive_offset=500),
      {
          "messages": older,
          "has_more": False,
          "next_before": 0
      },
  ]
  with patched_cli_gets(cfg, ["session", "dialog", "--turns", "1"], payloads) as get_mock:
    session.main()

  assert get_mock.call_count == 2
  assert get_mock.call_args_list[1].kwargs["params"] == {"before": 1005, "limit": 200}
  out = capsys.readouterr().out
  assert out == (
      "last 1 turns \u00b7 from event 1009 \u00b7 1006 earlier events\n"
      "\n"
      "## user \u00b7 2026-07-10T08:00:09Z \u00b7 event 1009\n"
      "newest ask\n")
