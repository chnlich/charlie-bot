"""Shared helpers for chat message persistence and rendering."""

import asyncio
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import TYPE_CHECKING

from src.core import event_types as ET
from src.core.message_aggregator import MessageAggregator
from src.core.message_events import _ATTACHED_FILES_MARKER, _stable_history_projection

if TYPE_CHECKING:
  from src.core.message_projection import MessageProjection
  from src.core.models import SessionMetadata
  from src.core.sessions import SessionManager

__all__ = [
    "SessionBootstrapData",
    "SessionViewData",
    "build_agent_input_content",
    "build_agent_message_event",
    "build_scheduled_trigger_event",
    "build_session_bootstrap_data",
    "build_session_view_data",
    "build_user_event",
    "events_to_messages",
    "events_to_view",
    "get_message_projection_fast",
]


def build_agent_input_content(content: str, uploaded_files: list[dict] | None = None) -> str:
  """Append absolute attachment paths to the agent-visible message."""
  paths = [str(f.get("path", "")).strip() for f in uploaded_files or [] if str(f.get("path", "")).strip()]
  if not paths:
    return content
  return content + _ATTACHED_FILES_MARKER + "\n".join(f"- {path}" for path in paths)


def build_user_event(content: str, uploaded_files: list[dict] | None = None) -> dict:
  """Build the persisted user event payload for chat history and websocket updates."""
  event = {
      "type": ET.USER,
      "content": content,
      "timestamp": datetime.now(UTC).isoformat(),
  }
  if uploaded_files:
    event["uploaded_files"] = uploaded_files
  return event


def build_scheduled_trigger_event(content: str) -> dict:
  """Build the persisted scheduled-trigger auto-wake event.

  Parallel to ``build_user_event`` but carries the dedicated ``ET.SCHEDULED_TRIGGER``
  type and never accepts attachments or voice flags -- scheduled-trigger events
  are system self-wakes, not real user messages.
  """
  return {
      "type": ET.SCHEDULED_TRIGGER,
      "content": content,
      "timestamp": datetime.now(UTC).isoformat(),
  }


def build_agent_message_event(content: str, *, from_session: str, from_session_name: str) -> dict:
  """Build the persisted agent-relay event for a cross-session message.

  Parallel to ``build_scheduled_trigger_event`` but carries the dedicated
  ``ET.AGENT_MESSAGE`` type plus the caller session's provenance. Agent
  messages are not real user messages: the authorization gate excludes them
  by type, so they neither mint nor revoke an authorization window.
  """
  return {
      "type": ET.AGENT_MESSAGE,
      "content": content,
      "from_session": from_session,
      "from_session_name": from_session_name,
      "timestamp": datetime.now(UTC).isoformat(),
  }


@dataclass
class SessionBootstrapData:
  """Critical data needed to make one chat session usable."""
  session: 'SessionMetadata'
  messages: list[dict]
  pending_draft: dict | None = None
  total_event_count: int = 0
  oldest_message_ordinal: int = 0
  has_more: bool = False


@dataclass
class SessionViewData:
  """Data produced by the messages → usage → mark-read pipeline."""
  messages: list[dict]
  threads: list[dict]
  usage: dict | None
  pending_draft: dict | None = None
  total_event_count: int | None = None
  oldest_message_ordinal: int = 0
  has_more: bool = False


async def get_message_projection_fast(
    session_mgr: 'SessionManager',
    session_id: str,
) -> 'MessageProjection | None':
  """Return the session's message projection via the warm-hit fast path.

  A warm projection hit is a dict read + len compare answered on the event
  loop; only a miss pays the executor round-trip the threaded getter needs
  for its disk reads. Returns None when no projection is available (an
  archived session), and the caller takes its fallback path.
  """
  projection = session_mgr.projection_memo_hit(session_id)
  if projection is None:
    projection = await asyncio.to_thread(session_mgr.get_message_projection, session_id)
  return projection


async def _projection_page(
    session_mgr: 'SessionManager',
    session_id: str,
    message_limit: int,
) -> tuple[list[dict], dict | None, int, int, bool] | None:
  """Read one turn-aligned page off the session's memoized message projection.

  Returns (messages, pending_draft, event_count, oldest_ordinal, has_more), or
  None when no projection is available and the caller must take the legacy
  tail-events path.
  """
  projection = await get_message_projection_fast(session_mgr, session_id)
  if projection is None:
    return None
  messages, oldest_ordinal, has_more = projection.tail(message_limit)
  return messages, projection.pending_draft, projection.event_count, oldest_ordinal, has_more


async def _tail_events_page(
    session_mgr: 'SessionManager',
    session_id: str,
    archive_offset: int,
    message_limit: int,
) -> tuple[list[dict], dict | None, int, int, bool]:
  """Load the last *message_limit* events and view them with global ordinals.

  Returns (messages, pending_draft, total_event_count, oldest_message_ordinal,
  has_more). Indices are global (archive_offset + line-in-live-file), matching
  ``load_chat_events_range``; ``has_more`` is set when the tail window is full
  or archived events precede it.
  """
  tail_events, total_count, has_more = await asyncio.to_thread(
      session_mgr.load_chat_events_tail, session_id, message_limit)
  offset = archive_offset + total_count - len(tail_events)
  messages, pending_draft = events_to_view(tail_events, event_index_offset=offset)
  return (messages, pending_draft, archive_offset + total_count, offset, has_more or archive_offset > 0)


