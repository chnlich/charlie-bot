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


def extract_thinking_from_message(msg: dict | None) -> str:
  """Join thinking content blocks of a CC assistant message.

  Signatures are intentionally discarded; only the human-readable ``thinking``
  field is returned.
  """
  blocks = (msg or {}).get("content") or []
  if not isinstance(blocks, list):
    return ""
  return "".join(b.get("thinking", "") for b in blocks if isinstance(b, dict) and b.get("type") == "thinking")


def extract_tool_result_text(block: dict) -> str:
  """Return the renderable text of a CC ``tool_result`` block; ``content`` is either a plain string or a list of typed parts where only ``{"type": "text"}`` parts contribute."""
  raw = block.get("content", "")
  if isinstance(raw, list):
    return "\n".join(p.get("text", "") for p in raw if isinstance(p, dict) and p.get("type") == "text")
  return str(raw)


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
  return {'role': 'system', 'content': msg, 'kind': 'context_compacted'}


def _context_compact_failed_msg(ev: dict) -> dict:
  error = ev.get('error')
  content = 'Compaction failed' if not error else f'Compaction failed — {error}'
  return {'role': 'system', 'kind': 'context_compact_failed', 'content': content}


def _resume_context_dropped_msg(ev: dict) -> dict:
  reason = ev.get('reason')
  if reason == 'anchor_missing':
    msg = 'Context not resumed: no previous session anchor was found'
  elif reason == 'transcript_missing':
    msg = 'Context not resumed: the previous session transcript is missing'
  else:
    msg = 'Context not resumed'
  return {'role': 'system', 'content': f'{msg} — started a new conversation'}


def _system_msg(ev: dict) -> dict | None:
  if ev.get("subtype") != "tui_menu_dismissed":
    return None
  return {
      "role": "system",
      "content": ev.get("content", ""),
  }


def _backend_switched_msg(ev: dict) -> dict:
  return {
      "role": "system",
      "content": f"Backend switched: {ev.get('from', '')} → {ev.get('to', '')}",
  }


def _scheduled_run_skipped_msg(ev: dict) -> dict:
  task = ev.get('task', '')
  skipped_at = ev.get('skipped_at', '')
  reason = ev.get('reason', '')
  return {
      'role': 'system',
      'content': f"Scheduled run of '{task}' skipped at {skipped_at}: {reason}",
  }


def _task_delegated_msg(ev: dict) -> dict:
  backend = ev.get("backend") or ev.get("resolved_backend") or ""
  model = ev.get("model") or ev.get("resolved_model") or ""
  return {
      "role": "task_delegated",
      "content": "Task delegated",
      "thread_id": ev.get("thread_id", ""),
      "description": ev.get("description", ""),
      "delegate_invocation": ev.get("delegate_invocation"),
      "backend": backend,
      "model": model,
      "timestamp": ev.get("timestamp") or ev.get("created_at"),
  }


# Dispatch table for event types that usually follow the flush-then-append pattern.
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
        _task_delegated_msg,
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
    ET.CONTEXT_COMPACT_FAILED:
        _context_compact_failed_msg,
    ET.RESUME_CONTEXT_DROPPED:
        _resume_context_dropped_msg,
    ET.SYSTEM:
        _system_msg,
    ET.CLONE_START:
        lambda ev: {
            "role": "clone_start",
            "content": ev.get("parent_session_name", "Unknown session"),
            "parent_session_id": ev.get("parent_session_id", ""),
        },
    ET.SCHEDULED_TRIGGER:
        lambda ev: {
            "role": "scheduled_trigger",
            "content": ev.get("content", ""),
        },
    ET.AGENT_MESSAGE:
        lambda ev: {
            "role": "agent_message",
            "content": ev.get("content", ""),
            "from_session": ev.get("from_session", ""),
            "from_session_name": ev.get("from_session_name", ""),
        },
    ET.BACKEND_SWITCHED:
        _backend_switched_msg,
    ET.SCHEDULED_RUN_SKIPPED:
        _scheduled_run_skipped_msg,
}


