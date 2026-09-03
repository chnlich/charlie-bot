"""Shared JSON read/write helpers and the single home of the atomic file-write rule."""

import asyncio
import contextlib
import json
import os
import uuid
from collections.abc import Callable
from pathlib import Path
from typing import BinaryIO

import structlog
from pydantic import BaseModel

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


def atomic_write_text(path: Path, text: str, *, private: bool = False) -> None:
  """Write *text* to *path* atomically, UTF-8 encoded.

  The tmp-naming, 0600, swap, and mid-write cleanup rules are
  :func:`atomic_write_stream`'s; this adapter fixes its payload as encoded
  text.
  """
  atomic_write_stream(path, lambda stream: stream.write(text.encode("utf-8")), private=private)


def atomic_write_stream(path: Path, write: Callable[[BinaryIO], None], *, private: bool = False) -> None:
  """Stream the payload ``write`` emits into *path* atomically: a uniquely named tmp sibling swapped in by ``os.replace``.

  ``write`` receives the tmp sibling open for binary writing and runs to
  completion before the swap, so a large payload never materializes as one
  object. The uuid suffix keeps concurrent writers of one path from
  interleaving their bytes. ``os.replace`` publishes the payload whole: a
  crash mid-write leaves the previous content intact and no half-written tmp
  behind. ``private=True`` marks the file 0600, so a secret never appears at
  its final path readable by anyone but the owner.

  The swap must stay an ``os.replace`` attribute lookup on this module's ``os``:
  tests hook the swap by patching ``os.replace`` here.
  """
  temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
  try:
    if private:
      # 0600 at creation: a umask can strip permission bits, never add them, so
      # the payload is never briefly wider-readable before the swap.
      temporary.touch(mode=0o600)
    with temporary.open("wb") as stream:
      write(stream)
    os.replace(temporary, path)
  except BaseException:
    with contextlib.suppress(OSError):
      temporary.unlink()
    raise


def write_json_atomically(
    path: Path,
    value: object,
    *,
    indent: int | None = None,
    newline: bool = False,
    private: bool = False,
) -> None:
  """Serialize *value* as JSON and swap it into *path* in one step.

  ``private=True`` marks the file 0600 (see :func:`atomic_write_text`).
  """
  text = json.dumps(
      value,
      ensure_ascii=False,
      indent=indent,
      # indent=None must stay compact: json's default (", ", ": ") padding
      # would whitespace-inflate every compact caller's file.
      separators=(",", ":") if indent is None else None,
  )
  if newline:
    text += "\n"
  atomic_write_text(path, text, private=private)


async def write_model_json_atomically(path: Path, model: BaseModel) -> None:
  """Serialize *model* as indented JSON and publish it at *path* under :func:`atomic_write_text`'s rule.

  Creates the parent directory when missing. Callers' readers parse the file
  from executor threads with no coordination, so the publish must stay an
  async atomic swap: a plain truncate-write lets them observe a half-written
  file.
  """
  path.parent.mkdir(parents=True, exist_ok=True)
  await asyncio.to_thread(atomic_write_text, path, model.model_dump_json(indent=2))
