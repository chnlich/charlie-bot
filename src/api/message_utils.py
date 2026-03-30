"""Shared helpers for converting raw chat events into displayable messages."""

import asyncio
from dataclasses import dataclass
from typing import Callable, TYPE_CHECKING

import structlog

from src.core import event_types as ET

if TYPE_CHECKING:
  from src.core.models import ThreadMetadata
  from src.core.sessions import SessionManager
  from src.core.threads import ThreadManager

log = structlog.get_logger()


def extract_text_from_message(msg: dict | None) -> str:
  """Join text from content blocks of a CC assistant message."""
  blocks = (msg or {}).get("content") or []
  if not isinstance(blocks, list):
    return ""
  return "".join(b.get("text", "") for b in blocks if isinstance(b, dict) and b.get("type") == "text")


@dataclass
class SessionViewData:
  """Data produced by the load-events → messages → usage → mark-read pipeline."""
  raw_events: list[dict]
  messages: list[dict]
  threads: list['ThreadMetadata']
  usage: dict | None


async def build_session_view_data(
    session_id: str,
    session_mgr: 'SessionManager',
    thread_mgr: 'ThreadManager',
) -> SessionViewData:
  """Load events + threads in parallel, derive messages and usage, and mark read."""
  events_task = asyncio.to_thread(session_mgr.load_chat_events_sync, session_id)
  threads_task = thread_mgr.list_threads(session_id)
  raw_events, threads = await asyncio.gather(events_task, threads_task)
  messages = events_to_messages(raw_events)
  usage = session_mgr.get_usage_cached(session_id) or session_mgr.usage_from_events(raw_events)
  try:
    await session_mgr.mark_read(session_id)
  except Exception:
    log.warning("mark_read_failed", session_id=session_id, exc_info=True)
  return SessionViewData(raw_events=raw_events, messages=messages, threads=threads, usage=usage)


@dataclass
class FastSessionViewData:
  """Data from tail-loading: messages from the last N events + usage from cache."""
  messages: list[dict]
  threads: list['ThreadMetadata']
  usage: dict | None
  total_event_count: int
  has_more: bool


async def build_session_view_data_fast(
    session_id: str,
    session_mgr: 'SessionManager',
    thread_mgr: 'ThreadManager',
    limit: int = 200,
) -> FastSessionViewData:
  """Fast path: load only the tail events + cached usage. No full event scan."""
  tail_task = asyncio.to_thread(session_mgr.load_chat_events_tail, session_id, limit)
  threads_task = thread_mgr.list_threads(session_id)
  (tail_events, total_count, has_more), threads = await asyncio.gather(tail_task, threads_task)

  # Offset so event_index values match real file line positions
  offset = total_count - len(tail_events)
  messages = events_to_messages(tail_events, event_index_offset=offset)

  usage = session_mgr.get_usage_cached(session_id)

  try:
    await session_mgr.mark_read(session_id)
  except Exception:
    log.warning("mark_read_failed", session_id=session_id, exc_info=True)

  return FastSessionViewData(
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

  def _flush() -> None:
    nonlocal assistant_buf, last_assistant_ts
    if assistant_buf:
      messages.append(
          {
              "role": "assistant",
              "content": assistant_buf,
              "event_index": last_event_idx,
              "timestamp": last_assistant_ts,
          })
      assistant_buf = ""
      last_assistant_ts = None

  for idx, ev in enumerate(events):
    idx += event_index_offset
    t = ev.get("type")
    if t == ET.USER:
      # Skip CC-internal user events (tool results) — they have a "message" field
      # but no top-level "content". Only real user messages have "content".
      if "message" in ev and "content" not in ev:
        continue
      _flush()
      messages.append(
          {
              "role": "user",
              "content": ev.get("content", ""),
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
