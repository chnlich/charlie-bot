"""Shared JSON read/write helpers and the single home of the atomic file-write rule."""

from __future__ import annotations

import contextlib
import json
import os
import uuid
from collections.abc import Callable
from pathlib import Path
from typing import TYPE_CHECKING, BinaryIO, TypeVar

from src.core.log_once import LazyStructlogLogger

# future-annotations keep the hint unevaluated; the pydantic import rides the one
# call that needs it, so the claude-sub launch chain (M108) imports this module
# without the model stack. asyncio does the same: its interpreter+concurrent-
# futures cost is the launch chain's single largest import slice, and this
# module's one async writer is the only reader.
if TYPE_CHECKING:
  from pydantic import BaseModel

BaseModelT = TypeVar("BaseModelT", bound="BaseModel")

log = LazyStructlogLogger()


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


def load_model_meta(path: Path, model_cls: type[BaseModelT]) -> BaseModelT | None:
  """The *model_cls*-validated content of *path*, or None when the file is
  missing or malformed — the pydantic sibling of :func:`load_json_meta`."""
  try:
    return model_cls.model_validate_json(path.read_text(encoding="utf-8"))
  except (OSError, ValueError):
    return None


def load_json_dict(path: Path) -> dict:
  """The parsed JSON object at *path*, or ``{}`` when the file does not exist yet.

  The callers' documents are machine-written caches (the paired write is
  :func:`write_json_atomically`), so a malformed parse raises;
  :func:`load_json_meta` is the tolerant reader for optional metadata files.
  """
  if not path.exists():
    return {}
  return json.loads(path.read_text(encoding="utf-8"))


def atomic_write_text(path: Path, text: str, *, private: bool = False) -> tuple[int, int] | None:
  """Write *text* to *path* atomically, UTF-8 encoded; return the published signature.

  The tmp-naming, 0600, swap, and mid-write cleanup rules are
  :func:`atomic_write_stream`'s; this adapter fixes its payload as encoded
  text.
  """
  return atomic_write_stream(path, lambda stream: stream.write(text.encode("utf-8")), private=private)


def atomic_write_stream(path: Path,
                        write: Callable[[BinaryIO], None],
                        *,
                        private: bool = False) -> tuple[int, int] | None:
  """Stream the payload ``write`` emits into *path* atomically: a uniquely named tmp sibling
  swapped in by ``os.replace``. Returns the published file's ``(mtime_ns, size)`` signature.

  ``write`` receives the tmp sibling open for binary writing and runs to
  completion before the swap, so a large payload never materializes as one
  object. The uuid suffix keeps concurrent writers of one path from
  interleaving their bytes. ``os.replace`` publishes the payload whole: a
  crash mid-write leaves the previous content intact and no half-written tmp
  behind. ``private=True`` marks the file 0600, so a secret never appears at
  its final path readable by anyone but the owner.

  The signature is the tmp's own stat taken after close, before the swap: the
  swap publishes that inode unchanged, so the returned pair is the signature
  ``stat_signature`` reads at *path* until another writer's swap replaces the
  inode. A caller keying a parse memo on it proves the file still carries the
  bytes this call wrote. None means the stat failed; the swap still ran.

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
    try:
      st = os.stat(temporary)
    except OSError:
      st = None
    os.replace(temporary, path)
    return None if st is None else (st.st_mtime_ns, st.st_size)
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
  import asyncio

  path.parent.mkdir(parents=True, exist_ok=True)
  await asyncio.to_thread(atomic_write_text, path, model.model_dump_json(indent=2))
