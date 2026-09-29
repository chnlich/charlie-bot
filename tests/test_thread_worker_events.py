"""Incremental read semantics of read_thread_worker_events (the 5 s workers-panel poll path)."""

import json
from pathlib import Path

from conftest import fresh_state_fixture

from src.api import threads as threads_api

TS = "2026-08-31T00:00:00+00:00"

_clear_events_cache = fresh_state_fixture(threads_api._thread_events_cache.clear)


def _write_events(path: Path, blocks: list[str]) -> None:
  path.write_text("".join(blocks), encoding="utf-8")


def _assistant_block(text: str, tool_id: str | None = None) -> str:
  content = [{"type": "text", "text": text}]
  if tool_id is not None:
    content.append({"type": "tool_use", "id": tool_id, "name": "Bash", "input": {"cmd": "ls"}})
  return json.dumps({"type": "assistant", "timestamp": TS, "message": {"content": content}}) + "\n"


def _tool_result_block(tool_use_id: str, content: str) -> str:
  block = {"type": "tool_result", "tool_use_id": tool_use_id, "content": content}
  return json.dumps({"type": "user", "timestamp": TS, "message": {"content": [block]}}) + "\n"


def test_repeated_reads_with_appends_match_one_full_read(tmp_path: Path) -> None:
  path = tmp_path / "events.jsonl"
  _write_events(path, [_assistant_block("hello", tool_id="t1"), '{"type": "system"}\n', "\n", "{bad json\n"])
  first = threads_api.read_thread_worker_events(path)

  with path.open("a", encoding="utf-8") as f:
    f.write(_tool_result_block("t1", "out"))
  second = threads_api.read_thread_worker_events(path)

  assert [e.model_dump() for e in second][:len(first)] == [e.model_dump() for e in first]

  threads_api._thread_events_cache.clear()
  full = threads_api.read_thread_worker_events(path)
  # An event without a stored timestamp gets now() at parse time, so a from-
  # scratch re-parse differs in that field alone; everything else must match.
  absent = {"timestamp"}
  assert [e.model_dump(exclude=absent) for e in second] == [e.model_dump(exclude=absent) for e in full]
  assert [e.type for e in full] == ["assistant", "tool_use", "system", "tool_result"]
  assert full[-1].tool_name == "Bash"
  assert full[0].timestamp.isoformat() == TS


def test_partial_trailing_line_waits_for_completion(tmp_path: Path) -> None:
  path = tmp_path / "events.jsonl"
  _write_events(path, [_assistant_block("first")])
  assert len(threads_api.read_thread_worker_events(path)) == 1

  with path.open("a", encoding="utf-8") as f:
    f.write(f'{{"type": "assistant", "timestamp": "{TS}", "message": {{"content":')
  assert len(threads_api.read_thread_worker_events(path)) == 1

  with path.open("a", encoding="utf-8") as f:
    f.write(' [{"type": "text", "text": "second"}]}}\n')
  events = threads_api.read_thread_worker_events(path)
  assert [e.content for e in events if e.type == "assistant"] == ["first", "second"]


def test_full_body_store_requires_the_snapshot_token(tmp_path: Path) -> None:
  path = tmp_path / "events.jsonl"
  _write_events(path, [_assistant_block("hello", tool_id="t1")])
  threads_api.read_thread_worker_events(path)
  snap = threads_api._thread_events_snapshot(path)
  assert snap is not None
  _rows, entry, offset = snap

  # A concurrent poller consumes an append: the projection moves past the
  # snapshot, and the stale render must not land on it.
  with path.open("a", encoding="utf-8") as f:
    f.write(_tool_result_block("t1", "out"))
  threads_api.read_thread_worker_events(path)
  threads_api.store_thread_events_full_body(path, b"stale", entry, offset)
  assert threads_api.stored_thread_events_full_body(path) is None

  # A fresh snapshot's render stores and serves.
  snap = threads_api._thread_events_snapshot(path)
  assert snap is not None
  _rows, entry, offset = snap
  threads_api.store_thread_events_full_body(path, b"fresh", entry, offset)
  assert threads_api.stored_thread_events_full_body(path) == b"fresh"
