"""NDJSON (newline-delimited JSON) file utilities."""

import asyncio
import json
import os
from collections.abc import Iterable, Iterator
from itertools import islice
from pathlib import Path
from typing import Any, BinaryIO

import numpy as np
import orjson
import structlog

from src.core.memo import StatSignatureMemo

log = structlog.get_logger()

_COUNT_CHUNK_SIZE = 1024 * 1024

# Both tail readers bound their trailing reads to this one window.
_TAIL_WINDOW_SIZE = 512 * 1024

# Bound on _count_memo in files, not bytes: the chat tail page (archived
# sessions) and the event-count callers cycle one live file per view, so a
# small cap covers every concurrently viewed session.
_COUNT_MEMO_LIMIT = 64

# path -> line count, and (path, limit) -> (events, total, has_more). The
# count entry's signature is the pre-scan stat: an append during the scan keys
# the count under the older signature, which no later stat can match, so the
# entry is never served stale — the next call re-stats, misses, and recounts;
# a post-scan signature could instead key a stale count under bytes the scan
# never reached, and that entry would serve until the file changed again.
# Chat event files only append; their atomic archive rewrites replace the
# whole file. Tail entries share their event dicts with every caller;
# consumers must treat them as read-only.
_count_memo: StatSignatureMemo[Path, int] = StatSignatureMemo(_COUNT_MEMO_LIMIT)
_tail_memo: StatSignatureMemo[tuple[Path, int], tuple[list[dict], int, bool]] = StatSignatureMemo(_COUNT_MEMO_LIMIT)


def _count_lines(f: BinaryIO) -> int:
  """Count the lines remaining in an open binary file, reading it once from
  the current position. The count matches Python's file-iteration contract
  (a final line without a trailing newline counts), which the tail reader's
  ``total_line_count`` feeds into global ordinal math. The SIMD count is ~4x
  ``bytes.count`` on the production host (~3 GB/s vs ~0.7 GB/s measured)."""
  total = 0
  last_byte = b""
  while chunk := f.read(_COUNT_CHUNK_SIZE):
    total += int(np.count_nonzero(np.frombuffer(chunk, dtype=np.uint8) == 0x0A))
    last_byte = chunk[-1:]
  if last_byte and last_byte != b"\n":
    total += 1
  return total


# The one skip label for the readers that consume a whole file or an in-memory
# line stream. The windowed readers (tail, tail_parseable, range) name their
# own windows, so a debug log still says which reader skipped the line.
PARSE_SKIP_LOG_EVENT = "ndjson_parse_skip"


def iter_ndjson_events(lines: Iterable[str | bytes], *, log_event: str, log_fields: dict[str, Any]) -> Iterator[dict]:
  """Yield the JSON objects parsed from *lines*, skipping blank and malformed lines.

  The one definition of the NDJSON reader skip contract: a line that strips to
  empty is invisible, and a line the parser rejects logs *log_event* (plus
  *log_fields* and the parse error) at debug level and yields nothing. Lazy, so
  first-match and early-stop readers terminate without reading the rest.
  The parser is orjson, ~2x stdlib json.loads per line measured on the live
  corpora; orjson rejects the stdlib json NaN/Infinity extensions and float
  literals that overflow a double (those lines skip as malformed), and ints at
  or beyond 2**64 parse as float where stdlib keeps exact precision.
  """
  for raw_line in lines:
    line = raw_line.strip()
    if not line:
      continue
    try:
      yield orjson.loads(line)
    except ValueError as e:
      log.debug(log_event, error=str(e), **log_fields)


def parse_ndjson_file(path: Path) -> list[dict]:
  """Sync read+parse an NDJSON file. Skips blank/malformed lines."""
  if not path.exists():
    return []
  with open(path, encoding="utf-8") as f:
    return list(iter_ndjson_events(f, log_event=PARSE_SKIP_LOG_EVENT, log_fields={}))


def count_ndjson_lines(path: Path) -> int:
  """Return the number of persisted NDJSON lines without parsing JSON.

  Memoized on the file's (mtime_ns, size): a repeat call over an unchanged
  file pays one stat and zero file bytes. A missing file answers 0 fresh
  every call — it has no signature to memoize, matching the sibling memo
  policy in plans.py.
  """
  if not path.exists():
    return 0
  st = path.stat()
  memoized = _count_memo.fresh(path, st)
  if memoized is not None:
    return memoized
  with open(path, "rb") as f:
    total = _count_lines(f)
  _count_memo.record(path, st, total)
  return total


