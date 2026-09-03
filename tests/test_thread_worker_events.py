"""Incremental read semantics of read_thread_worker_events (the 5 s workers-panel poll path)."""

import json
from pathlib import Path

import pytest

from src.api import threads as threads_api

TS = "2026-08-31T00:00:00+00:00"


@pytest.fixture(autouse=True)
def _clear_events_cache():
  threads_api._thread_events_cache.clear()
  yield
  threads_api._thread_events_cache.clear()


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


def test_shrunk_file_rescans_from_start(tmp_path: Path) -> None:
  path = tmp_path / "events.jsonl"
  _write_events(path, [_assistant_block("longer surviving line"), _assistant_block("tail")])
  assert len(threads_api.read_thread_worker_events(path)) == 2

  _write_events(path, [_assistant_block("replaced")])
  events = threads_api.read_thread_worker_events(path)
  assert [e.content for e in events] == ["replaced"]


def test_tool_result_resolves_tool_name_written_by_an_earlier_read(tmp_path: Path) -> None:
  path = tmp_path / "events.jsonl"
  _write_events(path, [_assistant_block("plan", tool_id="t9")])
  threads_api.read_thread_worker_events(path)

  with path.open("a", encoding="utf-8") as f:
    f.write(_tool_result_block("t9", "done"))
  result = threads_api.read_thread_worker_events(path)[-1]
  assert result.type == "tool_result"
  assert result.tool_name == "Bash"


def test_missing_file_returns_empty_and_drops_stale_cache(tmp_path: Path) -> None:
  path = tmp_path / "events.jsonl"
  assert threads_api.read_thread_worker_events(path) == []

  _write_events(path, [_assistant_block("x")])
  assert len(threads_api.read_thread_worker_events(path)) == 1
  path.unlink()
  assert threads_api.read_thread_worker_events(path) == []
  assert threads_api._thread_events_cache == {}


def test_cache_evicts_beyond_cap(tmp_path: Path) -> None:
  cap = threads_api._THREAD_EVENTS_CACHE_CAP
  paths = [tmp_path / f"events{i}.jsonl" for i in range(cap + 1)]
  for path in paths:
    _write_events(path, [_assistant_block("x")])
    threads_api.read_thread_worker_events(path)

  assert len(threads_api._thread_events_cache) == cap
  assert str(paths[0]) not in threads_api._thread_events_cache
  assert str(paths[-1]) in threads_api._thread_events_cache