async def _messages_page(
    session_mgr: 'SessionManager',
    session_id: str,
    archive_offset: int,
    message_limit: int,
) -> tuple[list[dict], dict | None, int, int, bool]:
  """One bounded message page: the projection when usable, the tail path otherwise.

  Returns (messages, pending_draft, event_count, oldest_ordinal, has_more).
  The projection getter refuses an archived (``archive_offset != 0``) session,
  so that gate runs here, before the executor round-trip, and archived
  sessions always take the legacy tail-events path. A live session whose
  projection read still misses (it was archived after the caller read its
  metadata) falls back the same way.
  """
  if archive_offset == 0:
    page = await _projection_page(session_mgr, session_id, message_limit)
    if page is not None:
      return page
  return await _tail_events_page(session_mgr, session_id, archive_offset, message_limit)


async def build_session_bootstrap_data(
    session_id: str,
    session_mgr: 'SessionManager',
    *,
    message_limit: int = 40,
) -> SessionBootstrapData:
  """Load the minimal session data needed for first paint or SPA switching.

  A projection-served page is turn-aligned and holds at least *message_limit*
  messages (unless history is exhausted); the legacy tail-events path folds
  the last *message_limit* raw events, which can render fewer messages.
  """
  session_meta = await session_mgr.get_session(session_id)
  if session_meta is None:
    raise ValueError(f"session '{session_id}' metadata missing during bootstrap build")

  messages, pending_draft, total_event_count, oldest_ordinal, has_more = await _messages_page(
      session_mgr, session_id, session_meta.archive_offset, message_limit)

  return SessionBootstrapData(
      session=session_meta,
      messages=messages,
      pending_draft=pending_draft,
      total_event_count=total_event_count,
      oldest_message_ordinal=oldest_ordinal,
      has_more=has_more,
  )


async def build_session_view_data(
    session_id: str,
    session_mgr: 'SessionManager',
    thread_rows: list[dict],
    *,
    message_limit: int | None = 40,
) -> SessionViewData:
  """Build the view's messages and usage.

  *thread_rows* are the session view's thread rows (``view_thread_rows``'s
  shape), resolved by the caller so the view's row proof is shared with the
  workers-panel list. When *message_limit* is None, loads all events. When
  set, the page shape follows the shared projection-or-tail policy
  (``_messages_page``): a projection-served page is turn-aligned and holds at
  least *message_limit* messages (unless history is exhausted), while the
  tail path folds the last *message_limit* raw events, which can render
  fewer messages.

  Returns committed messages plus an optional pending_draft (the in-progress
  assistant draft that has not yet been flushed). Live render paths show the
  pending_draft in the streaming preview, not as a committed bubble; this
  keeps SSR aligned with the per-session live aggregator and avoids the
  duplicate-bubble seen on mid-stream reload.
  """
  session_meta = await session_mgr.get_session(session_id)
  if session_meta is None:
    raise ValueError(f"session '{session_id}' metadata missing during view build")

  if message_limit is None:
    raw_events = await asyncio.to_thread(session_mgr.load_chat_events_sync, session_id)
    total_event_count = session_meta.archive_offset + len(raw_events)
    oldest_message_ordinal = session_meta.archive_offset
    has_more = session_meta.archive_offset > 0
    messages, pending_draft = events_to_view(raw_events, event_index_offset=session_meta.archive_offset)
  else:
    messages, pending_draft, total_event_count, oldest_message_ordinal, has_more = await _messages_page(
        session_mgr, session_id, session_meta.archive_offset, message_limit)

  usage = await session_mgr.resolve_session_usage(session_id, session_meta)

  return SessionViewData(
      messages=messages,
      threads=thread_rows,
      usage=usage,
      pending_draft=pending_draft,
      total_event_count=total_event_count,
      oldest_message_ordinal=oldest_message_ordinal,
      has_more=has_more,
  )


def _committed_messages(agg: MessageAggregator, events: list[dict]) -> list[dict]:
  """Feed events through the aggregator and collect the committed message deltas."""
  return [
      delta["message"] for delta in agg.feed_indexed(_stable_history_projection(events)) if delta["type"] == "message"
  ]


def events_to_messages(events: list[dict], event_index_offset: int = 0) -> list[dict]:
  """Convert raw chat_events.jsonl entries into a flat list of displayable messages.

  Final-flushes any in-progress assistant draft; suitable for stable history
  (paginated older events). For the live-render entrypoint, see ``events_to_view``.
  """
  agg = MessageAggregator(event_index_offset=event_index_offset, emit_stream_deltas=False)
  messages = _committed_messages(agg, events)
  for delta in agg.flush_pending():
    messages.append(delta["message"])
  return messages


def events_to_view(events: list[dict], event_index_offset: int = 0) -> tuple[list[dict], dict | None]:
  """Aggregate events without final-flush; return (committed_messages, pending_draft).

  Used for the initial render of a live session: committed messages render in
  chat history, pending_draft (if any) renders into the streaming preview.
  This matches the per-session live aggregator's state so subsequent live
  events extend the draft rather than producing a duplicate bubble.
  """
  agg = MessageAggregator(event_index_offset=event_index_offset, emit_stream_deltas=False)
  return _committed_messages(agg, events), agg.pending_draft_message()
