"""Memoized worker-event rendering for the thread-detail poll path."""

import json
from pathlib import Path

import pytest

from src.core import thread_events
from src.core.thread_events import render_worker_events

TS = "2026-08-31T00:00:00+00:00"
PING = {"type": "ping", "timestamp": TS}
ASSISTANT = {"type": "assistant", "timestamp": TS, "message": {"role": "assistant", "content": [
    {"type": "text", "text": "listing files"},
    {"type": "tool_use", "id": "tu1", "name": "Bash", "input": {"command": "ls"}}]}}
TOOL_RESULT = {"type": "user", "timestamp": TS, "message": {"role": "user", "content": [
    {"type": "tool_result", "tool_use_id": "tu1", "content": "file.txt"}]}}


@pytest.fixture(autouse=True)
def _clear_memo() -> None:
  thread_events.reset_for_tests()


def _append(path: Path, rows: list) -> None:
  with open(path, "a", encoding="utf-8") as f:
    for row in rows:
      f.write((row if isinstance(row, str) else json.dumps(row)) + "\n")


def test_full_render_parses_blocks_and_skips_malformed(tmp_path: Path) -> None:
  path = tmp_path / "events.jsonl"
  _append(path, [ASSISTANT, '{"broken json', "", TOOL_RESULT, PING])

  events = render_worker_events(path)

  assert [(e.type, e.tool_name, e.content) for e in events] == [
      ("assistant", None, "listing files"),
      ("tool_use", "Bash", None),
      ("tool_result", "Bash", "file.txt"),
      ("ping", None, None),
  ]


def test_memo_hit_and_appended_tail_render_incrementally(tmp_path: Path) -> None:
  assert render_worker_events(tmp_path / "missing.jsonl") == []
  path = tmp_path / "events.jsonl"
  _append(path, [ASSISTANT])
  first = render_worker_events(path)

  assert render_worker_events(path) is first

  _append(path, [TOOL_RESULT])

  grown = render_worker_events(path)

  thread_events.reset_for_tests()
  fresh = render_worker_events(path)
  assert [e.model_dump() for e in grown] == [e.model_dump() for e in fresh]
  assert grown is not first  # a memoized list is never extended in place


def test_partial_trailing_line_is_reread_once_completed(tmp_path: Path) -> None:
  path = tmp_path / "events.jsonl"
  _append(path, [ASSISTANT])
  render_worker_events(path)
  partial = json.dumps(TOOL_RESULT)
  with open(path, "a", encoding="utf-8") as f:
    f.write(partial[: len(partial) // 2])

  assert len(render_worker_events(path)) == 2

  with open(path, "a", encoding="utf-8") as f:
    f.write(partial[len(partial) // 2:] + "\n")

  rendered = render_worker_events(path)
  assert [(e.type, e.tool_name) for e in rendered] == [("assistant", None), ("tool_use", "Bash"), ("tool_result", "Bash")]


def test_replaced_or_truncated_file_renders_fresh_content(tmp_path: Path) -> None:
  path = tmp_path / "events.jsonl"
  _append(path, [ASSISTANT, TOOL_RESULT])
  render_worker_events(path)

  path.unlink()
  _append(path, [PING])
  assert [e.type for e in render_worker_events(path)] == ["ping"]

  path.write_text("", encoding="utf-8")
  assert render_worker_events(path) == []

  _append(path, [ASSISTANT])
  assert [e.type for e in render_worker_events(path)] == ["assistant", "tool_use"]