def parse_ndjson_tail(path: Path, limit: int = 200) -> tuple[list[dict], int, bool]:
  """Read the last *limit* lines from an NDJSON file using seek-from-end.

  Returns (events, total_line_count, has_more). The whole page memoizes on
  (mtime_ns, size): the chat tail page is re-requested per view of an
  unchanged file (SPA switches, re-materializations), where a repeat pays
  one stat and zero file bytes; an append re-reads the count and window.
  """
  if not path.exists():
    return [], 0, False
  st = path.stat()
  memoized = _tail_memo.fresh((path, limit), st)
  if memoized is not None:
    return memoized

  total = count_ndjson_lines(path)
  if total == 0:
    return [], 0, False

  has_more = total > limit
  take = min(limit, total)
  if take == 0:
    return [], total, has_more

  with open(path, "rb") as f:
    f.seek(0, 2)
    file_size = f.tell()
    if file_size <= _TAIL_WINDOW_SIZE:
      f.seek(0)
      tail_lines = [line for line in f.read().split(b"\n") if line.strip()][-take:]
    else:
      window_start = file_size - _TAIL_WINDOW_SIZE
      f.seek(window_start)
      window = f.read()
      split_lines = window.split(b"\n")

      complete_lines = split_lines[1:]
      if complete_lines and complete_lines[-1] == b"":
        complete_lines = complete_lines[:-1]
      window_lines = [line for line in complete_lines if line.strip()]

      if len(window_lines) < take:
        f.seek(0)
        tail_lines = [line for line in f.read().split(b"\n") if line.strip()][-take:]
      else:
        tail_lines = window_lines[-take:]

  events = list(iter_ndjson_events(tail_lines, log_event="ndjson_tail_parse_skip", log_fields={}))

  _tail_memo.record((path, limit), st, (events, total, has_more))
  return events, total, has_more


def iter_ndjson_events_from_end(path: Path, *, log_event: str, log_fields: dict[str, Any]) -> Iterator[dict]:
  """Yield the JSON objects parsed from *path*, newest line first.

  Same skip contract as :func:`iter_ndjson_events` (a line that strips to
  empty is invisible, a line the parser rejects logs and yields nothing).
  512 KiB segments from the end walk lines backwards, the segment's
  left-truncated first line carried into the next older segment, so a consumer
  that stops early never reads the bytes past its answer. A missing file
  yields nothing.
  """
  if not path.exists():
    return
  with open(path, "rb") as f:
    f.seek(0, 2)
    pos = f.tell()
    carry = b""  # the current segment's left-truncated first line, completed by the next older segment
    while pos > 0:
      start = max(0, pos - _TAIL_WINDOW_SIZE)
      f.seek(start)
      lines = (f.read(pos - start) + carry).split(b"\n")
      carry = b""
      if start > 0:
        carry = lines[0]
        lines = lines[1:]
      if lines and lines[-1] == b"":
        lines = lines[:-1]
      yield from iter_ndjson_events(reversed(lines), log_event=log_event, log_fields=log_fields)
      pos = start


def parse_ndjson_tail_parseable(path: Path, limit: int) -> list[dict]:
  """Return the last *limit* parseable events of an NDJSON file, in file order.

  Same result as ``parse_ndjson_file(path)[-limit:]`` — blank and malformed
  lines are skipped and never count toward *limit* — but reads only as many
  trailing bytes as the limit needs (the from-the-end walk of
  :func:`iter_ndjson_events_from_end`, stopped by *limit*). Callers that must
  see every line (exact prefixes, global ordinals) keep ``parse_ndjson_file``;
  this reader is for the "last N of whatever parsed" budget the worker-summary
  readers carry. A missing file returns ``[]`` and *limit* <= 0 returns
  ``[]``.
  """
  if limit <= 0:
    return []
  collected = list(
      islice(iter_ndjson_events_from_end(path, log_event="ndjson_tail_parseable_skip", log_fields={}), limit))
  collected.reverse()
  return collected


def parse_ndjson_range(path: Path, start: int, end: int) -> tuple[list[dict], bool]:
  """Read NDJSON lines in range [start, end) by line index.

  Returns (events, has_more) where has_more is True when start > 0.
  """
  if not path.exists():
    return [], False
  with open(path, encoding="utf-8") as f:
    events = list(iter_ndjson_events(islice(f, start, end), log_event="ndjson_range_parse_skip", log_fields={}))
  return events, start > 0


def _append_ndjson_sync(path: Path, line: str) -> None:
  """One open(O_APPEND)+write+close per append.

  The handle is opened per call on purpose: O_APPEND re-resolves the path, so
  an append after an atomic archive rewrite (or a recreate) lands on the new
  file, never behind a stale handle pointing at a swapped-out inode. The loop
  keeps the io stack's write-all contract: a short write keeps going instead
  of publishing a torn line.
  """
  view = memoryview(line.encode("utf-8"))
  fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o666)
  try:
    while view:
      view = view[os.write(fd, view):]
    os.fdatasync(fd)  # durable before close: a hard VM kill must not leave a size-without-data NUL hole
  finally:
    os.close(fd)


async def append_ndjson(path: Path, data: dict) -> None:
  """Async-append a single JSON line to an NDJSON file."""
  path.parent.mkdir(parents=True, exist_ok=True)
  await asyncio.to_thread(_append_ndjson_sync, path, json.dumps(data) + "\n")
