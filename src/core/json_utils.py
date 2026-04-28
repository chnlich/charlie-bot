"""Shared JSON load helpers for consistent encoding and error handling."""

import json
from pathlib import Path

import structlog

log = structlog.get_logger()


def load_json_meta(
    path: Path,
    log_event: str,
    *,
    catch: tuple[type[BaseException], ...] = (json.JSONDecodeError, OSError),
) -> dict | None:
  """Read and parse a JSON metadata file. Returns None if missing or malformed."""
  if not path.exists():
    return None
  try:
    return json.loads(path.read_text(encoding='utf-8'))
  except catch as e:
    log.debug(log_event, path=str(path), error=str(e))
    return None
