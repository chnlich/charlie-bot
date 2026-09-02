"""Tests for the NDJSON line readers in src/core/ndjson.py.

The count half of the readers must match Python's file-iteration contract
exactly — a final line without a trailing newline counts — because the tail
reader's ``total_line_count`` feeds global ordinal math in the chat paging
paths.
"""

import json
from pathlib import Path

from src.core.ndjson import (
  count_ndjson_lines,
  parse_ndjson_file,
  parse_ndjson_tail,
  parse_ndjson_tail_parseable,
)


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


def _write_mixed(path: Path, chunks: list[str], trailing_newline: bool = True) -> None:
  body = "\n".join(chunks)
  if trailing_newline and body:
    body += "\n"
  path.write_text(body, encoding="utf-8")


def test_parse_ndjson_tail_parseable_missing_and_edge_limits(tmp_path: Path) -> None:
  assert parse_ndjson_tail_parseable(tmp_path / "absent.jsonl", 3) == []
  target = tmp_path / "empty.jsonl"
  target.write_text("", encoding="utf-8")
  assert parse_ndjson_tail_parseable(target, 3) == []
  _write_ndjson(target, [{"i": 1}])
  assert parse_ndjson_tail_parseable(target, 0) == []


def test_parse_ndjson_tail_parseable_matches_full_parse_tail(tmp_path: Path) -> None:
  # Blank and malformed lines never count toward the limit; parity is against
  # parse_ndjson_file's last-N, not the last N physical lines.
  target = tmp_path / "events.jsonl"
  chunks = []
  for i in range(10):
    chunks.append(json.dumps({"i": i}))
    chunks.append("")
    chunks.append("{not json")
  _write_mixed(target, chunks)
  assert parse_ndjson_tail_parseable(target, 3) == parse_ndjson_file(target)[-3:]
  assert [e["i"] for e in parse_ndjson_tail_parseable(target, 3)] == [7, 8, 9]
  assert parse_ndjson_tail_parseable(target, 50) == parse_ndjson_file(target)


def test_parse_ndjson_tail_parseable_unterminated_last_line(tmp_path: Path) -> None:
  target = tmp_path / "events.jsonl"
  _write_ndjson(target, [{"i": i} for i in range(5)], trailing_newline=False)
  assert parse_ndjson_tail_parseable(target, 2) == [{"i": 3}, {"i": 4}]


def test_parse_ndjson_tail_parseable_malformed_final_line_loses_no_event(tmp_path: Path) -> None:
  # A torn final line is skipped like the full parse skips it, so the reader
  # still reaches the *limit* parseable events before it.
  target = tmp_path / "events.jsonl"
  _write_mixed(target, [json.dumps({"i": i}) for i in range(4)] + ['{"i": 4, "tra'], trailing_newline=False)
  assert parse_ndjson_tail_parseable(target, 3) == [{"i": 1}, {"i": 2}, {"i": 3}]


def test_parse_ndjson_tail_parseable_crosses_window_boundary(tmp_path: Path) -> None:
  # 300 lines of ~3 KB each: the 512 KiB window holds fewer than the requested
  # 200 parseable events only when malformed lines eat the slice, so this file
  # also forces the growth path with a malformed band across the boundary.
  target = tmp_path / "events.jsonl"
  chunks = []
  for i in range(300):
    chunks.append(json.dumps({"i": i, "blob": "x" * 3000}))
    if 100 <= i < 150:
      chunks.append('{"malformed": ' + "y" * 3000)
  _write_mixed(target, chunks)
  assert parse_ndjson_tail_parseable(target, 200) == parse_ndjson_file(target)[-200:]
  assert [e["i"] for e in parse_ndjson_tail_parseable(target, 200)] == list(range(100, 300))
