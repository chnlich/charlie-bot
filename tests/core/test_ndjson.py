"""Tests for the NDJSON line readers in src/core/ndjson.py.

The count half of the readers must match Python's file-iteration contract
exactly — a final line without a trailing newline counts — because the tail
reader's ``total_line_count`` feeds global ordinal math in the chat paging
paths.
"""

import asyncio
import itertools
import json
import os
import pathlib
from typing import IO, Any

import pytest

from src.core import ndjson


def _write_ndjson(path: pathlib.Path, payloads: list[dict], trailing_newline: bool = True) -> None:
  body = "".join(json.dumps(p) + "\n" for p in payloads)
  if not trailing_newline and body:
    body = body[:-1]
  path.write_text(body, encoding="utf-8")


def test_iter_ndjson_events_matches_stdlib_parse_over_event_shapes() -> None:
  # The parser swap must be output-identical to stdlib json.loads for every
  # value shape the writers emit: nesting, CJK, escapes, floats (exponents,
  # in-range extremes), duplicate keys, empty containers.
  # The dup line is a raw string: a Python dict literal collapses duplicate
  # keys before json.dumps ever runs.
  lines = [
      json.dumps(p) for p in [
          {
              "i": 1,
              "nested": {
                  "a": [1, {
                      "b": None
                  }],
                  "c": []
              },
              "d": {}
          },
          {
              "text": "引数 'вектор' — ✅ \U0001f680 \\n \"quoted\" \\"
          },
          {
              "f": [0.5, -3.25e-8, 1e308, -0.0, 1.0]
          },
          {
              "big": 2**31,
              "neg": -2**31,
              "zero": 0
          },
          {
              "b": True,
              "n": None
          },
      ]
  ] + ["  " + json.dumps({"padded": True}) + "  ", '{"dup": 1, "dup": 2}']
  assert list(ndjson.iter_ndjson_events(lines, log_event="t",
                                        log_fields={})) == [json.loads(raw_line) for raw_line in lines]


def test_parse_ndjson_line_applies_the_skip_contract_per_line() -> None:
  # The one-line contract home: a blank (or whitespace-only) line and a line
  # the parser rejects (including the orjson NaN/Infinity boundary) answer
  # None; a parseable line answers its dict.
  assert ndjson.parse_ndjson_line('{"i": 1}', log_event="t", log_fields={}) == {"i": 1}
  assert ndjson.parse_ndjson_line('  {"i": 1}  ', log_event="t", log_fields={}) == {"i": 1}
  assert ndjson.parse_ndjson_line("", log_event="t", log_fields={}) is None
  assert ndjson.parse_ndjson_line("   \n", log_event="t", log_fields={}) is None
  assert ndjson.parse_ndjson_line("{not json", log_event="t", log_fields={}) is None
  assert ndjson.parse_ndjson_line('{"a": NaN}', log_event="t", log_fields={}) is None


def test_parse_ndjson_line_bytes_torn_multibyte_parses_as_replacement_char() -> None:
  # orjson rejects invalid UTF-8 before the JSON structure: the contract's
  # replace fallback decides the line, so a torn multibyte char inside an
  # otherwise valid line parses as U+FFFD instead of skipping as malformed.
  line = b'{"text": "ok\xff"}'
  assert ndjson.parse_ndjson_line(line, log_event="t", log_fields={}) == {"text": "ok\ufffd"}


def test_parse_ndjson_file_matches_the_from_end_walk_over_mixed_corpora(tmp_path: pathlib.Path) -> None:
  # The whole-file parse and the from-the-end walk share one skip contract over
  # one line domain, so a corpus with blank, whitespace, malformed, torn-UTF-8,
  # hard-corrupt, multi-megabyte and unterminated-final lines parses identically
  # from both ends — the parity every events reader rests on.
  target = tmp_path / "events.jsonl"
  giant = json.dumps({"i": "giant", "blob": "x" * (5 * 1024 * 1024)})
  with target.open("wb") as f:
    f.write(b'{"i": 0}\n')
    f.write(b"\n")
    f.write(b"   \n")
    f.write(b"{not json\n")
    f.write(b'{"i": 1, "torn": "ok\xff"}\n')
    f.write(b'{"i": 2, "hard": "ok\xff\n')
    f.write(giant.encode() + b"\n")
    f.write(b'{"i": 3}')
  assert ndjson.parse_ndjson_file(target) == list(
      ndjson.iter_ndjson_events_from_end(target, log_event="t", log_fields={}))[::-1]


def test_parse_ndjson_file_multi_megabyte_lines_parse_whole(tmp_path: pathlib.Path) -> None:
  # A line far larger than any read chunk parses whole: the mapping has no
  # chunk boundaries, so the parse output is the line's JSON object exactly.
  target = tmp_path / "events.jsonl"
  payload = {"blob": "x" * (8 * 1024 * 1024)}
  _write_ndjson(target, [payload, {"i": 1}])
  assert ndjson.parse_ndjson_file(target) == [payload, {"i": 1}]


def _write_mixed(path: pathlib.Path, chunks: list[str], trailing_newline: bool = True) -> None:
  body = "\n".join(chunks)
  if trailing_newline and body:
    body += "\n"
  path.write_text(body, encoding="utf-8")


def test_iter_ndjson_events_from_end_tail_limit_skips_a_malformed_band(tmp_path: pathlib.Path) -> None:
  # 300 lines of ~3 KB each with a malformed band in the middle: the newest
  # 200 parseable events the walk answers equal the whole-file parse's last
  # 200, so lines the parser rejects never count toward the consumer's limit.
  target = tmp_path / "events.jsonl"
  chunks = []
  for i in range(300):
    chunks.append(json.dumps({"i": i, "blob": "x" * 3000}))
    if 100 <= i < 150:
      chunks.append('{"malformed": ' + "y" * 3000)
  _write_mixed(target, chunks)
  newest_first = list(itertools.islice(ndjson.iter_ndjson_events_from_end(target, log_event="t", log_fields={}), 200))
  assert newest_first[::-1] == ndjson.parse_ndjson_file(target)[-200:]
  assert [e["i"] for e in newest_first[::-1]] == list(range(100, 300))


def _spy_opens(monkeypatch: pytest.MonkeyPatch) -> list[str]:
  calls: list[str] = []
  real_open = open

  def spy(file: pathlib.Path | str, mode: str = "r", *args: object, **kwargs: object) -> IO[Any]:
    calls.append(str(file))
    return real_open(file, mode, *args, **kwargs)

  monkeypatch.setattr("builtins.open", spy)
  return calls


def test_count_ndjson_lines_memo_recounts_after_append(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  target = tmp_path / "events.jsonl"
  _write_ndjson(target, [{"i": i} for i in range(5)])
  assert ndjson.count_ndjson_lines(target) == 5
  calls = _spy_opens(monkeypatch)
  with open(target, "a", encoding="utf-8") as f:
    f.write(json.dumps({"i": 5}) + "\n")
  assert ndjson.count_ndjson_lines(target) == 6
  assert str(target) in calls


def test_append_ndjson_fdatasyncs_once_after_the_writes(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
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
  asyncio.run(ndjson.append_ndjson(target, {"i": 1}))

  # One fdatasync, on the fd that received the writes, after every write.
  fd = ops[0][1]
  assert ops == [("write", fd), ("fdatasync", fd)]
