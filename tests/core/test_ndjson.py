"""Tests for the NDJSON line readers in src/core/ndjson.py.

The count half of the readers must match Python's file-iteration contract
exactly — a final line without a trailing newline counts — because the tail
reader's ``total_line_count`` feeds global ordinal math in the chat paging
paths.
"""

import asyncio
import json
import os
from pathlib import Path

import orjson
import pytest

from src.core.ndjson import (
    _COUNT_MEMO_LIMIT,
    append_ndjson,
    count_ndjson_lines,
    iter_ndjson_events,
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


def test_iter_ndjson_events_matches_stdlib_parse_over_event_shapes() -> None:
  # The parser swap must be output-identical to stdlib json.loads for every
  # value shape the writers emit: nesting, CJK, escapes, floats (exponents,
  # in-range extremes), duplicate keys, empty containers.
  lines = [
      json.dumps(p)
      for p in [
          {"i": 1, "nested": {"a": [1, {"b": None}], "c": []}, "d": {}},
          {"text": "引数 'вектор' — ✅ \U0001f680 \\n \"quoted\" \\"},
          {"f": [0.5, -3.25e-8, 1e308, -0.0, 1.0]},
          {"dup": 1, "dup": 2},
          {"big": 2**31, "neg": -2**31, "zero": 0},
          {"b": True, "n": None},
      ]
  ] + ["  " + json.dumps({"padded": True}) + "  "]
  assert list(iter_ndjson_events(lines, log_event="t", log_fields={})) == [json.loads(l) for l in lines]


def test_iter_ndjson_events_accepts_bytes_lines_with_cjk() -> None:
  line = json.dumps({"text": "引数"}).encode("utf-8")
  assert list(iter_ndjson_events([line, b'  {"i": 1}  '], log_event="t", log_fields={})) == [
      {"text": "引数"},
      {"i": 1},
  ]


@pytest.mark.parametrize(
    "bad_line",
    [
        "{not json",
        '{"a": NaN}',
        '{"a": Infinity}',
        '{"a": -Infinity}',
        '{"a": 1e400}',
    ],
    ids=["malformed", "nan", "infinity", "neg-infinity", "double-overflow"],
)
def test_iter_ndjson_events_skips_rejected_lines_without_raising(bad_line: str) -> None:
  # The skip contract covers orjson's stricter boundary: the stdlib NaN/Infinity
  # extensions and double-overflow floats are malformed lines, invisible like
  # any other.
  assert list(iter_ndjson_events([bad_line, '{"ok": 1}'], log_event="t", log_fields={})) == [{"ok": 1}]


def test_iter_ndjson_events_coerces_64bit_ints_to_float() -> None:
  # orjson's int boundary: exact through 2**64-1, float past it — the one
  # value-level difference from stdlib, documented in the reader contract.
  exact = list(iter_ndjson_events(['{"a": 18446744073709551615}'], log_event="t", log_fields={}))
  coerced = list(iter_ndjson_events(['{"a": 18446744073709551616}'], log_event="t", log_fields={}))
  assert exact == [{"a": 18446744073709551615}]
  assert coerced == [{"a": 1.8446744073709552e19}]
  assert isinstance(coerced[0]["a"], float)


def test_parse_ndjson_file_applies_the_skip_contract(tmp_path: Path) -> None:
  target = tmp_path / "events.jsonl"
  target.write_text(
      '{"i": 1}\n\n{"i": 2}\n{not json}\n{"i": 3, "x": NaN}\n{"i": 4}', encoding="utf-8")
  # The NaN-bearing line sits inside the skip contract's boundary and is
  # invisible like the malformed one.
  assert parse_ndjson_file(target) == [{"i": 1}, {"i": 2}, {"i": 4}]

def test_count_ndjson_lines_empty_file(tmp_path: Path) -> None:
  target = tmp_path / "empty.jsonl"
  target.write_text("", encoding="utf-8")
  assert count_ndjson_lines(target) == 0


@pytest.mark.parametrize("trailing_newline", [True, False], ids=["terminated", "unterminated"])
def test_count_ndjson_lines_final_line_counts(tmp_path: Path, trailing_newline: bool) -> None:
  target = tmp_path / "events.jsonl"
  _write_ndjson(target, [{"i": i} for i in range(5)], trailing_newline=trailing_newline)
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


def _spy_opens(monkeypatch: pytest.MonkeyPatch) -> list[str]:
  calls: list[str] = []
  real_open = open

  def spy(file, mode="r", *args, **kwargs):
    calls.append(str(file))
    return real_open(file, mode, *args, **kwargs)

  monkeypatch.setattr("builtins.open", spy)
  return calls


def test_count_ndjson_lines_memo_hit_skips_the_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  target = tmp_path / "events.jsonl"
  _write_ndjson(target, [{"i": i} for i in range(5)])
  assert count_ndjson_lines(target) == 5
  calls = _spy_opens(monkeypatch)
  assert count_ndjson_lines(target) == 5
  assert str(target) not in calls


def test_count_ndjson_lines_memo_recounts_after_append(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  target = tmp_path / "events.jsonl"
  _write_ndjson(target, [{"i": i} for i in range(5)])
  assert count_ndjson_lines(target) == 5
  calls = _spy_opens(monkeypatch)
  with open(target, "a", encoding="utf-8") as f:
    f.write(json.dumps({"i": 5}) + "\n")
  assert count_ndjson_lines(target) == 6
  assert str(target) in calls


def test_count_ndjson_lines_memo_missing_file_never_memoized(tmp_path: Path) -> None:
  target = tmp_path / "absent.jsonl"
  assert count_ndjson_lines(target) == 0
  _write_ndjson(target, [{"i": 1}, {"i": 2}])
  assert count_ndjson_lines(target) == 2


def test_parse_ndjson_tail_memo_hit_parity_and_zero_opens(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  target = tmp_path / "events.jsonl"
  _write_ndjson(target, [{"i": i} for i in range(50)])
  first = parse_ndjson_tail(target, 3)
  calls = _spy_opens(monkeypatch)
  # A steady-state repeat pays one stat and zero opens: the count and the
  # tail window both come from the memos instead of re-reading the bytes.
  for _ in range(5):
    assert parse_ndjson_tail(target, 3) == first
  assert str(target) not in calls


def test_parse_ndjson_tail_memo_recomputes_after_append(tmp_path: Path) -> None:
  target = tmp_path / "events.jsonl"
  _write_ndjson(target, [{"i": i} for i in range(50)])
  first = parse_ndjson_tail(target, 3)
  with open(target, "a", encoding="utf-8") as f:
    f.write(json.dumps({"i": 50}) + "\n")
  events, total, has_more = parse_ndjson_tail(target, 3)
  assert (total, has_more) == (51, True)
  assert events == [{"i": 48}, {"i": 49}, {"i": 50}]
  assert events != first[0]


def test_count_memo_lru_eviction_bounds_size(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  first_path = tmp_path / "f0.jsonl"
  _write_ndjson(first_path, [{"i": 0}])
  assert count_ndjson_lines(first_path) == 1
  for i in range(1, _COUNT_MEMO_LIMIT + 1):
    _write_ndjson(tmp_path / f"f{i}.jsonl", [{"i": i}])
    count_ndjson_lines(tmp_path / f"f{i}.jsonl")
  calls = _spy_opens(monkeypatch)
  # The oldest entry was evicted and re-reads; every entry still resident
  # answers without touching its file.
  assert count_ndjson_lines(first_path) == 1
  assert str(first_path) in calls
  calls.clear()
  assert count_ndjson_lines(tmp_path / f"f{_COUNT_MEMO_LIMIT}.jsonl") == 1
  assert not calls


def test_append_ndjson_fdatasyncs_once_after_the_writes(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  target = tmp_path / "events.jsonl"
  ops: list[tuple[str, int]] = []
  real_write, real_fdatasync = os.write, os.fdatasync

  def spy_write(fd: int, data: memoryview) -> int:
    n = real_write(fd, data)
    ops.append(("write", fd))
    return n

  def spy_fdatasync(fd: int) -> None:
    ops.append(("fdatasync", fd))
    real_fdatasync(fd)

  monkeypatch.setattr(os, "write", spy_write)
  monkeypatch.setattr(os, "fdatasync", spy_fdatasync)
  asyncio.run(append_ndjson(target, {"i": 1}))

  # One fdatasync, on the fd that received the writes, after every write.
  fd = ops[0][1]
  assert ops == [("write", fd), ("fdatasync", fd)]
