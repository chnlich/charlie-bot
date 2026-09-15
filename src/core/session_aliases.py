"""The one session/thread alias record: ``sessions/session_aliases.json``.

Old entry points (legacy thread ids, pre-tree session ids) resolve to the
canonical task and Run through this one file; new runs register their
compatibility thread alias here at creation, so the legacy thread transport
resolves to the same Run without a second ThreadMetadata ever being written.
The migration stage (``session_tree_migration.py``) fills ``old_session_ids``
and the historical ``old_threads`` rows; the run owner writes only new-run
rows. Reads merge both sources; writers hold the tree control write lock.
"""

import json
from pathlib import Path

from src.core.json_utils import atomic_write_text, load_json_meta
from src.core.log_once import LazyStructlogLogger

log = LazyStructlogLogger()

ALIASES_FILE_NAME = "session_aliases.json"


def alias_thread_key(owner_session_id: str, thread_id: str) -> str:
  """The ``old_threads`` key for one owner-session/thread-id pair."""
  return f"{owner_session_id}/{thread_id}"


class SessionAliasStore:
  """Read/resolve/register access to the sessions-root alias file."""

  def __init__(self, sessions_dir: Path) -> None:
    self.path = sessions_dir / ALIASES_FILE_NAME

  def _read(self) -> dict:
    empty = {"old_session_ids": {}, "old_threads": {}}
    raw = load_json_meta(self.path, "session_aliases_unreadable")
    if raw is None:
      return empty
    old_session_ids = raw.get("old_session_ids")
    old_threads = raw.get("old_threads")
    return {
        "old_session_ids": old_session_ids if isinstance(old_session_ids, dict) else {},
        "old_threads": old_threads if isinstance(old_threads, dict) else {},
    }

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
    return sorted(old for old, canonical in self._read()["old_session_ids"].items() if canonical == canonical_session_id)

  def register_run_thread(self, session_id: str, run_id: str) -> None:
    """Register the new-run compatibility thread alias (idempotent, atomic rewrite)."""
    self._put(alias_thread_key(session_id, run_id), {"session_id": session_id, "run_id": run_id})

  def register_owner_thread_alias(self, owner_session_id: str, session_id: str, run_id: str) -> None:
    """Alias a compat thread entry addressed from *owner_session_id* to the run's REAL owner.

    The delegate path's parent-addressed entry: the delegating session is the
    address, the child task is where the Run lives. Recording the owner as the
    target session would resolve to a run that does not exist there.
    """
    self._put(
        alias_thread_key(owner_session_id, run_id), {"session_id": session_id, "run_id": run_id})

  def put_old_session(self, old_session_id: str, canonical_session_id: str) -> None:
    """Register one imported old-session mapping (migration-stage entry point)."""
    sessions = self._read()["old_session_ids"]
    sessions[old_session_id] = canonical_session_id
    self._write({"old_session_ids": sessions, "old_threads": self._read()["old_threads"]})

  def _put(self, key: str, target: dict) -> None:
    raw = self._read()
    raw["old_threads"][key] = target
    self._write(raw)

  def _write(self, payload: dict) -> None:
    self.path.parent.mkdir(parents=True, exist_ok=True)
    atomic_write_text(self.path, json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True))
