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
    issued_at = datetime.fromisoformat(timestamp.replace("Z", "+00:00"))
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
  latest_user_has_takeoff = False
  latest_pre_takeoff_at: datetime | None = None
  for event in events:
    if not _is_real_user_message(event):
      continue
    content = event.get("content")
    normalized = _normalize_takeoff_content(content)
    latest_user_has_takeoff = _TAKEOFF_PHRASE in normalized
    if _PRE_TAKEOFF_PHRASE in normalized:
      issued_at = _parse_pre_takeoff_timestamp(event, session_id)
      if issued_at is not None:
        latest_pre_takeoff_at = issued_at

  pre_takeoff_active = (
      latest_pre_takeoff_at is not None and
      latest_pre_takeoff_at <= effective_now < latest_pre_takeoff_at + _PRE_TAKEOFF_WINDOW)
  if latest_user_has_takeoff or pre_takeoff_active:
    return

  raise DelegationBlockedError(
      'Delegation blocked: no active authorization. A valid "pre take off" within 12 hours or '
      '"take off" in the latest real user message is required before delegating.')
