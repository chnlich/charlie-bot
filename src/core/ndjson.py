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


async def append_ndjson(path: Path, data: dict) -> None:
  """Async-append a single JSON line to an NDJSON file."""
  path.parent.mkdir(parents=True, exist_ok=True)
  async with aiofiles.open(path, "a", encoding="utf-8") as f:
    await f.write(json.dumps(data) + "\n")
