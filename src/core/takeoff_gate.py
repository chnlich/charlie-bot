"""Takeoff authorization gate for delegation requests.

A delegation (worker spawn) may proceed only when the session's chat history
carries either an explicit "take off" in the latest real user message or a
"pre take off" issued within the last 12 hours. This module owns the full
gate — phrase matching, timestamp parsing, and the blocking exception —
extracted from src.core.spawner, which consumes none of it.

The verdict reads two answers out of the chat history: the takeoff phrase in
the file-last real user message, and the file-last parseable pre-takeoff
stamp. Both are complete prefix facts of the event list, so the answers memo
carries them across calls and an appended suffix folds by scanning only the
suffix: a user message in the suffix is the new file-last one, and the
prefix's stored stamp answer is the file-older bound the backward
continuation would stop at. A wholesale list replacement (a new object)
rebuilds from a fresh walk, the same identity contract the usage fold's memo
rides.
"""

from datetime import UTC, datetime, timedelta

import structlog

from src.core import event_types as ET
from src.core.memo import BoundedMemo
from src.core.sessions import SessionManager

log = structlog.get_logger()

_PRE_TAKEOFF_PHRASE = "pre take off"
_TAKEOFF_PHRASE = "take off"
_PRE_TAKEOFF_WINDOW = timedelta(hours=12)

# session_id -> (events list, covered length, latest_user_has_takeoff,
# latest_pre_takeoff_at). Pinning the list keeps id() stable, so an identity
# match can never be an id-reuse collision with a different list; the
# chat-events cache mutates the list only by in-place append (save_chat_event)
# or wholesale replacement, and a replacement is a new object. The answers are
# a pure function of the list content, so a list object shared by two
# managers serves one entry safely.
_gate_answers_memo: BoundedMemo[str, tuple[list[dict], int, bool, datetime | None]] = BoundedMemo(64)


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


def _backward_user_answers(
    events: list[dict],
    session_id: str,
) -> tuple[bool, datetime | None, bool]:
  """Backward walk over the given span: the span-last real user message's
  takeoff phrase, the span-last parseable pre-takeoff stamp, and whether the
  span held a real user message at all.

  No early break. The stored answers must be complete prefix facts for the
  suffix fold to combine with, and the break the scan replaced could fire
  with the stamp answer still unset behind a takeoff-allowed verdict — the
  one under-fill this walk removes. Past a settled stamp the break changed
  nothing (both answers settle, and a file-older message cannot overwrite
  either), so walking on only fills that corner and never flips a verdict.
  """
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
  return latest_user_has_takeoff, latest_pre_takeoff_at, seen_latest_user


def _settled_user_answers(
    events: list[dict],
    session_id: str,
) -> tuple[bool, datetime | None]:
  """Return the two answers over ``events``, serving the answers memo.

  A cold or replaced list pays one full backward walk and stores the answers
  with the list and its length. An identity match folds only the appended
  suffix — the chat-events cache grows the list in place, so the answers as
  of the covered length stay valid and the suffix holds the file-last user
  message when it holds one at all; the store claims exactly the scanned
  span, so an append landing between the slice and the store is scanned by
  the next call instead of being claimed unseen.
  """
  cached = _gate_answers_memo.get(session_id)
  if cached is not None and cached[0] is events:
    covered, has_takeoff, pre_takeoff_at = cached[1], cached[2], cached[3]
    suffix = events[covered:]
    if suffix:
      suffix_has_takeoff, suffix_pre_takeoff_at, seen_user = _backward_user_answers(suffix, session_id)
      if seen_user:
        has_takeoff = suffix_has_takeoff
      if suffix_pre_takeoff_at is not None:
        pre_takeoff_at = suffix_pre_takeoff_at
      _gate_answers_memo.store(session_id, (events, covered + len(suffix), has_takeoff, pre_takeoff_at))
    return has_takeoff, pre_takeoff_at
  count = len(events)
  span = events[:count]  # the walked span is the claimed span: an append landing mid-walk is not claimed unseen
  has_takeoff, pre_takeoff_at, _ = _backward_user_answers(span, session_id)
  _gate_answers_memo.store(session_id, (events, count, has_takeoff, pre_takeoff_at))
  return has_takeoff, pre_takeoff_at


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
  latest_user_has_takeoff, latest_pre_takeoff_at = _settled_user_answers(events, session_id)

  pre_takeoff_active = (
      latest_pre_takeoff_at is not None and
      latest_pre_takeoff_at <= effective_now < latest_pre_takeoff_at + _PRE_TAKEOFF_WINDOW)
  if latest_user_has_takeoff or pre_takeoff_active:
    return

  raise DelegationBlockedError(
      'Delegation blocked: no active authorization. A valid "pre take off" within 12 hours or '
      '"take off" in the latest real user message is required before delegating.')
