"""Shared helpers for chat message persistence and rendering."""

import asyncio
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Callable, TYPE_CHECKING

import structlog

from src.core import event_types as ET

if TYPE_CHECKING:
  from src.core.models import ThreadMetadata
  from src.core.sessions import SessionManager
  from src.core.threads import ThreadManager

log = structlog.get_logger()

_ATTACHED_FILES_MARKER = "\n\n[Attached files]\n"


def extract_text_from_message(msg: dict | None) -> str:
  """Join text from content blocks of a CC assistant message."""
  blocks = (msg or {}).get("content") or []
  if not isinstance(blocks, list):
    return ""
  return "".join(b.get("text", "") for b in blocks if isinstance(b, dict) and b.get("type") == "text")


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
    messages = events_to_messages(tail_events, event_index_offset=offset)
    usage = await session_mgr.resolve_session_usage(session_id, session_meta, tail_events)
    raw_events = tail_events
  else:
    raw_events = events_result
    total_count = None
    has_more = False
    messages = events_to_messages(raw_events)
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
      total_event_count=total_count,
      has_more=has_more,
  )


def _handler_result_msg(ev: dict) -> dict:
  icon = '\u2713' if ev.get('status') == 'ok' else '\u2717'
  return {
      'role': 'system',
      'content': f"{icon} {ev.get('task', '')}: {ev.get('message', '')}",
  }


def _context_compacted_msg(ev: dict) -> dict:
  trigger = ev.get('trigger', 'auto')
  pre_tokens = ev.get('pre_tokens')
  msg = 'Context compacted'
  if trigger:
    msg += f' ({trigger})'
  if pre_tokens:
    msg += f' \u2014 was {round(pre_tokens / 1000)}k tokens'
  return {'role': 'system', 'content': msg}


# Dispatch table for event types that follow the flush-then-append pattern.
# Each handler returns a message dict (role + content + any extras) or None to
# skip the event.  The loop adds event_index and a default timestamp.
_SIMPLE_HANDLERS: dict[str, Callable[[dict], dict | None]] = {
    ET.MASTER_DONE:
        lambda ev: None if ev.get('still_thinking') else {
            'role': 'separator',
            'thinking_seconds': ev.get('thinking_seconds'),
        },
    ET.ASSISTANT_ERROR:
        lambda ev: {
            'role': 'system',
            'content': f"Error: {ev.get('content', '')}",
        },
    ET.ERROR:
        lambda ev: {
            'role': 'system',
            'content': f"Error: {ev.get('content') or ev.get('message') or 'Unknown error'}",
        },
    ET.TASK_DELEGATED:
        lambda ev: {
            'role': 'task_delegated',
            'content': f"Task delegated: {ev.get('description', '')}",
            'timestamp': ev.get('timestamp') or ev.get('created_at'),
        },
    ET.WORKER_SUMMARY:
        lambda ev: {
            'role': 'worker_summary',
            'content': ev.get('content', ''),
            'full_content': ev.get('full_content', ''),
        },
    ET.HANDLER_RESULT:
        _handler_result_msg,
    ET.CONTEXT_COMPACTED:
        _context_compacted_msg,
    ET.CLONE_START:
        lambda ev: {
            "role": "clone_start",
            "content": ev.get("parent_session_name", "Unknown session"),
            "parent_session_id": ev.get("parent_session_id", ""),
        },
}


def events_to_messages(events: list[dict], event_index_offset: int = 0) -> list[dict]:
  """Convert raw chat_events.jsonl entries into displayable messages.

  Args:
    events: parsed NDJSON event dicts.
    event_index_offset: added to each local index so event_index values
      match real file line positions (used by tail-loading).
  """
  messages = []
  assistant_buf = ""
  last_event_idx = 0
  last_assistant_ts = None
  tools_buf: list[dict] = []

  def _flush() -> None:
    nonlocal assistant_buf, last_assistant_ts, tools_buf
    if assistant_buf or tools_buf:
      msg = {
          "role": "assistant",
          "content": assistant_buf,
          "event_index": last_event_idx,
          "timestamp": last_assistant_ts,
      }
      if tools_buf:
        msg["tools"] = tools_buf
      messages.append(msg)
      assistant_buf = ""
      last_assistant_ts = None
      tools_buf = []

  for idx, ev in enumerate(events):
    idx += event_index_offset
    t = ev.get("type")
    if t == ET.USER:
      # CC-internal user events carry tool_result blocks — attach the output
      # to the most recent tool_use entry so the UI can render it inline.
      if "message" in ev and "content" not in ev:
        for block in (ev.get("message") or {}).get("content", []):
          if isinstance(block, dict) and block.get("type") == "tool_result":
            raw = block.get("content", "")
            if isinstance(raw, list):
              text = "\n".join(p.get("text", "") for p in raw if isinstance(p, dict) and p.get("type") == "text")
            else:
              text = str(raw)
            if tools_buf:
              tools_buf[-1]["output"] = text
              tools_buf[-1]["is_error"] = bool(block.get("is_error", False))
        continue
      _flush()
      normalized = normalize_user_message_event(ev)
      messages.append(
          {
              "role": "user",
              "content": normalized["content"],
              "uploaded_files": normalized["uploaded_files"],
              "is_voice": ev.get("is_voice", False),
              "event_index": idx,
              "timestamp": ev.get("timestamp"),
          })
    elif t == ET.ASSISTANT:
      last_event_idx = idx
      if not assistant_buf:
        last_assistant_ts = ev.get("timestamp")
      msg = ev.get("message") or {}
      blocks = msg.get("content") or []
      for b in blocks:
        if isinstance(b, dict) and b.get('type') == 'tool_use' and b.get('name') == 'ExitPlanMode':
          plan_text = (b.get('input') or {}).get('plan', '')
          if plan_text:
            _flush()
            messages.append(
                {
                    'role': 'plan',
                    'content': plan_text,
                    'event_index': idx,
                    'timestamp': ev.get('timestamp'),
                })
          elif assistant_buf:
            messages.append(
                {
                    'role': 'plan',
                    'content': assistant_buf,
                    'event_index': idx,
                    'timestamp': last_assistant_ts,
                })
            assistant_buf = ''
            last_assistant_ts = None
      for b in blocks:
        if isinstance(b, dict) and b.get('type') == 'tool_use' and b.get('name') != 'ExitPlanMode':
          tools_buf.append({
              'name': b.get('name', ''),
              'input': b.get('input', {}),
              'output': '',
              'is_error': False,
          })
      text = extract_text_from_message(msg)
      if text and assistant_buf:
        _flush()
        last_assistant_ts = ev.get("timestamp")
      assistant_buf += text
    else:
      handler = _SIMPLE_HANDLERS.get(t)
      if handler is not None:
        _flush()
        result = handler(ev)
        if result is not None:
          result.setdefault('timestamp', ev.get('timestamp'))
          result['event_index'] = idx
          messages.append(result)

  _flush()
  return messages
