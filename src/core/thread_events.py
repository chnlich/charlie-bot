"""Rendered worker events for the thread-detail view.

The workers panel re-fetches the rendered WorkerEvent list every 5 s while a
running thread's detail is expanded, and a thread's ``data/events.jsonl`` is
append-only, so the rendered list is memoized per file: an unchanged
(dev, ino, size) serves the memo, a grown file parses only its appended tail,
a replaced or truncated file re-parses in full. At most ``_MEMO_MAX`` files
are memoized; the poll pattern touches one file per expanded thread.
"""

import json
import threading
from collections import OrderedDict
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

import structlog

from src.core.message_aggregator import (
  extract_text_from_message,
  extract_tool_result_text,
)
from src.core.models import WorkerEvent

log = structlog.get_logger()

_MEMO_MAX = 8


@dataclass
class _MemoEntry:
  dev: int
  ino: int
  consumed: int  # file offset just past the last fully parsed newline-terminated line
  events: list[WorkerEvent]
  tool_names: dict[str, str]  # tool_use id -> name, resolves tool_result blocks


_memo: OrderedDict[Path, _MemoEntry] = OrderedDict()
_memo_lock = threading.Lock()


def reset_for_tests() -> None:
  with _memo_lock:
    _memo.clear()


def render_worker_events(path: Path) -> list[WorkerEvent]:
  """Return the thread's events rendered for display; repeat calls re-read only the appended tail.

  The consumed offset never passes a partial trailing line, so a torn append
  is re-read once its writer completes it. A missing file renders as empty.
  """
  with _memo_lock:
    try:
      st = path.stat()
    except OSError:
      _memo.pop(path, None)
      return []
    entry = _memo.get(path)
    if entry is not None and (entry.dev, entry.ino) != (st.st_dev, st.st_ino):
      entry = None
    if entry is not None and st.st_size == entry.consumed:
      _memo.move_to_end(path)
      return entry.events
    offset = entry.consumed if entry is not None and st.st_size >= entry.consumed else 0

    with open(path, "rb") as stream:
      stream.seek(offset)
      raw = stream.read(st.st_size - offset)
    consumed = offset + (len(raw) if raw.endswith(b"\n") else raw.rfind(b"\n") + 1)
    # Memoized state is never mutated in place: a response in flight may still
    # be serializing the previously returned list. Seeding the map with the
    # accumulated ids lets cross-region tool_results resolve their tool names.
    tool_names = {} if offset == 0 else dict(entry.tool_names)
    new_events = _render_lines(raw[: consumed - offset], tool_names)
    events = new_events if offset == 0 else entry.events + new_events
    entry = _MemoEntry(st.st_dev, st.st_ino, consumed, events, tool_names)
    _memo[path] = entry
    while len(_memo) > _MEMO_MAX:
      _memo.popitem(last=False)
    return entry.events


def _render_lines(region: bytes, tool_names: dict[str, str]) -> list[WorkerEvent]:
  """Render newline-delimited JSON event rows, skipping blank and malformed lines."""
  events: list[WorkerEvent] = []
  for line in region.decode("utf-8").splitlines():
    if not line.strip():
      continue
    try:
      data = json.loads(line)
    except json.JSONDecodeError as e:
      log.debug("thread_events_parse_skip", error=str(e))
      continue
    _render_event(data, events, tool_names)
  return events


def _render_event(data: dict, events: list[WorkerEvent], tool_names: dict[str, str]) -> None:
  """Render one raw event dict onto *events*, tracking tool_use names in *tool_names*."""
  event_timestamp = data.get("timestamp") or datetime.now(UTC)
  event_type = data.get('type', '')
  if event_type == 'assistant' and isinstance(data.get('message'), dict):
    text = extract_text_from_message(data['message'])
    if text:
      events.append(WorkerEvent(type='assistant', content=text, timestamp=event_timestamp))
    for block in data['message'].get('content', []):
      if isinstance(block, dict) and block.get('type') == 'tool_use':
        tool_names[block['id']] = block['name']
        events.append(
            WorkerEvent(
                type='tool_use',
                tool_name=block['name'],
                input=block.get('input', {}),
                timestamp=event_timestamp,
            ))
  elif event_type == 'user' and isinstance(data.get('message'), dict):
    for block in data['message'].get('content', []):
      if block.get('type') == 'tool_result':
        tool_use_id = block.get('tool_use_id', '')
        name = tool_names.get(tool_use_id, '')
        result_text = extract_tool_result_text(block)
        events.append(
            WorkerEvent(
                type='tool_result',
                tool_name=name,
                content=result_text,
                timestamp=event_timestamp,
            ))
  else:
    try:
      events.append(WorkerEvent(**{k: v for k, v in data.items() if k in WorkerEvent.model_fields}))
    except Exception as e:
      log.debug('event_parse_failed', error=str(e))
      events.append(WorkerEvent(type='raw', content=str(data)))
