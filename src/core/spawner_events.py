"""Thread event-log reading and worker-summary event construction."""

import asyncio
from datetime import datetime
from zoneinfo import ZoneInfo

from src.api.message_utils import extract_text_from_message
from src.core import event_types as ET
from src.core.config import HOUSE_TIMEZONE
from src.core.models import ThreadMetadata
from src.core.ndjson import parse_ndjson_tail_parseable
from src.core.threads import ThreadManager

# Events budget of the worker-finish summary: the summary quotes the last parseable
# events of the log, so the reader never needs bytes older than its own budget.
_SUMMARY_EVENT_LIMIT = 80


def _worker_summary_timestamp() -> str:
  return datetime.now(ZoneInfo(HOUSE_TIMEZONE)).strftime('%Y-%m-%d %H:%M %Z')


def _worker_locator_summary(thread_id: str, status: str, timestamp: str) -> str:
  short_id = thread_id[:8]
  return (
      f"Worker `{short_id}` | thread `{thread_id}` | status: {status} | time: {timestamp} | "
      f"find in Workers panel by thread ID")


def _thread_worker_event(
    thread: ThreadMetadata,
    status: str,
    full_content: str,
    *,
    content: str | None,
) -> dict:
  """Build a worker_summary event whose chat content is the thread's locator summary.

  ``content`` overrides the locator summary when the caller already computed it, so
  content and a full_content quoting it stay byte-consistent (one wall-clock read).
  """
  event = {
      "type":
          ET.WORKER_SUMMARY,
      "thread_id":
          thread.id,
      "content":
          content if content is not None else _worker_locator_summary(thread.id, status, _worker_summary_timestamp()),
      "status":
          status,
      "full_content":
          full_content,
  }
  if thread.backend:
    event["resolved_backend"] = thread.backend
  if thread.model:
    event["resolved_model"] = thread.model
  return event


async def _read_thread_events(session_id: str, thread_id: str, thread_mgr: ThreadManager) -> list[dict]:
  """Read a thread's recorded events.jsonl tail, parsed off the event loop.

  The ``asyncio.to_thread`` hop is load-bearing: parsing a long log inline
  would block the event loop.
  """
  events_path = await thread_mgr.get_events_log_path(session_id, thread_id)
  return await asyncio.to_thread(parse_ndjson_tail_parseable, events_path, _SUMMARY_EVENT_LIMIT)


async def read_events_summary(session_id: str, thread_id: str, thread_mgr: ThreadManager) -> str:
  """Read the log's last parseable events, up to the summary budget, for summarization."""
  events = await _read_thread_events(session_id, thread_id, thread_mgr)
  if not events:
    return "(no events recorded)"
  parts = []
  for ev in events:
    ev_type = ev.get("type", "unknown")
    content = _extract_event_content(ev, ev_type)
    if content:
      parts.append(f"[{ev_type}] {content}")
  return "\n".join(parts) if parts else "(empty event log)"


def _extract_event_content(ev: dict, ev_type: str) -> str:
  """Extract human-readable content from a Claude Code stream-json event."""
  if ev_type == ET.RESULT:
    return str(ev.get("result", ""))[:500]

  if ev_type == ET.ASSISTANT:
    raw_message = ev.get("message")
    msg = raw_message if isinstance(raw_message, dict) else {}
    text = extract_text_from_message(msg)
    blocks = msg.get("content") or []
    tool_parts = [
        f"[tool_use: {b.get('name', '?')}]" for b in (blocks if isinstance(blocks, list) else [])
        if isinstance(b, dict) and b.get("type") == ET.TOOL_USE
    ]
    parts = ([text] if text else []) + tool_parts
    return " ".join(parts)[:300]

  if ev_type == ET.RATE_LIMIT_EVENT:
    rli = ev.get("rate_limit_info", {})
    status = rli.get("status", "unknown")
    rate_type = rli.get("rateLimitType", "unknown")
    return f"Rate limit {status} ({rate_type})"

  if ev_type in (ET.THINKING, ET.ERROR, ET.COMPLETE, ET.TOOL_RESULT, ET.TOOL_USE, ET.FILE_WRITE):
    content = ev.get("content", ev.get("message", ""))
    if isinstance(content, list):
      text = extract_text_from_message({"content": content})
      return text[:200]
    return str(content)[:200]

  return ""
