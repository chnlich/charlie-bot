"""Shared helpers for chat message persistence and rendering."""

import asyncio
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import TYPE_CHECKING

import structlog

from src.core import event_types as ET
from src.core.message_aggregator import (
  MessageAggregator,
  extract_text_from_message,
  extract_tool_result_text,
)

if TYPE_CHECKING:
  from src.core.models import SessionMetadata, ThreadMetadata
  from src.core.sessions import SessionManager
  from src.core.threads import ThreadManager

log = structlog.get_logger()

_ATTACHED_FILES_MARKER = "\n\n[Attached files]\n"

# Re-export so existing call sites (src.core.spawner, src.core.improve_command,
# tests) keep importing extract_text_from_message from this module.
__all__ = [
    "extract_text_from_message",
    "extract_tool_result_text",
    "serialize_uploaded_files",
    "build_agent_input_content",
    "build_user_event",
    "build_scheduled_trigger_event",
    "build_agent_message_event",
    "strip_attached_files_block",
    "normalize_user_message_event",
    "SessionBootstrapData",
    "SessionViewData",
    "build_session_bootstrap_data",
    "build_session_view_data",
    "events_to_messages",
    "events_to_view",
]


def serialize_uploaded_files(uploaded_files: list[object] | None) -> list[dict]:
  """Convert uploaded-file models or dicts into JSON-serializable dicts."""
  serialized: list[dict] = []
  for uploaded_file in uploaded_files or []:
    if hasattr(uploaded_file, "model_dump"):
      serialized.append(uploaded_file.model_dump(mode="json", exclude_none=True))
    elif isinstance(uploaded_file, dict):
      serialized.append(uploaded_file)
    elif isinstance(uploaded_file, str):
      filename = uploaded_file.replace("\\", "/").rstrip("/").split("/")[-1] or uploaded_file
      serialized.append({"filename": filename, "path": uploaded_file})
    else:
      raise TypeError(f"Unsupported uploaded file payload: {type(uploaded_file)!r}")
  return serialized


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


def strip_attached_files_block(content: str) -> tuple[str, list[dict]]:
  """Split a legacy attachment footer from user-visible content."""
  if _ATTACHED_FILES_MARKER not in content:
    return content, []

  body, marker, attachments_block = content.rpartition(_ATTACHED_FILES_MARKER)
  if not marker:
    return content, []

  uploaded_files: list[dict] = []
  for line in attachments_block.splitlines():
    if not line.startswith("- "):
      return content, []
    path = line[2:].strip()
    if not path:
      return content, []
    filename = path.replace("\\", "/").rstrip("/").split("/")[-1] or path
    uploaded_files.append({"filename": filename, "path": path})

  return body, uploaded_files


def normalize_user_message_event(ev: dict) -> dict:
  """Return display content + structured uploads for a raw user event."""
  content = ev.get("content", "")
  uploaded_files = serialize_uploaded_files(ev.get("uploaded_files"))
  if uploaded_files:
    return {"content": content, "uploaded_files": uploaded_files}

  stripped_content, legacy_files = strip_attached_files_block(content)
  return {"content": stripped_content, "uploaded_files": legacy_files}


def _stable_history_projection(events: list[dict]) -> list[tuple[int, dict]]:
  """Move queued users behind completed OpenCode runs without changing source events."""
  complete_intervals: list[tuple[int, int]] = []
  interval_start: int | None = None
  for idx, event in enumerate(events):
    if event.get("type") is None and event.get("session_id"):
      interval_start = idx
    elif event.get("type") == ET.MASTER_DONE and interval_start is not None:
      complete_intervals.append((interval_start, idx))
      interval_start = None

  deferred_indices: set[int] = set()
  deferred_by_end: dict[int, list[tuple[int, dict]]] = {}
  for start, end in complete_intervals:
    deferred: list[tuple[int, dict]] = []
    for idx in range(start + 1, end):
      event = events[idx]
      if event.get("type") != ET.USER:
        continue
      if "message" in event and "content" not in event:
        continue
      if normalize_user_message_event(event)["content"].startswith("/"):
        continue
      deferred.append((idx, event))
    if deferred:
      deferred_by_end[end] = deferred
      deferred_indices.update(idx for idx, _ in deferred)

  projected: list[tuple[int, dict]] = []
  for idx, event in enumerate(events):
    if idx not in deferred_indices:
      projected.append((idx, event))
    projected.extend(deferred_by_end.get(idx, []))
  return projected


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
  """Data produced by the load-events → messages → usage → mark-read pipeline."""
  raw_events: list[dict]
  messages: list[dict]
  threads: list['ThreadMetadata']
  usage: dict | None
  pending_draft: dict | None = None
  total_event_count: int | None = None
  oldest_message_ordinal: int = 0
  has_more: bool = False