class MessageAggregator:
  """Convert raw chat events into a stream of message + stream deltas."""

  def __init__(self, event_index_offset: int = 0):
    self._idx_offset = event_index_offset
    self._processed = 0
    self._assistant_buf = ""
    self._thinking_buf = ""
    self._last_assistant_ts: str | None = None
    self._last_event_idx = 0
    self._last_event_id: str | None = None
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

  def feed_indexed(self, events: list[tuple[int, dict]]) -> Iterator[dict]:
    """Feed projected events while preserving their original list indices."""
    for idx, ev in events:
      self._processed += 1
      yield from self._feed(ev, self._idx_offset + idx)

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
    if not self._assistant_buf and not self._tools_buf and not self._thinking_buf:
      return None
    msg: dict = {
        "role": "assistant",
        "content": self._assistant_buf,
        "event_index": self._last_event_idx,
        "id": self._last_event_id or f"legacy:{self._last_event_idx}",
        "timestamp": self._last_assistant_ts,
    }
    if self._tools_buf:
      msg["tools"] = [dict(t) for t in self._tools_buf]
    if self._thinking_buf:
      msg["thinking"] = self._thinking_buf
    return msg

  # ---------------------------------------------------------------------------
  # private
  # ---------------------------------------------------------------------------

  def _flush_to_message_delta(self) -> Iterator[dict]:
    msg = self.pending_draft_message()
    if msg is not None:
      self._assistant_buf = ""
      self._thinking_buf = ""
      self._last_assistant_ts = None
      self._last_event_id = None
      self._tools_buf = []
      yield {"type": "message", "message": msg}

  def _stream_delta(self) -> dict | None:
    msg = self.pending_draft_message()
    if msg is None:
      return None
    return {"type": "stream", "message": msg}

  def _feed(self, ev: dict, idx: int) -> Iterator[dict]:
    t = ev.get("type")
    ev_id = str(ev.get("id") or f"legacy:{idx}")
    if t == ET.USER:
      # CC-internal user events carry tool_result blocks -- attach the output
      # to the most recent tool_use entry so the UI can render it inline.
      if "message" in ev and "content" not in ev:
        for block in (ev.get("message") or {}).get("content", []):
          if isinstance(block, dict) and block.get("type") == "tool_result":
            text = extract_tool_result_text(block)
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
          "message":
              {
                  "role": "user",
                  "content": normalized["content"],
                  "uploaded_files": normalized["uploaded_files"],
                  "is_voice": ev.get("is_voice", False),
                  "event_index": idx,
                  "id": ev_id,
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
                "message":
                    {
                        'role': 'plan',
                        'content': plan_text,
                        'event_index': idx,
                        'id': ev_id,
                        'timestamp': ev.get('timestamp'),
                    },
            }
          elif self._assistant_buf:
            plan_msg = {
                'role': 'plan',
                'content': self._assistant_buf,
                'event_index': idx,
                'id': ev_id,
                'timestamp': self._last_assistant_ts,
            }
            self._assistant_buf = ''
            self._last_assistant_ts = None
            self._last_event_id = None
            yield {"type": "message", "message": plan_msg}

      for b in blocks:
        if isinstance(b, dict) and b.get('type') == 'tool_use' and b.get('name') != 'ExitPlanMode':
          self._tools_buf.append(
              {
                  'name': b.get('name', ''),
                  'input': b.get('input', {}),
                  'output': '',
                  'is_error': False,
              })

      thinking_snapshot = extract_thinking_from_message(msg)
      if thinking_snapshot:
        if self._thinking_buf and thinking_snapshot.startswith(self._thinking_buf):
          self._thinking_buf = thinking_snapshot
        else:
          self._thinking_buf = thinking_snapshot

      text = extract_text_from_message(msg)
      if text and self._assistant_buf:
        yield from self._flush_to_message_delta()
        self._last_assistant_ts = ev.get("timestamp")
      self._assistant_buf += text
      if self._assistant_buf or self._tools_buf or self._thinking_buf:
        self._last_event_id = ev_id

      delta = self._stream_delta()
      if delta is not None:
        yield delta
      return

    if t == ET.TOOL_USE:
      self._tools_buf.append(
          {
              'name': ev.get('name', ''),
              'input': ev.get('input', {}),
              'output': '',
              'is_error': False,
          })
      self._last_event_idx = idx
      self._last_event_id = ev_id
      if self._last_assistant_ts is None:
        self._last_assistant_ts = ev.get('timestamp')
      delta = self._stream_delta()
      if delta is not None:
        yield delta
      return

    if t == ET.TOOL_RESULT:
      if not self._tools_buf:
        return
      self._tools_buf[-1]['output'] = ev.get('content', '')
      self._tools_buf[-1]['is_error'] = bool(ev.get('is_error', False))
      self._last_event_idx = idx
      delta = self._stream_delta()
      if delta is not None:
        yield delta
      return

    if t == ET.THINKING:
      self._last_event_idx = idx
      self._thinking_buf += str(ev.get("content", ""))
      if self._last_assistant_ts is None:
        self._last_assistant_ts = ev.get("timestamp")
      if self._assistant_buf or self._tools_buf or self._thinking_buf:
        self._last_event_id = ev_id
      delta = self._stream_delta()
      if delta is not None:
        yield delta
      return

    handler = _SIMPLE_HANDLERS.get(t)
    if handler is None:
      return
    if t == ET.SYSTEM:
      result = handler(ev)
      if result is None:
        return
      yield from self._flush_to_message_delta()
      result.setdefault('timestamp', ev.get('timestamp'))
      result['event_index'] = idx
      result['id'] = ev_id
      yield {"type": "message", "message": result}
      return
    yield from self._flush_to_message_delta()
    result = handler(ev)
    if result is not None:
      result.setdefault('timestamp', ev.get('timestamp'))
      result['event_index'] = idx
      result['id'] = ev_id
      yield {"type": "message", "message": result}
