"""NDJSON (newline-delimited JSON) file utilities."""

import json
from pathlib import Path
from typing import BinaryIO

import aiofiles
import numpy as np
import structlog

log = structlog.get_logger()

_COUNT_CHUNK_SIZE = 1024 * 1024
_TAIL_PARSEABLE_WINDOW = 512 * 1024


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


def parse_ndjson_file(path: Path) -> list[dict]:
  """Sync read+parse an NDJSON file. Skips blank/malformed lines."""
  if not path.exists():
    return []
  events: list[dict] = []
  with open(path, encoding="utf-8") as f:
    for raw_line in f:
      line = raw_line.strip()
      if not line:
        continue
      try:
        events.append(json.loads(line))
      except json.JSONDecodeError as e:
        log.debug("ndjson_parse_skip", error=str(e))
  return events


def count_ndjson_lines(path: Path) -> int:
  """Return the number of persisted NDJSON lines without parsing JSON."""
  if not path.exists():
    return 0
  with open(path, "rb") as f:
    return _count_lines(f)


def parse_ndjson_tail(path: Path, limit: int = 200) -> tuple[list[dict], int, bool]:
  """Read the last *limit* lines from an NDJSON file using seek-from-end.

  Returns (events, total_line_count, has_more).
  """
  if not path.exists():
    return [], 0, False

  tail_window_size = 512 * 1024
  with open(path, "rb") as f:
    # Fast-count total lines
    total = _count_lines(f)
    if total == 0:
      return [], 0, False

    has_more = total > limit
    take = min(limit, total)
    if take == 0:
      return [], total, has_more

    f.seek(0, 2)
    file_size = f.tell()
    if file_size <= tail_window_size:
      f.seek(0)
      tail_lines = [line for line in f.read().split(b"\n") if line.strip()][-take:]
    else:
      window_start = file_size - tail_window_size
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

  events: list[dict] = []
  for raw in tail_lines:
    try:
      events.append(json.loads(raw))
    except json.JSONDecodeError as e:
      log.debug("ndjson_tail_parse_skip", error=str(e))

  return events, total, has_more


def parse_ndjson_tail_parseable(path: Path, limit: int) -> list[dict]:
  """Return the last *limit* parseable events of an NDJSON file, in file order.

  Same result as ``parse_ndjson_file(path)[-limit:]`` — blank and malformed
  lines are skipped and never count toward *limit* — but reads only as many
  trailing bytes as the limit needs: 512 KiB segments from the end walk lines
  backwards, the segment's left-truncated first line carried into the next
  older segment, stopping once they collect *limit* events or cover the whole
  file. Callers that must see every line (exact prefixes, global ordinals)
  keep ``parse_ndjson_file``; this reader is for the "last N of whatever
  parsed" budget the worker-summary readers carry. A missing file returns
  ``[]`` and *limit* <= 0 returns ``[]``.
  """
  collected: list[dict] = []
  if limit <= 0 or not path.exists():
    return collected
  with open(path, "rb") as f:
    f.seek(0, 2)
    pos = f.tell()
    carry = b""  # the current segment's left-truncated first line, completed by the next older segment
    while pos > 0 and len(collected) < limit:
      start = max(0, pos - _TAIL_PARSEABLE_WINDOW)
      f.seek(start)
      lines = (f.read(pos - start) + carry).split(b"\n")
      carry = b""
      if start > 0:
        carry = lines[0]
        lines = lines[1:]
      if lines and lines[-1] == b"":
        lines = lines[:-1]
      for raw in reversed(lines):
        line = raw.strip()
        if not line:
          continue
        try:
          collected.append(json.loads(line))
        except json.JSONDecodeError as e:
          log.debug("ndjson_tail_parseable_skip", error=str(e))
        if len(collected) >= limit:
          break
      pos = start
  collected.reverse()
  return collected


def parse_ndjson_range(path: Path, start: int, end: int) -> tuple[list[dict], bool]:
  """Read NDJSON lines in range [start, end) by line index.

  Returns (events, has_more) where has_more is True when start > 0.
  """
  if not path.exists():
    return [], False
  events: list[dict] = []
  with open(path, encoding="utf-8") as f:
    for i, raw_line in enumerate(f):
      if i >= end:
        break
      if i < start:
        continue
      line = raw_line.strip()
      if not line:
        continue
      try:
        events.append(json.loads(line))
      except json.JSONDecodeError as e:
        log.debug("ndjson_range_parse_skip", error=str(e))
  return events, start > 0


async def append_ndjson(path: Path, data: dict) -> None:
  """Async-append a single JSON line to an NDJSON file."""
  path.parent.mkdir(parents=True, exist_ok=True)
  async with aiofiles.open(path, "a", encoding="utf-8") as f:
    await f.write(json.dumps(data) + "\n")