def _projection_page(
    session_mgr: 'SessionManager',
    session_id: str,
    message_limit: int,
) -> tuple[list[dict], dict | None, int, int, bool] | None:
  """Read one turn-aligned page off the session's memoized message projection.

  Returns (messages, pending_draft, event_count, oldest_ordinal, has_more), or
  None when no projection is available and the caller must take the legacy
  tail-events path.
  """
  projection = session_mgr.get_message_projection(session_id)
  if projection is None:
    return None
  messages, oldest_ordinal, has_more = projection.tail(message_limit)
  return messages, projection.pending_draft, projection.event_count, oldest_ordinal, has_more


async def _mark_read_best_effort(session_mgr: 'SessionManager', session_id: str) -> 'SessionMetadata | None':
  """mark_read whose failure degrades to a logged warning.

  These readers must keep serving on a bookkeeping-write failure, so the
  exception is swallowed here; callers treat ``None`` as "metadata unchanged".
  """
  try:
    return await session_mgr.mark_read(session_id)
  except Exception:
    log.warning("mark_read_failed", session_id=session_id, exc_info=True)
    return None


async def build_session_bootstrap_data(
    session_id: str,
    session_mgr: 'SessionManager',
    *,
    message_limit: int = 40,
) -> SessionBootstrapData:
  """Load the minimal session data needed for first paint or SPA switching.

  When a message projection is available (``archive_offset == 0``), first
  paint is served from ``projection.tail(message_limit)`` — a turn-aligned
  page of at least ``message_limit`` messages (unless history is exhausted)
  with O(page) cost and zero file reads. Otherwise the legacy
  tail-events path is used.
  """
  session_meta = await session_mgr.get_session(session_id)
  if session_meta is None:
    raise ValueError(f"session '{session_id}' metadata missing during bootstrap build")

  if session_meta.archive_offset == 0:
    result = await asyncio.to_thread(_projection_page, session_mgr, session_id, message_limit)
    if result is not None:
      messages, pending_draft, total_event_count, oldest_ordinal, has_more = result
      read_meta = await _mark_read_best_effort(session_mgr, session_id)
      if read_meta is not None:
        session_meta = read_meta
      return SessionBootstrapData(
          session=session_meta,
          messages=messages,
          pending_draft=pending_draft,
          total_event_count=total_event_count,
          oldest_message_ordinal=oldest_ordinal,
          has_more=has_more,
      )

  tail_events, total_count, has_more = await asyncio.to_thread(
      session_mgr.load_chat_events_tail, session_id, message_limit)
  offset = session_meta.archive_offset + total_count - len(tail_events)
  messages, pending_draft = events_to_view(tail_events, event_index_offset=offset)

  read_meta = await _mark_read_best_effort(session_mgr, session_id)
  if read_meta is not None:
    session_meta = read_meta

  return SessionBootstrapData(
      session=session_meta,
      messages=messages,
      pending_draft=pending_draft,
      total_event_count=session_meta.archive_offset + total_count,
      oldest_message_ordinal=offset,
      has_more=has_more or session_meta.archive_offset > 0,
  )


