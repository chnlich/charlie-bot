"""Stateful aggregator that turns raw chat events into renderable message deltas.

The aggregator is the single source of truth for chat-event aggregation. All
three render paths (SSR, WS catchup, WS live broadcast) feed events through
the same logic, eliminating the previous duplication between
`events_to_messages` (Python) and `handleWSEvent` (JavaScript).

Two delta shapes are emitted:
  * ``{"type": "message", "message": {role, content, ...}}`` -- a finalized
    message; append to the rendered chat.
  * ``{"type": "stream", "message": {role: "assistant", content, ...}}`` --
    the in-progress assistant draft snapshot; replace the streaming preview.

A subsequent flush-triggering event (user, master_done, etc.) emits a
``message`` delta carrying the buffered content; the next ``stream`` delta
(or the absence of one) tells the client whether to keep or clear the preview.
"""

from collections.abc import Iterator
from typing import Callable

from src.core import event_types as ET


def extract_text_from_message(msg: dict | None) -> str:
  """Join text from content blocks of a CC assistant message."""
  blocks = (msg or {}).get("content") or []
  if not isinstance(blocks, list):
    return ""
  return "".join(b.get("text", "") for b in blocks if isinstance(b, dict) and b.get("type") == "text")


def _handler_result_msg(ev: dict) -> dict:
  icon = '✓' if ev.get('status') == 'ok' else '✗'
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
    msg += f' — was {round(pre_tokens / 1000)}k tokens'
  return {'role': 'system', 'content': msg}


# Dispatch table for event types that follow the flush-then-append pattern.
# Each handler returns a message dict (role + content + any extras) or None to
# skip the event.  The aggregator adds event_index and a default timestamp.
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


class MessageAggregator:
  """Convert raw chat events into a stream of message + stream deltas."""

  def __init__(self, event_index_offset: int = 0):
    self._idx_offset = event_index_offset
    self._processed = 0
    self._assistant_buf = ""
    self._last_assistant_ts: str | None = None
    self._last_event_idx = 0
    self._tools_buf: list[dict] = []

  def feed(self, event: dict) -> Iterator[dict]:
    """Process a single event, yield zero or more deltas."""
    idx = self._idx_offset + self._processed
    self._processed += 1
    yield from self._feed(event, idx)

  def feed_all(self, events: list[dict]) -> Iterator[dict]:
    """Feed a list of events in order, yielding deltas as they arise."""
    for ev in events:
      yield from self.feed(ev)

  def flush_pending(self) -> Iterator[dict]:
    """Emit any pending assistant draft as a finalized message.

    Use at the end of a fully-completed history (paginated load, snapshot
    rendering of a stable conversation). Live render paths should not call
    this -- the draft remains in the streaming preview until a flushing
    event commits it.
    """
    yield from self._flush_to_message_delta()

  def pending_draft_message(self) -> dict | None:
    """Snapshot the current draft. Returns None if there's nothing buffered."""
    if not self._assistant_buf and not self._tools_buf:
      return None
    msg: dict = {
        "role": "assistant",
        "content": self._assistant_buf,
        "event_index": self._last_event_idx,
        "timestamp": self._last_assistant_ts,
    }
    if self._tools_buf:
      msg["tools"] = [dict(t) for t in self._tools_buf]
    return msg

  # ---------------------------------------------------------------------------
  # private
  # ---------------------------------------------------------------------------

  def _flush_to_message_delta(self) -> Iterator[dict]:
    msg = self.pending_draft_message()
    if msg is not None:
      self._assistant_buf = ""
      self._last_assistant_ts = None
      self._tools_buf = []
      yield {"type": "message", "message": msg}

  def _stream_delta(self) -> dict | None:
    msg = self.pending_draft_message()
    if msg is None:
      return None
    return {"type": "stream", "message": msg}

  def _feed(self, ev: dict, idx: int) -> Iterator[dict]:
    t = ev.get("type")
    if t == ET.USER:
      # CC-internal user events carry tool_result blocks -- attach the output
      # to the most recent tool_use entry so the UI can render it inline.
      if "message" in ev and "content" not in ev:
        for block in (ev.get("message") or {}).get("content", []):
          if isinstance(block, dict) and block.get("type") == "tool_result":
            raw = block.get("content", "")
            if isinstance(raw, list):
              text = "\n".join(p.get("text", "") for p in raw if isinstance(p, dict) and p.get("type") == "text")
            else:
              text = str(raw)
            if self._tools_buf:
              self._tools_buf[-1]["output"] = text
              self._tools_buf[-1]["is_error"] = bool(block.get("is_error", False))
        delta = self._stream_delta()
        if delta is not None:
          yield delta
        return
      yield from self._flush_to_message_delta()
      from src.api.message_utils import normalize_user_message_event
      normalized = normalize_user_message_event(ev)
      yield {
          "type": "message",
          "message": {
              "role": "user",
              "content": normalized["content"],
              "uploaded_files": normalized["uploaded_files"],
              "is_voice": ev.get("is_voice", False),
              "event_index": idx,
              "timestamp": ev.get("timestamp"),
          },
      }
      return

    if t == ET.ASSISTANT:
      self._last_event_idx = idx
      if not self._assistant_buf:
        self._last_assistant_ts = ev.get("timestamp")
      msg = ev.get("message") or {}
      blocks = msg.get("content") or []

      for b in blocks:
        if isinstance(b, dict) and b.get('type') == 'tool_use' and b.get('name') == 'ExitPlanMode':
          plan_text = (b.get('input') or {}).get('plan', '')
          if plan_text:
            yield from self._flush_to_message_delta()
            yield {
                "type": "message",
                "message": {
                    'role': 'plan',
                    'content': plan_text,
                    'event_index': idx,
                    'timestamp': ev.get('timestamp'),
                },
            }
          elif self._assistant_buf:
            plan_msg = {
                'role': 'plan',
                'content': self._assistant_buf,
                'event_index': idx,
                'timestamp': self._last_assistant_ts,
            }
            self._assistant_buf = ''
            self._last_assistant_ts = None
            yield {"type": "message", "message": plan_msg}

      for b in blocks:
        if isinstance(b, dict) and b.get('type') == 'tool_use' and b.get('name') != 'ExitPlanMode':
          self._tools_buf.append({
              'name': b.get('name', ''),
              'input': b.get('input', {}),
              'output': '',
              'is_error': False,
          })

      text = extract_text_from_message(msg)
      if text and self._assistant_buf:
        yield from self._flush_to_message_delta()
        self._last_assistant_ts = ev.get("timestamp")
      self._assistant_buf += text

      delta = self._stream_delta()
      if delta is not None:
        yield delta
      return

    handler = _SIMPLE_HANDLERS.get(t)
    if handler is None:
      return
    yield from self._flush_to_message_delta()
    result = handler(ev)
    if result is not None:
      result.setdefault('timestamp', ev.get('timestamp'))
      result['event_index'] = idx
      yield {"type": "message", "message": result}
