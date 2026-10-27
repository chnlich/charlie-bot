"""Tests for the agent_message cross-session relay (A1 event, A2 route).

The contract: an ``agent_message`` event carries a message from another agent
session into a session's event log and wakes its master — but it is NOT a real
user message, so the takeoff gate excludes it purely by event type. The
spawner gate code itself stays untouched (the exclusion is by type, like
``scheduled_trigger``).
"""

import hashlib

from src.runtime import control_events

# ---------------------------------------------------------------------------
# A2 route boundaries
# ---------------------------------------------------------------------------


def test_derived_agent_send_request_id_uses_run_verb_target_and_text() -> None:
  digest = hashlib.sha256(b"run-1\x00session send\x00target\x00status please").hexdigest()[:24]
  assert control_events.derived_agent_send_request_id("run-1", "session send", "target",
                                                      "status please") == (f"agent-send-{digest}")
