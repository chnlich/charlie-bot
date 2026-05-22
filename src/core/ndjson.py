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


def count_ndjson_lines(path: Path) -> int:
  """Return the number of persisted NDJSON lines without parsing JSON."""
  if not path.exists():
    return 0
  count = 0
  with open(path, "rb") as f:
    for _ in f:
      count += 1
  return count


def parse_ndjson_tail(path: Path, limit: int = 200) -> tuple[list[dict], int, bool]:
  """Read the last *limit* lines from an NDJSON file using seek-from-end.

  Returns (events, total_line_count, has_more).
  """
  if not path.exists():
    return [], 0, False

  tail_window_size = 512 * 1024
  with open(path, "rb") as f:
    # Fast-count total lines
    total = 0
    for _ in f:
      total += 1
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
