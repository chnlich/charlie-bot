"""NDJSON (newline-delimited JSON) file utilities."""

import json
from pathlib import Path

import aiofiles
import structlog

log = structlog.get_logger()


def parse_ndjson_file(path: Path) -> list[dict]:
  """Sync read+parse an NDJSON file. Skips blank/malformed lines."""
  if not path.exists():
    return []
  events: list[dict] = []
  with open(path, "r", encoding="utf-8") as f:
    for line in f:
      line = line.strip()
      if not line:
        continue
      try:
        events.append(json.loads(line))
      except json.JSONDecodeError as e:
        log.debug("ndjson_parse_skip", error=str(e))
  return events


def parse_ndjson_tail(path: Path, limit: int = 200) -> tuple[list[dict], int, bool]:
  """Read the last *limit* lines from an NDJSON file using seek-from-end.

  Returns (events, total_line_count, has_more).
  """
  if not path.exists():
    return [], 0, False

  chunk_size = 8192
  with open(path, "rb") as f:
    # Fast-count total lines
    total = 0
    for _ in f:
      total += 1
    if total == 0:
      return [], 0, False

    has_more = total > limit
    take = min(limit, total)

    # Seek-from-end to collect the last `take` raw lines
    f.seek(0, 2)
    file_size = f.tell()
    buf = b""
    lines: list[bytes] = []
    pos = file_size

    while pos > 0 and len(lines) < take + 1:
      read_size = min(chunk_size, pos)
      pos -= read_size
      f.seek(pos)
      buf = f.read(read_size) + buf
      lines = buf.split(b"\n")

    # lines may have an empty trailing element from the final \n
    raw_lines = [ln for ln in lines if ln.strip()]
    tail_lines = raw_lines[-take:]

  events: list[dict] = []
  for raw in tail_lines:
    try:
      events.append(json.loads(raw))
    except json.JSONDecodeError as e:
      log.debug("ndjson_tail_parse_skip", error=str(e))

  return events, total, has_more


def parse_ndjson_range(path: Path, start: int, end: int) -> tuple[list[dict], bool]:
  """Read NDJSON lines in range [start, end) by line index.

  Returns (events, has_more) where has_more is True when start > 0.
  """
  if not path.exists():
    return [], False
  events: list[dict] = []
  with open(path, "r", encoding="utf-8") as f:
    for i, line in enumerate(f):
      if i >= end:
        break
      if i < start:
        continue
      line = line.strip()
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
