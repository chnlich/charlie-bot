"""Sidebar status snapshot and dirty registry — process memory only.

Single home of the derived sidebar-state cache behind ``/api/sessions/status``:
every writer of the probed state (session metadata, thread metadata, trigger
files, plans.json, thinking busy state) calls :func:`mark_sidebar_dirty` at the
transition, the poll re-probes only dirty sessions, and clean sessions are
served from the snapshot with zero disk access. Disk stays the single source of
truth — the snapshot is a rebuildable derived value and starts cold at boot;
nothing here is ever persisted.

Follows the ``thinking_state`` precedent: module-level set/dict + functions, no
constructor wiring; every accessor is a synchronous set/dict operation (no I/O,
no await), so transition-point calls are cheap and there is no check-then-act
window around a mark.
"""

from collections.abc import Iterable

# Every Nth populate_sidebar_state call re-probes all active sessions: the
# bounded self-heal window for a state-transition write path that forgets to
# mark its session dirty (active polls ~3s -> stale field heals within ~30s).
_FORCE_FULL_EVERY = 10

# Session ids whose probed state changed since the last re-probe.
_dirty: set[str] = set()
# session id -> {"thread_running": bool, "pending_trigger_count": int,
# "next_trigger_at": datetime | None, "has_pending_plan_approval": bool}.
# A session without an entry is cold for that poll (probed like a dirty one),
# so an empty dict — a fresh boot — is a full probe.
_snapshot: dict[str, dict] = {}
# populate_sidebar_state invocation counter (process lifetime).
_poll_count = 0


def mark_sidebar_dirty(session_id: str) -> None:
  """Flag *session_id*'s probed sidebar state for re-probe on the next poll."""
  _dirty.add(session_id)


def is_dirty(session_id: str) -> bool:
  """True if *session_id* is flagged for re-probe."""
  return session_id in _dirty


def discard_dirty(session_ids: Iterable[str]) -> None:
  """Drop *session_ids* from the dirty set (no-op for unknown ids).

  The poll calls this with exactly the ids it selected for re-probe, at
  selection time: a transition mark landing while the probe runs re-adds the
  id, so that write is picked up by the next poll even if the in-flight probe
  raced it.
  """
  for session_id in session_ids:
    _dirty.discard(session_id)


def snapshot_entry(session_id: str) -> dict | None:
  """The probed sidebar-state entry for *session_id*, or None when cold for it."""
  return _snapshot.get(session_id)


def required_snapshot_entry(session_id: str) -> dict:
  """The probed sidebar-state entry for *session_id*.

  Indexes directly — a poll has either just probed the session or found a prior
  entry, so a KeyError here means the poll's probe-or-serve invariant broke and
  must fail loud, not serve a fabricated default.
  """
  return _snapshot[session_id]


def store_snapshot_entry(session_id: str, entry: dict) -> None:
  """Refresh the snapshot entry for *session_id* with fresh probe results."""
  _snapshot[session_id] = entry


def register_poll(force: bool) -> bool:
  """Count one populate_sidebar_state invocation; return True when it must re-probe everything.

  Every ``_FORCE_FULL_EVERY``-th call, and any call with *force* set (the
  ``/status?force=1`` escape hatch), is a full re-probe of all active sessions
  in that call.
  """
  global _poll_count
  _poll_count += 1
  return force or _poll_count % _FORCE_FULL_EVERY == 0


def reset_for_tests() -> None:
  """Clear the dirty set, the snapshot, and the poll counter (tests only)."""
  global _poll_count
  _dirty.clear()
  _snapshot.clear()
  _poll_count = 0
