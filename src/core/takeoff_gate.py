"""Takeoff authorization gate for delegation requests.

A delegation (worker spawn) may proceed only when the session's chat history
carries either an explicit "take off" in the latest real user message or a
"pre take off" issued within the last 12 hours. This module owns the full
gate — phrase matching, timestamp parsing, and the blocking exception —
extracted from src.core.spawner, which consumes none of it.
"""

from datetime import UTC, datetime, timedelta

import structlog

from src.core import event_types as ET
from src.core.sessions import SessionManager

log = structlog.get_logger()

_PRE_TAKEOFF_PHRASE = "pre take off"
_TAKEOFF_PHRASE = "take off"
_PRE_TAKEOFF_WINDOW = timedelta(hours=12)


class DelegationBlockedError(Exception):
  """Raised when the takeoff gate rejects a delegation attempt."""


def _is_real_user_message(event: dict) -> bool:
  """Real user message = ET.USER with string content (excludes trigger events and nested tool_result blocks)."""
  if event.get("type") != ET.USER:
    return False
  return isinstance(event.get("content"), str)


def _normalize_takeoff_content(content: str) -> str:
  """Normalize case and consecutive whitespace for authorization phrase matching."""
  return " ".join(content.casefold().split())


def _parse_pre_takeoff_timestamp(event: dict, session_id: str) -> datetime | None:
  """Parse a pre-takeoff event timestamp as UTC, failing closed when it is invalid."""
  timestamp = event.get("timestamp")
  if not isinstance(timestamp, str) or not timestamp:
    log.warning(
        "pre_takeoff_timestamp_missing",
        session=session_id,
        event_id=event.get("id"),
    )
    return None
  try:
    issued_at = datetime.fromisoformat(timestamp)
  except ValueError:
    log.warning(
        "pre_takeoff_timestamp_unparseable",
        session=session_id,
        event_id=event.get("id"),
        timestamp=timestamp,
    )
    return None
  if issued_at.tzinfo is None:
    log.warning(
        "pre_takeoff_timestamp_timezone_missing",
        session=session_id,
        event_id=event.get("id"),
        timestamp=timestamp,
    )
    return None
  return issued_at.astimezone(UTC)


def check_takeoff_gate(
    session_id: str,
    session_mgr: SessionManager,
    now: datetime | None = None,
) -> None:
  """Verify an active pre-takeoff or ordinary takeoff authorization window."""
  effective_now = now if now is not None else datetime.now(UTC)
  if effective_now.tzinfo is None:
    raise ValueError("authorization check time must be timezone-aware")
  effective_now = effective_now.astimezone(UTC)

  events = session_mgr.load_chat_events_sync(session_id)
  # Backward scan. The authorization verdict reads only two answers: the takeoff
  # phrase in the file-last real user message, and the file-last parseable
  # pre-takeoff stamp. A forward walk overwrites both with every later message
  # of their kind, so once the backward scan has seen the file-last of a kind,
  # no file-older message can change either answer — it stops there. The
  # delegation target is the busiest master session, so the tail after its last
  # user message is one turn's length while the file grows without bound.
  latest_user_has_takeoff = False
  latest_pre_takeoff_at: datetime | None = None
  seen_latest_user = False
  for event in reversed(events):
    if not _is_real_user_message(event):
      continue
    normalized = _normalize_takeoff_content(event.get("content"))
    if not seen_latest_user:
      seen_latest_user = True
      latest_user_has_takeoff = _TAKEOFF_PHRASE in normalized
    if latest_pre_takeoff_at is None and _PRE_TAKEOFF_PHRASE in normalized:
      issued_at = _parse_pre_takeoff_timestamp(event, session_id)
      if issued_at is not None:
        latest_pre_takeoff_at = issued_at
    if seen_latest_user and (latest_user_has_takeoff or latest_pre_takeoff_at is not None):
      break

  pre_takeoff_active = (
      latest_pre_takeoff_at is not None and
      latest_pre_takeoff_at <= effective_now < latest_pre_takeoff_at + _PRE_TAKEOFF_WINDOW)
  if latest_user_has_takeoff or pre_takeoff_active:
    return

  raise DelegationBlockedError(
      'Delegation blocked: no active authorization. A valid "pre take off" within 12 hours or '
      '"take off" in the latest real user message is required before delegating.')
