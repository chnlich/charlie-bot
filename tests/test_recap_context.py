from __future__ import annotations

from src.core import event_types as ET
from src.core.recap import build_recap_context


class FakeSessionManager:
  def __init__(self, events: list[dict]):
    self.events = events
    self.loads: list[tuple[str, int, int]] = []

  def get_chat_event_count_sync(self, session_id: str) -> int:
    assert session_id == "session"
    return len(self.events)

  def load_chat_events_range(self, session_id: str, start: int, end: int) -> tuple[list[dict], bool]:
    assert session_id == "session"
    self.loads.append((session_id, start, end))
    return self.events[start:end], end < len(self.events)


def user(content: str) -> dict:
  return {"type": ET.USER, "content": content}


def assistant(content: str, *, tool_name: str | None = None, tool_input: dict | None = None) -> dict:
  blocks = [{"type": "text", "text": content}]
  if tool_name is not None:
    blocks.append({"type": "tool_use", "name": tool_name, "input": tool_input or {}})
  return {"type": ET.ASSISTANT, "message": {"content": blocks}}


def tool_result(content: str, *, is_error: bool = False) -> dict:
  return {
      "type": ET.USER,
      "message": {
          "content": [{
              "type": "tool_result",
              "content": content,
              "is_error": is_error,
          }]
      },
  }


def master_done() -> dict:
  return {"type": ET.MASTER_DONE, "thinking_seconds": 1}


def test_recap_context_upto_excludes_rounds_beyond_bound() -> None:
  events = [
      user("round 1 ask"),
      assistant("round 1 answer"),
      master_done(),
      user("round 2 ask"),
      assistant("round 2 answer"),
      master_done(),
      user("leaked round ask"),
      assistant("leaked round answer"),
  ]
  mgr = FakeSessionManager(events)

  context = build_recap_context(mgr, "session", upto=5)

  assert mgr.loads == [("session", 0, 6)]
  assert "round 2 ask" in context
  assert "round 2 answer" in context
  assert "leaked round ask" not in context
  assert "leaked round answer" not in context


def test_recap_context_without_upto_uses_actual_last_rounds() -> None:
  events = [
      user("round 1 ask"),
      assistant("round 1 answer"),
      master_done(),
      user("round 2 ask"),
      assistant("round 2 answer"),
      master_done(),
      user("round 3 ask"),
      assistant("round 3 answer"),
      master_done(),
      user("round 4 ask"),
      assistant("round 4 answer"),
  ]
  mgr = FakeSessionManager(events)

  context = build_recap_context(mgr, "session", full_rounds=2)

  assert mgr.loads == [("session", 0, len(events))]
  assert "### Round 1 (condensed)" in context
  assert "### Round 2 (condensed)" in context
  assert "### Round 3 (full)" in context
  assert "### Round 4 (full)" in context


def test_recap_context_condenses_older_rounds_without_tool_noise() -> None:
  events = [
      user("old ask"),
      assistant("old final answer", tool_name="Bash", tool_input={"cmd": "old"}),
      tool_result("old noisy output"),
      {"type": ET.TASK_DELEGATED, "description": "implement old task"},
      {"type": ET.WORKER_SUMMARY, "content": "worker concise", "full_content": "worker full detail"},
      user("recent ask"),
      assistant("recent answer", tool_name="Bash", tool_input={"cmd": "recent"}),
      tool_result("recent output"),
  ]

  context = build_recap_context(FakeSessionManager(events), "session", full_rounds=1)

  assert "User: old ask" in context
  assert "Assistant: old final answer" in context
  assert "Task delegated: implement old task" in context
  assert "Worker summary: worker full detail" in context
  assert "old noisy output" not in context
  assert "\"cmd\": \"old\"" not in context
  assert "**Tool: Bash**" in context
  assert "\"cmd\": \"recent\"" in context
  assert "recent output" in context


def test_recap_context_truncates_each_tool_output() -> None:
  long_output = "ABCDEFGHIJ" + "K" * 40
  events = [
      user("run tool"),
      assistant("running", tool_name="WebFetch", tool_input={"url": "https://example.com"}),
      tool_result(long_output),
  ]

  context = build_recap_context(FakeSessionManager(events), "session", tool_output_cap=10)

  assert "ABCDEFGHI…" in context
  assert "KKKK" not in context


def test_recap_context_auto_injected_users_and_clone_start_are_not_rounds() -> None:
  events = [
      {
          "type": ET.CLONE_START,
          "parent_session_name": "Parent Session",
          "parent_session_id": "parent",
      },
      user("This session was cloned from a previous conversation.\n\nbootstrap"),
      user("real first ask"),
      assistant("first answer"),
      user("[Scheduled trigger fired for task]\nnot a real ask"),
      {
          "type": ET.CLONE_START,
          "parent_session_name": "Another Parent",
          "parent_session_id": "parent-2",
      },
      user("real second ask"),
      assistant("second answer"),
  ]

  context = build_recap_context(FakeSessionManager(events), "session", full_rounds=1)

  assert "### Round 1 (condensed)" in context
  assert "### Round 2 (full)" in context
  assert "### Round 3" not in context
  assert "real first ask" in context
  assert "real second ask" in context
  assert "Scheduled trigger fired" not in context
  assert "Parent Session" not in context
  assert "Another Parent" not in context
