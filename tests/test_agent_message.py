"""Tests for the agent_message cross-session relay (A1 event, A2 route, A3 CLI).

The contract: an ``agent_message`` event carries a message from another agent
session into a session's event log and wakes its master — but it is NOT a real
user message, so the takeoff gate excludes it purely by event type. The
spawner gate code itself stays untouched (the exclusion is by type, like
``scheduled_trigger``).
"""

import hashlib
import json
import pathlib

import conftest
import pytest

from src.infra import constants, text_length
from src.runtime import control_events
from src.runtime.cli import common, session

# ---------------------------------------------------------------------------
# A2 route boundaries
# ---------------------------------------------------------------------------


def test_derived_agent_send_request_id_uses_run_verb_target_and_text() -> None:
  digest = hashlib.sha256(b"run-1\x00session send\x00target\x00status please").hexdigest()[:24]
  assert control_events.derived_agent_send_request_id("run-1", "session send", "target",
                                                      "status please") == (f"agent-send-{digest}")


# ---------------------------------------------------------------------------
# A3 CLI
# ---------------------------------------------------------------------------


def test_cli_session_send_relays_message(tmp_path: pathlib.Path) -> None:
  cfg = conftest.make_cli_config(tmp_path)
  resp = conftest.make_json_response({"status": "accepted"})

  with conftest.patched_cli_post(cfg,
                                 ["session", "send", "target-id", "--message", "relay this", "--session", "caller-id"],
                                 return_value=resp) as post_mock:
    session.main()

  assert post_mock.call_count == 1
  url = post_mock.call_args[0][0]
  assert url.endswith("/api/internal/session-message")
  assert post_mock.call_args[1]["json"] == {
      "session_id": "caller-id",
      "target_session_id": "target-id",
      "content": "relay this",
  }


# ---------------------------------------------------------------------------
# Weighted length (the limit's counting rule)
# ---------------------------------------------------------------------------


def test_weighted_length_counts_cjk_punctuation_and_fullwidth_double() -> None:
  """ASCII counts 1; CJK ideographs, CJK punctuation, and fullwidth forms count 2."""
  assert text_length.weighted_length("plain ascii") == 11
  assert text_length.weighted_length("一" * 1000) == 2000
  assert text_length.weighted_length("。") == 2
  assert text_length.weighted_length("Ａ") == 2
  assert text_length.weighted_length("") == 0


# ---------------------------------------------------------------------------
# The weighted-character limit on the relay route
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# A3 CLI: the refusal exits 2 through the rejection printer
# ---------------------------------------------------------------------------


def test_cli_session_send_maps_a_422_refusal_to_exit_2(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """The over-limit refusal is a rejection, not a server fault: exit 2, effect none."""
  monkeypatch.setattr(common, "_maybe_version_skew_hint", lambda _base_url: None)
  detail = (
      f"Message is 2002 weighted characters; the limit is {constants.MAX_AGENT_MESSAGE_WEIGHTED_CHARS}.\n"
      "CJK characters, CJK punctuation, and fullwidth characters count 2.\n"
      "Write the long content to a file. Send a short message with the conclusion,\n"
      "the request, and the file path.")
  cfg = conftest.make_cli_config(tmp_path)
  resp = conftest.make_json_response({"detail": detail}, status_code=422)

  with pytest.raises(SystemExit) as excinfo, conftest.patched_cli_post(
      cfg, ["session", "send", "target-id", "--message", "a" * 2001, "--session", "caller-id"], return_value=resp):
    session.main()

  assert excinfo.value.code == session.EXIT_SEND_REFUSED
  payload = json.loads(capsys.readouterr().err)
  assert payload["error"] == detail
  assert payload["code"] == "server_error"
  assert payload["effect"] == "none"
