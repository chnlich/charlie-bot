"""WebSocket connection registry for live Worker output streaming.

``stream`` frames (MessageAggregator's in-progress draft snapshots) are
fire-and-replace preview content: the client swaps its streaming preview for
each frame (usage.js showStreaming), and the catchup replay keeps only the
latest (server._replay_aggregated_catchup). That contract lets the fan-out
coalesce them per channel: the first frame of a burst ships immediately,
later frames in one _STREAM_COALESCE_INTERVAL window collapse into one
trailing send of the newest draft, and frames that hide the preview
client-side (_PREVIEW_HIDING_TYPES) drop the pending draft so a stale preview
never surfaces after one of them.
"""

import asyncio
import json
from collections import defaultdict
from collections.abc import Awaitable, Callable
from typing import Any

import structlog
from fastapi import WebSocket

from src.core import event_types as ET
from src.core.tasks import create_logged_task

log = structlog.get_logger()

# The client already throttles stream-draft paints to a 200 ms cadence
# (usage.js showStreaming), so fan-out at the same cadence adds at most one
# client paint tick of latency while cutting per-delta serialization of the
# whole accumulated draft to one dump per window.
_STREAM_COALESCE_INTERVAL = 0.2  # seconds

# websocket.js hideStreaming reach: _commitMessage on every ``message`` frame
# (comments there pin the commit-supersedes-preview rule) plus the
# ``assistant_error`` and ``error`` arms of the event switch.
_PREVIEW_HIDING_TYPES = frozenset({"message", "assistant_error", "error"})


class StreamingManager:
  """Fan-out WebSocket events from Worker subprocesses to browser clients."""

  def __init__(self):
    self._connections: dict[str, set[WebSocket]] = defaultdict(set)
    self._pending_stream: dict[str, dict[str, Any]] = {}
    self._stream_timers: dict[str, asyncio.TimerHandle] = {}
    self._lock = asyncio.Lock()

  async def subscribe(self, thread_id: str, ws: WebSocket) -> None:
    async with self._lock:
      self._connections[thread_id].add(ws)

  async def unsubscribe(self, thread_id: str, ws: WebSocket) -> None:
    async with self._lock:
      self._connections[thread_id].discard(ws)
      if not self._connections[thread_id]:
        del self._connections[thread_id]
    if thread_id not in self._connections:
      # Flushing to zero sockets is wasted work; the next subscriber's catchup supplies the latest draft.
      self._drop_pending_stream(thread_id)

  async def broadcast(self, thread_id: str, event: dict[str, Any]) -> None:
    """Send a JSON event to all subscribers of this thread; ``stream`` frames
    coalesce per the module docstring, every frame serializes once per fan-out
    (not per socket), and a subscriber-less channel costs a dict lookup."""
    if event.get("type") == "stream" and not self._connections.get(thread_id):
      return
    if event.get("type") == "stream":
      if thread_id in self._stream_timers:
        self._pending_stream[thread_id] = event
        return
      self._stream_timers[thread_id] = asyncio.get_running_loop().call_later(
          _STREAM_COALESCE_INTERVAL, self._stream_window_ended, thread_id)
      await self._fan_out(thread_id, event)
      return
    if event.get("type") in _PREVIEW_HIDING_TYPES:
      self._pending_stream.pop(thread_id, None)
    await self._fan_out(thread_id, event)

  def _stream_window_ended(self, thread_id: str) -> None:
    """Timer callback: flush the window's pending draft, then close the window."""
    self._stream_timers.pop(thread_id, None)
    event = self._pending_stream.pop(thread_id, None)
    if event is not None:
      create_logged_task(self._fan_out(thread_id, _serialize(event)), name=f"stream-flush-{thread_id}")

  def _drop_pending_stream(self, thread_id: str) -> None:
    timer = self._stream_timers.pop(thread_id, None)
    if timer is not None:
      timer.cancel()
    self._pending_stream.pop(thread_id, None)

  async def _fan_out(self, thread_id: str, payload: dict[str, Any] | str) -> None:
    """Send to the channel's sockets, serializing dict payloads; a str arrives serialized."""
    async with self._lock:
      sockets = set(self._connections.get(thread_id, set()))
    if sockets:
      text = payload if isinstance(payload, str) else _serialize(payload)
      await self._send_all(thread_id, sockets, text)

  async def _send_all(self, thread_id: str, sockets: set[WebSocket], text: str) -> None:
    dead: set[WebSocket] = set()
    for ws in sockets:
      try:
        await ws.send_text(text)
      except Exception as e:
        log.debug("ws_send_failed", channel=thread_id, error=str(e))
        dead.add(ws)

    if dead:
      async with self._lock:
        self._connections[thread_id] -= dead

  async def close_all(self) -> None:
    """Close all active WebSocket connections (called during server shutdown)."""
    for thread_id in list(self._stream_timers):
      self._drop_pending_stream(thread_id)
    async with self._lock:
      all_sockets = {ws for sockets in self._connections.values() for ws in sockets}
    for ws in all_sockets:
      try:
        await ws.close()
      except Exception as e:
        log.debug("ws_close_on_shutdown_failed", error=str(e))


def _serialize(event: dict[str, Any]) -> str:
  """Same dumps starlette's send_json ran, so the wire bytes are unchanged."""
  return json.dumps(event, separators=(",", ":"), ensure_ascii=False)


# Module-level singleton used across the application
streaming_manager = StreamingManager()


async def handle_compaction_events(
    event: dict[str, Any],
    persist_and_broadcast: Callable[[dict[str, Any]], Awaitable[None]],
    log_context: dict[str, Any],
) -> None:
  """Detect compact_boundary and compact-failure system events, log, persist, and
  broadcast a synthesized event. At most one synthesized event is emitted per
  input event."""
  if event.get("type") != ET.SYSTEM:
    return
  subtype = event.get("subtype")
  if subtype == ET.COMPACT_BOUNDARY:
    meta = event.get(ET.COMPACT_METADATA, {})
    trigger = meta.get("trigger", "unknown")
    pre_tokens = meta.get("pre_tokens")
    log.info("cc_context_compacted", trigger=trigger, pre_tokens=pre_tokens, **log_context)
    compact_event: dict[str, Any] = {
        "type": ET.CONTEXT_COMPACTED,
        "trigger": trigger,
        "pre_tokens": pre_tokens,
    }
    await persist_and_broadcast(compact_event)
    return
  if subtype == "status" and event.get("compact_result") == "failed":
    error = event.get("compact_error")
    log.info("cc_context_compact_failed", error=error, **log_context)
    compact_failed_event: dict[str, Any] = {
        "type": ET.CONTEXT_COMPACT_FAILED,
        "error": error,
    }
    await persist_and_broadcast(compact_failed_event)
