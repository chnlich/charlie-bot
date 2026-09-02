"""Tests for the NDJSON line readers in src/core/ndjson.py.

The count half of the readers must match Python's file-iteration contract
exactly — a final line without a trailing newline counts — because the tail
reader's ``total_line_count`` feeds global ordinal math in the chat paging
paths.
"""

import json
from pathlib import Path

from src.core.ndjson import count_ndjson_lines, parse_ndjson_tail


def _write_ndjson(path: Path, payloads: list[dict], trailing_newline: bool = True) -> None:
  body = "".join(json.dumps(p) + "\n" for p in payloads)
  if not trailing_newline and body:
    body = body[:-1]
  path.write_text(body, encoding="utf-8")


def test_count_ndjson_lines_missing_file(tmp_path: Path) -> None:
  assert count_ndjson_lines(tmp_path / "absent.jsonl") == 0


def test_count_ndjson_lines_empty_file(tmp_path: Path) -> None:
  target = tmp_path / "empty.jsonl"
  target.write_text("", encoding="utf-8")
  assert count_ndjson_lines(target) == 0


def test_count_ndjson_lines_terminated_lines(tmp_path: Path) -> None:
  target = tmp_path / "events.jsonl"
  _write_ndjson(target, [{"i": i} for i in range(5)])
  assert count_ndjson_lines(target) == 5


def test_count_ndjson_lines_unterminated_last_line(tmp_path: Path) -> None:
  target = tmp_path / "events.jsonl"
  _write_ndjson(target, [{"i": i} for i in range(5)], trailing_newline=False)
  assert count_ndjson_lines(target) == 5


def test_count_ndjson_lines_blank_lines_count(tmp_path: Path) -> None:
  target = tmp_path / "events.jsonl"
  target.write_text('{"a": 1}\n\n{"b": 2}\n', encoding="utf-8")
  assert count_ndjson_lines(target) == 3


def test_count_ndjson_lines_spanning_chunks(tmp_path: Path) -> None:
  target = tmp_path / "events.jsonl"
  payload = {"blob": "x" * 2000}
  _write_ndjson(target, [payload] * 600)  # > 1 MiB, crossing one chunk boundary
  assert count_ndjson_lines(target) == 600


def test_parse_ndjson_tail_reports_total_and_tail(tmp_path: Path) -> None:
  target = tmp_path / "events.jsonl"
  _write_ndjson(target, [{"i": i} for i in range(10)])
  events, total, has_more = parse_ndjson_tail(target, 3)
  assert total == 10
  assert has_more is True
  assert [e["i"] for e in events] == [7, 8, 9]


def test_parse_ndjson_tail_unterminated_last_line(tmp_path: Path) -> None:
  target = tmp_path / "events.jsonl"
  _write_ndjson(target, [{"i": i} for i in range(4)], trailing_newline=False)
  events, total, has_more = parse_ndjson_tail(target, 2)
  assert total == 4
  assert has_more is True
  assert [e["i"] for e in events] == [2, 3]


def test_parse_ndjson_tail_window_fallback_reads_whole_tail(tmp_path: Path) -> None:
  # 300 lines of ~3 KB each: the 512 KB tail window holds fewer than the
  # requested 200, forcing the full-file fallback path.
  target = tmp_path / "events.jsonl"
  _write_ndjson(target, [{"i": i, "blob": "x" * 3000} for i in range(300)])
  events, total, has_more = parse_ndjson_tail(target, 200)
  assert total == 300
  assert has_more is True
  assert [e["i"] for e in events] == list(range(100, 300))


def test_parse_ndjson_tail_missing_and_empty(tmp_path: Path) -> None:
  assert parse_ndjson_tail(tmp_path / "absent.jsonl", 3) == ([], 0, False)
  target = tmp_path / "empty.jsonl"
  target.write_text("", encoding="utf-8")
  assert parse_ndjson_tail(target, 3) == ([], 0, False)
