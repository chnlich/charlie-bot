"""opencode's usage log, read for the "opencode" usage source (src/runtime/hooks/usage_sources.py).

opencode 2.0 keeps its history in one SQLite database, ``home.opencode_db()`` under the profile's store. The
``session_message`` table holds one row per message: ``id``, ``session_id``, ``type`` (the role), ``seq``,
``time_created``, ``time_updated`` (wall-clock ms, set by every insert and update) and a JSON ``data`` blob. Only
``type = 'assistant'`` rows carry usage. Such a row's blob carries ``model`` (``id`` and ``providerID``), ``tokens``
(``input``, ``output``, ``reasoning``, ``cache.read``, ``cache.write``) and ``time.created``. An error row has no
``tokens`` key.

Record id: ``opencode:<message id>``, one per contributing row; a row whose ``tokens.input`` and ``tokens.output`` are
both 0 or absent contributes nothing. The record's account is the row's provider and its session is the row's session.

Signature: ``<main size>:<main mtime_ns>:<wal size>:<wal mtime_ns>:<max time_updated>``. The first
four fields are the files' own state; a database whose files sit byte-still since the stored
signature has no new row, so ``read`` skips it without opening it. A moved database reads only the
rows at or above the stored max ``time_updated``: a row the last capture has not seen sits there,
unless the clock stepped backward. A stored signature in any other format reads the whole table.
"""

import datetime as dt
import json
import pathlib
import re
import sqlite3
from collections.abc import Iterator

from src.backends import opencode
from src.infra import home
from src.runtime.hooks import usage_sources

# The row prefilter: a blob without a tokens key cannot carry token counts. ASCII-case-
# insensitive, mirroring SQLite's LIKE folding; str.lower would mismatch marks SQLite
# leaves distinct.
_TOKENS_KEY = re.compile(r'"[tT][oO][kK][eE][nN][sS]"').search

_ACCOUNT = "default"


def logs() -> Iterator[tuple[pathlib.Path, str]]:
  """The opencode database, when it exists."""
  db = home.opencode_db()
  if db.exists():
    yield db, _ACCOUNT


def _file_state(db: pathlib.Path) -> str:
  """``<main size>:<main mtime_ns>:<wal size>:<wal mtime_ns>``; an absent WAL file reads ``0:0``."""
  main = db.stat()
  try:
    wal = db.with_name(db.name + "-wal").stat()
  except FileNotFoundError:
    return f"{main.st_size}:{main.st_mtime_ns}:0:0"
  return f"{main.st_size}:{main.st_mtime_ns}:{wal.st_size}:{wal.st_mtime_ns}"


def _stored_state(previous: str | None) -> tuple[str, int] | None:
  """The (file state, max time_updated) of a stored signature, or None for any other format."""
  if previous is None:
    return None
  parts = previous.split(":")
  if len(parts) != 5 or not all(part.isdigit() for part in parts):
    return None
  return ":".join(parts[:4]), int(parts[4])


def _strict_json_constant(name: str) -> None:
  """Reject the NaN/Infinity literals json.loads admits but SQLite's json_valid rejects."""
  raise ValueError(f"invalid JSON constant: {name}")


def _record(message_id: str, session_id: str, data: str) -> usage_sources.UsageRecord | None:
  """The ledger record of one assistant row, or None when the row contributes nothing.

  The filter chain is the tokens prefilter and a strict JSON parse; the role filter is the
  query's ``type = 'assistant'``.
  """
  if _TOKENS_KEY(data) is None:
    return None
  try:
    blob = json.loads(data, parse_constant=_strict_json_constant)
  except (ValueError, RecursionError):
    return None
  if not isinstance(blob, dict):
    return None
  tokens = blob.get("tokens")
  tokens = tokens if isinstance(tokens, dict) else {}
  cache = tokens.get("cache")
  cache = cache if isinstance(cache, dict) else {}
  created = blob.get("time")
  created = created.get("created") if isinstance(created, dict) else None
  in_fresh, output = tokens.get("input"), tokens.get("output")
  if not (in_fresh or output):
    return None
  model_blob = blob.get("model")
  model_blob = model_blob if isinstance(model_blob, dict) else {}
  model = model_blob.get("id") or "unknown"
  provider = model_blob.get("providerID")
  if model.startswith("/"):
    model = f"{pathlib.Path(model).name} ({provider})"
  ts = dt.datetime.fromtimestamp(created / 1000, dt.UTC).isoformat() if isinstance(created, (int, float)) else ""
  return usage_sources.UsageRecord(
      record_id=f"opencode:{message_id}",
      kind=usage_sources.RecordKind.NATIVE,
      source=opencode.USAGE_SOURCE,
      model=model,
      account=provider or "unknown",
      ts=ts,
      in_fresh=in_fresh or 0,
      cache_write=cache.get("write") or 0,
      cache_read=cache.get("read") or 0,
      output=output or 0,
      sessions=(session_id,))


def read(path: pathlib.Path, account: str, previous: str | None) -> tuple[str, list[usage_sources.UsageRecord]]:
  """The records of the assistant rows the stored signature has not seen, and the database's signature.

  The database opens read-only, so the capture never writes to it. A row updated since the last
  capture upserts on its id and moves its ledger record; a deleted row leaves the stored one
  untouched. A read or parse failure raises.
  """
  state = _file_state(path)
  stored = _stored_state(previous)
  if stored is not None and stored[0] == state:
    return previous, []
  con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
  try:
    max_updated = con.execute("select coalesce(max(time_updated), 0) from session_message").fetchone()[0]
    floor = 0 if stored is None else stored[1]
    records = [
        record for message_id, session_id, data in con.execute(
            "select id, session_id, data from session_message where type = 'assistant' and time_updated >= ?", (floor,))
        if (record := _record(message_id, session_id, data)) is not None
    ]
  finally:
    con.close()
  return f"{state}:{max_updated}", records


def sweep(scope: usage_sources.SweepScope) -> usage_sources.SourceSweep:
  """opencode's part of the cold-storage sweep; the sweep module loads on the first call."""
  from src.backends.opencode import usage_sweep  # deferred: import server

  return usage_sweep.sweep(scope)