async def build_session_view_data(
    session_id: str,
    session_mgr: 'SessionManager',
    thread_mgr: 'ThreadManager',
    *,
    message_limit: int | None = 40,
) -> SessionViewData:
  """Load events + threads in parallel, derive messages and usage, and mark read.

  When *message_limit* is None, loads all events. When set, loads the last
  *message_limit* messages — served from the message projection when
  ``archive_offset == 0`` (turn-aligned page of at least *message_limit*
  messages, O(page) cost), or from the legacy tail-events path otherwise.

  Returns committed messages plus an optional pending_draft (the in-progress
  assistant draft that has not yet been flushed). Live render paths show the
  pending_draft in the streaming preview, not as a committed bubble; this
  keeps SSR aligned with the per-session live aggregator and avoids the
  duplicate-bubble seen on mid-stream reload.
  """
  threads_task = thread_mgr.list_threads(session_id)
  session_task = session_mgr.get_session(session_id)
  threads, session_meta = await asyncio.gather(threads_task, session_task)
  if session_meta is None:
    raise ValueError(f"session '{session_id}' metadata missing during view build")

  if message_limit is not None and session_meta.archive_offset == 0:
    result = await asyncio.to_thread(_projection_page, session_mgr, session_id, message_limit)
    if result is not None:
      messages, pending_draft, total_event_count, oldest_ordinal, has_more = result
      events = await asyncio.to_thread(session_mgr.load_chat_events_sync, session_id)
      usage = await session_mgr.resolve_session_usage(session_id, session_meta)
      await _mark_read_best_effort(session_mgr, session_id)
      return SessionViewData(
          raw_events=events,
          messages=messages,
          threads=threads,
          usage=usage,
          pending_draft=pending_draft,
          total_event_count=total_event_count,
          oldest_message_ordinal=oldest_ordinal,
          has_more=has_more,
      )

  if message_limit is not None:
    events_task = asyncio.to_thread(session_mgr.load_chat_events_tail, session_id, message_limit)
  else:
    events_task = asyncio.to_thread(session_mgr.load_chat_events_sync, session_id)
  events_result = await events_task

  if message_limit is not None:
    tail_events, total_count, has_more = events_result
    offset = session_meta.archive_offset + total_count - len(tail_events)
    messages, pending_draft = events_to_view(tail_events, event_index_offset=offset)
    usage = await session_mgr.resolve_session_usage(session_id, session_meta)
    raw_events = tail_events
    total_event_count = session_meta.archive_offset + total_count
    oldest_message_ordinal = offset
    has_more = has_more or session_meta.archive_offset > 0
  else:
    raw_events = events_result
    total_event_count = session_meta.archive_offset + len(raw_events)
    oldest_message_ordinal = session_meta.archive_offset
    has_more = session_meta.archive_offset > 0
    messages, pending_draft = events_to_view(raw_events, event_index_offset=session_meta.archive_offset)
    usage = await session_mgr.resolve_session_usage(session_id, session_meta)

  await _mark_read_best_effort(session_mgr, session_id)

  return SessionViewData(
      raw_events=raw_events,
      messages=messages,
      threads=threads,
      usage=usage,
      pending_draft=pending_draft,
      total_event_count=total_event_count,
      oldest_message_ordinal=oldest_message_ordinal,
      has_more=has_more,
  )


def _committed_messages(agg: MessageAggregator, events: list[dict]) -> list[dict]:
  """Feed events through the aggregator and collect the committed message deltas."""
  messages: list[dict] = []
  for delta in agg.feed_indexed(_stable_history_projection(events)):
    if delta["type"] == "message":
      messages.append(delta["message"])
  return messages


def events_to_messages(events: list[dict], event_index_offset: int = 0) -> list[dict]:
  """Convert raw chat_events.jsonl entries into a flat list of displayable messages.

  Final-flushes any in-progress assistant draft; suitable for stable history
  (paginated older events). For the live-render entrypoint, see ``events_to_view``.
  """
  agg = MessageAggregator(event_index_offset=event_index_offset)
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
  agg = MessageAggregator(event_index_offset=event_index_offset)
  return _committed_messages(agg, events), agg.pending_draft_message()
