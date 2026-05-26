from __future__ import annotations

from src.core import event_types as ET
from src.core.message_aggregator import MessageAggregator


def test_flat_tool_use_aggregates_into_tools() -> None:
  aggregator = MessageAggregator()

  list(aggregator.feed({"type": ET.TOOL_USE, "name": "Bash", "input": {"cmd": "ls"}}))

  draft = aggregator.pending_draft_message()
  assert draft is not None
  assert len(draft["tools"]) == 1
  assert draft["tools"][0] == {
      "name": "Bash",
      "input": {
          "cmd": "ls"
      },
      "output": "",
      "is_error": False,
  }


def test_flat_tool_result_attaches_to_last_tool() -> None:
  aggregator = MessageAggregator()

  list(aggregator.feed({"type": ET.TOOL_USE, "name": "Bash", "input": {"cmd": "pwd"}}))
  list(aggregator.feed({"type": ET.TOOL_RESULT, "tool_name": "Bash", "content": "/home", "is_error": True}))

  draft = aggregator.pending_draft_message()
  assert draft is not None
  assert draft["tools"][0]["output"] == "/home"
  assert draft["tools"][0]["is_error"] is True


def test_text_and_flat_tools_interleave() -> None:
  aggregator = MessageAggregator()

  list(aggregator.feed({"type": ET.ASSISTANT, "message": {"content": [{"type": "text", "text": "hi "}]}}))
  list(aggregator.feed({"type": ET.TOOL_USE, "name": "Bash", "input": {"cmd": "ls"}}))
  list(aggregator.feed({"type": ET.TOOL_RESULT, "tool_name": "Bash", "content": "/home"}))
  deltas = list(aggregator.feed({"type": ET.ASSISTANT, "message": {"content": [{"type": "text", "text": "done"}]}}))

  finalized = [delta for delta in deltas if delta["type"] == "message"]
  assert len(finalized) == 1
  message = finalized[0]["message"]
  assert message["content"] == "hi "
  assert len(message["tools"]) == 1
  assert message["tools"][0]["output"] == "/home"

  draft = aggregator.pending_draft_message()
  assert draft is not None
  assert draft["content"] == "done"
  assert "tools" not in draft or draft["tools"] == []


def test_flat_tool_result_with_empty_buf_is_silent() -> None:
  aggregator = MessageAggregator()

  deltas = list(aggregator.feed({"type": ET.TOOL_RESULT, "tool_name": "Bash", "content": "orphan"}))

  assert deltas == []
  assert aggregator.pending_draft_message() is None
