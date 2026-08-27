"""Shared JSON read/write helpers for consistent encoding and error handling."""

import json
import os
import uuid
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


def write_json_atomically(
    path: Path,
    value: object,
    *,
    indent: int | None = None,
    newline: bool = False,
    private: bool = False,
) -> None:
  """Serialize *value* as JSON and swap it into *path* in one step.

  The payload lands in a uniquely named tmp sibling first — the uuid suffix keeps
  concurrent writers of one path from interleaving their bytes — and
  ``os.replace`` publishes it whole, so a crash mid-write leaves the previous
  content intact. ``private=True`` marks the file 0600 before the swap, so a
  secret never appears at its final path readable by anyone but the owner.
  """
  text = json.dumps(value, ensure_ascii=False, indent=indent)
  if newline:
    text += "\n"
  temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
  temporary.write_text(text, encoding="utf-8")
  if private:
    os.chmod(temporary, 0o600)
  os.replace(temporary, path)
