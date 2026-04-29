"""Shared helpers for chat message persistence and rendering."""

import asyncio
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import TYPE_CHECKING

import structlog

from src.core import event_types as ET
from src.core.message_aggregator import MessageAggregator, extract_text_from_message

if TYPE_CHECKING:
  from src.core.models import ThreadMetadata
  from src.core.sessions import SessionManager
  from src.core.threads import ThreadManager

log = structlog.get_logger()

_ATTACHED_FILES_MARKER = "\n\n[Attached files]\n"

# Re-export so existing call sites (src.core.spawner, src.core.improve_command,
# tests) keep importing extract_text_from_message from this module.
__all__ = [
    "extract_text_from_message",
    "serialize_uploaded_files",
    "build_agent_input_content",
    "build_user_event",
    "strip_attached_files_block",
    "normalize_user_message_event",
    "SessionViewData",
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


def build_user_event(content: str, uploaded_files: list[dict] | None = None, *, is_voice: bool = False) -> dict:
  """Build the persisted user event payload for chat history and websocket updates."""
  event = {
      "type": ET.USER,
      "content": content,
      "timestamp": datetime.now(timezone.utc).isoformat(),
      "is_voice": is_voice,
  }
  if uploaded_files:
    event["uploaded_files"] = uploaded_files
  return event


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


@dataclass
class SessionViewData:
  """Data produced by the load-events → messages → usage → mark-read pipeline."""
  raw_events: list[dict]
  messages: list[dict]
  threads: list['ThreadMetadata']
  usage: dict | None
  pending_draft: dict | None = None
  total_event_count: int | None = None
  has_more: bool = False


async def build_session_view_data(
    session_id: str,
    session_mgr: 'SessionManager',
    thread_mgr: 'ThreadManager',
    *,
    tail_limit: int | None = None,
) -> SessionViewData:
  """Load events + threads in parallel, derive messages and usage, and mark read.

  When tail_limit is None, loads all events. When set, loads only the last
  tail_limit events (fast path for SPA session switching).

  Returns committed messages plus an optional pending_draft (the in-progress
  assistant draft that has not yet been flushed). Live render paths show the
  pending_draft in the streaming preview, not as a committed bubble; this
  keeps SSR aligned with the per-session live aggregator and avoids the
  duplicate-bubble seen on mid-stream reload.
  """
  if tail_limit is not None:
    events_task = asyncio.to_thread(session_mgr.load_chat_events_tail, session_id, tail_limit)
  else:
    events_task = asyncio.to_thread(session_mgr.load_chat_events_sync, session_id)
  threads_task = thread_mgr.list_threads(session_id)
  session_task = session_mgr.get_session(session_id)
  events_result, threads, session_meta = await asyncio.gather(events_task, threads_task, session_task)
  if session_meta is None:
    raise ValueError(f"session '{session_id}' metadata missing during view build")

  if tail_limit is not None:
    tail_events, total_count, has_more = events_result
    offset = total_count - len(tail_events)
    messages, pending_draft = events_to_view(tail_events, event_index_offset=offset)
    usage = await session_mgr.resolve_session_usage(session_id, session_meta, tail_events)
    raw_events = tail_events
  else:
    raw_events = events_result
    total_count = None
    has_more = False
    messages, pending_draft = events_to_view(raw_events)
    usage = await session_mgr.resolve_session_usage(session_id, session_meta, raw_events)

  try:
    await session_mgr.mark_read(session_id)
  except Exception:
    log.warning("mark_read_failed", session_id=session_id, exc_info=True)

  return SessionViewData(
      raw_events=raw_events,
      messages=messages,
      threads=threads,
      usage=usage,
      pending_draft=pending_draft,
      total_event_count=total_count,
      has_more=has_more,
  )


def events_to_messages(events: list[dict], event_index_offset: int = 0) -> list[dict]:
  """Convert raw chat_events.jsonl entries into a flat list of displayable messages.

  Final-flushes any in-progress assistant draft; suitable for stable history
  (paginated older events). For the live-render entrypoint, see ``events_to_view``.
  """
  agg = MessageAggregator(event_index_offset=event_index_offset)
  messages: list[dict] = []
  for delta in agg.feed_all(events):
    if delta["type"] == "message":
      messages.append(delta["message"])
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
  messages: list[dict] = []
  for delta in agg.feed_all(events):
    if delta["type"] == "message":
      messages.append(delta["message"])
  return messages, agg.pending_draft_message()
