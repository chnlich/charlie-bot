"""The one session/thread alias record: ``sessions/session_aliases.json``.

Old entry points (legacy thread ids, pre-tree session ids) resolve to the
canonical task and Run through this one file; new runs register their
compatibility thread alias here at creation, so the legacy thread transport
resolves to the same Run without a second ThreadMetadata ever being written.
The run owner writes the new-run rows. Writers hold the tree control write
lock; the file's shape (``old_session_ids``, ``old_threads``) is unchanged.
"""

import json
import os
from pathlib import Path

from src.core.json_utils import atomic_write_text, load_json_meta
from src.core.log_once import LazyStructlogLogger
from src.core.memo import StatSignatureMemo

log = LazyStructlogLogger()

ALIASES_FILE_NAME = "session_aliases.json"

# The store reads one file, so the memo holds one entry; the key only labels it.
_ALIAS_MEMO_LIMIT = 1
_ALIAS_MEMO_KEY = "session_aliases"


def alias_thread_key(owner_session_id: str, thread_id: str) -> str:
  """The ``old_threads`` key for one owner-session/thread-id pair."""
  return f"{owner_session_id}/{thread_id}"


class SessionAliasStore:
  """Read/resolve/register access to the sessions-root alias file.

  Reads ride a stat-signature memo: a resolve between writes re-parses
  nothing, and a write's atomic replace moves the signature so the next read
  re-parses. Served values are shared and read-only; the only mutating
  caller (:meth:`_put`) writes from its own copy.
  """

  def __init__(self, sessions_dir: Path) -> None:
    self.path = sessions_dir / ALIASES_FILE_NAME
    self._memo: StatSignatureMemo[str, dict] = StatSignatureMemo(_ALIAS_MEMO_LIMIT)

  def _read(self) -> dict:
    try:
      st = os.stat(self.path)
    except OSError:
      self._memo.drop(_ALIAS_MEMO_KEY)
      return {"old_session_ids": {}, "old_threads": {}}
    cached = self._memo.fresh(_ALIAS_MEMO_KEY, st)
    if cached is not None:
      return cached
    raw = load_json_meta(self.path, "session_aliases_unreadable")
    if raw is None:
      # Missing (unlinked between the stat and the read) or malformed: the
      # per-call empty answer the resolvers have always seen, never memoized.
      return {"old_session_ids": {}, "old_threads": {}}
    old_session_ids = raw.get("old_session_ids")
    old_threads = raw.get("old_threads")
    value = {
        "old_session_ids": old_session_ids if isinstance(old_session_ids, dict) else {},
        "old_threads": old_threads if isinstance(old_threads, dict) else {},
    }
    self._memo.record(_ALIAS_MEMO_KEY, st, value)
    return value

  def resolve_session(self, old_session_id: str) -> str | None:
    """The canonical session id an imported old id maps to, or None."""
    return self._read()["old_session_ids"].get(old_session_id)

  def resolve_thread(self, owner_session_id: str, thread_id: str) -> dict | None:
    """The ``{"session_id", "run_id"}`` target one legacy thread entry resolves to.

    Both the raw owner id and its canonical mapping (when the caller addressed
    an imported old session id) are tried; None when neither names a row.
    """
    threads = self._read()["old_threads"]
    direct = threads.get(alias_thread_key(owner_session_id, thread_id))
    if isinstance(direct, dict):
      return direct
    canonical = self.resolve_session(owner_session_id)
    if canonical is not None and canonical != owner_session_id:
      mapped = threads.get(alias_thread_key(canonical, thread_id))
      if isinstance(mapped, dict):
        return mapped
    return None

  def old_ids_for(self, canonical_session_id: str) -> list[str]:
    """Every imported old id that resolves to *canonical_session_id*."""
    return sorted(
        old for old, canonical in self._read()["old_session_ids"].items() if canonical == canonical_session_id)

  def register_run_thread(self, session_id: str, run_id: str) -> None:
    """Register the new-run compatibility thread alias (idempotent, atomic rewrite)."""
    self._put(alias_thread_key(session_id, run_id), {"session_id": session_id, "run_id": run_id})

  def register_owner_thread_alias(self, owner_session_id: str, session_id: str, run_id: str) -> None:
    """Alias a compat thread entry addressed from *owner_session_id* to the run's REAL owner.

    The delegate path's parent-addressed entry: the delegating session is the
    address, the child task is where the Run lives. Recording the owner as the
    target session would resolve to a run that does not exist there.
    """
    self._put(alias_thread_key(owner_session_id, run_id), {"session_id": session_id, "run_id": run_id})

  def _put(self, key: str, target: dict) -> None:
    raw = self._read()
    # The read's value is the memo's shared entry: the new row lands in a copy,
    # never in the object concurrent resolvers are reading.
    payload = {
        "old_session_ids": dict(raw["old_session_ids"]),
        "old_threads": dict(raw["old_threads"]),
    }
    payload["old_threads"][key] = target
    self._write(payload)

  def _write(self, payload: dict) -> None:
    self.path.parent.mkdir(parents=True, exist_ok=True)
    atomic_write_text(self.path, json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True))
