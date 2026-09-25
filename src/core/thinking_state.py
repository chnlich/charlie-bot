"""Single source of truth for master thinking state.

``busy_since(session_id)`` is non-None if and only if that session has a work
item running or queued. The writer is master_cc (mark at enqueue, clear at
consumer teardown); readers are sessions / scheduled_sessions / api. All
accessors are synchronous dict operations — no I/O, no await — so stamping can
happen on every metadata return path (including per-session list reads) and
from synchronous contexts, and there is no check-then-act window between
setting and clearing.
"""

from datetime import UTC, datetime

from src.core.log_once import LazyStructlogLogger
from src.core.sidebar_state import mark_sidebar_dirty

log = LazyStructlogLogger()

# session_id -> busy interval start (a continuous run+queued stretch).
_busy_since: dict[str, datetime] = {}


def mark_busy(session_id: str, since: datetime | None = None) -> tuple[datetime, bool]:
  """Record the busy interval start for *session_id*.

  setdefault semantics: an already-busy session keeps its existing interval
  start. Returns (interval_start, created) — *created* is True only when this
  call opened a new interval, which is what callers use to decide whether a
  busy notification is needed.

  *since*, when an aware datetime, becomes the interval start instead of
  ``datetime.now(UTC)``. Its only supplier is a re-attached turn's
  persisted ``master_run.started_at`` (startup reconcile); ``None`` keeps the
  default now() start for every freshly-queued turn.
  """
  existing = _busy_since.get(session_id)
  if existing is not None:
    return existing, False
  started_at = since if since is not None else datetime.now(UTC)
  _busy_since[session_id] = started_at
  # busy flips the sidebar's has_running_tasks (bool(thinking_since) or running).
  mark_sidebar_dirty(session_id)
  log.debug("thinking_state_busy", session=session_id, busy_since=started_at.isoformat())
  return started_at, True


def clear_busy(session_id: str) -> None:
  """Drop the busy entry for *session_id*. Idempotent."""
  started_at = _busy_since.pop(session_id, None)
  if started_at is not None:
    # The busy -> idle flip changes the sidebar's has_running_tasks.
    mark_sidebar_dirty(session_id)
    log.debug("thinking_state_idle", session=session_id, busy_since=started_at.isoformat())


def busy_since(session_id: str) -> datetime | None:
  """Current busy interval start for *session_id*, or None."""
  return _busy_since.get(session_id)


# ---------------------------------------------------------------------------
# Task-tree worker Runs (a worker node's thinking_since)
# ---------------------------------------------------------------------------
# A worker node has no master queue: its busy interval is its live Run's,
# opened at the Run's recorded started_at (launch or re-attach) and closed at
# that Run's terminal fact. The node's has_running_tasks and work_state stay
# the task-tree activity derivation's (src.core.task_sessions); this map only
# records which Run opened the node's busy interval, so a finish closes exactly
# that interval and a Run that opened none (a manager_turn, a queued Run that
# never launched) closes nothing. Same process-memory shape as _busy_since:
# written at the transition, read with zero I/O, never persisted.
#
# worker session_id -> the id of the Run whose busy interval it holds.
_run_busy: dict[str, str] = {}
# session_id -> the newest launched Run's backend id (display only). The
# persisted metadata.backend is never rewritten by this map; readers fall back
# to it.
_run_backends: dict[str, str] = {}


def mark_run_busy(session_id: str, run_id: str, *, since: datetime | None) -> None:
  """Open a worker node's busy interval at its Run's recorded start."""
  _run_busy[session_id] = run_id
  mark_busy(session_id, since=since)


def clear_run_busy(session_id: str, run_id: str) -> None:
  """Close the busy interval *run_id* opened; any other Run's finish is a no-op."""
  if _run_busy.get(session_id) != run_id:
    return
  del _run_busy[session_id]
  clear_busy(session_id)


def note_run_backend(session_id: str, backend: str | None) -> None:
  """Record the newest Run's backend id for *session_id* (display only)."""
  if backend:
    _run_backends[session_id] = backend


def run_backend(session_id: str) -> str | None:
  """The newest Run's backend id for *session_id*, or None when no Run is known."""
  return _run_backends.get(session_id)


def reset_run_state_for_tests() -> None:
  """Clear the worker Run busy map and the display-backend map (tests only)."""
  _run_busy.clear()
  _run_backends.clear()
