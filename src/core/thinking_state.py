"""Single source of truth for master thinking state.

``busy_since(session_id)`` is non-None if and only if that session has a work
item running or queued. The writer is master_cc (mark at enqueue, clear at
consumer teardown); readers are sessions / scheduled_sessions / api. All
accessors are synchronous dict operations — no I/O, no await — so stamping can
happen on every metadata return path (including per-session list reads) and
from synchronous contexts, and there is no check-then-act window between
setting and clearing.
"""

from datetime import datetime, timezone
from typing import Optional

import structlog

log = structlog.get_logger()

# session_id -> busy interval start (a continuous run+queued stretch).
_busy_since: dict[str, datetime] = {}


def mark_busy(session_id: str, since: Optional[datetime] = None) -> tuple[datetime, bool]:
  """Record the busy interval start for *session_id*.

  setdefault semantics: an already-busy session keeps its existing interval
  start. Returns (interval_start, created) — *created* is True only when this
  call opened a new interval, which is what callers use to decide whether a
  busy notification is needed.

  *since*, when an aware datetime, becomes the interval start instead of
  ``datetime.now(timezone.utc)``. Its only supplier is a re-attached turn's
  persisted ``master_run.started_at`` (startup reconcile); ``None`` keeps the
  default now() start for every freshly-queued turn.
  """
  existing = _busy_since.get(session_id)
  if existing is not None:
    return existing, False
  started_at = since if since is not None else datetime.now(timezone.utc)
  _busy_since[session_id] = started_at
  log.debug("thinking_state_busy", session=session_id, busy_since=started_at.isoformat())
  return started_at, True


def clear_busy(session_id: str) -> None:
  """Drop the busy entry for *session_id*. Idempotent."""
  started_at = _busy_since.pop(session_id, None)
  if started_at is not None:
    log.debug("thinking_state_idle", session=session_id, busy_since=started_at.isoformat())


def busy_since(session_id: str) -> Optional[datetime]:
  """Current busy interval start for *session_id*, or None."""
  return _busy_since.get(session_id)
