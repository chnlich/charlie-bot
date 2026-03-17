"""WebSocket connection registry for live Worker output streaming."""

import asyncio
from collections import defaultdict
from typing import Any, Awaitable, Callable

import structlog
from fastapi import WebSocket

log = structlog.get_logger()


class StreamingManager:
  """Fan-out WebSocket events from Worker subprocesses to browser clients."""

  def __init__(self):
    self._connections: dict[str, set[WebSocket]] = defaultdict(set)
    self._lock = asyncio.Lock()

  async def subscribe(self, thread_id: str, ws: WebSocket) -> None:
    async with self._lock:
      self._connections[thread_id].add(ws)

  async def unsubscribe(self, thread_id: str, ws: WebSocket) -> None:
    async with self._lock:
      self._connections[thread_id].discard(ws)
      if not self._connections[thread_id]:
        del self._connections[thread_id]

  async def broadcast(self, thread_id: str, event: dict[str, Any]) -> None:
    """Send a JSON event to all WebSocket subscribers of this thread."""
    async with self._lock:
      sockets = set(self._connections.get(thread_id, set()))

    dead: set[WebSocket] = set()
    for ws in sockets:
      try:
        await ws.send_json(event)
      except Exception as e:
        log.debug("ws_send_failed", channel=thread_id, error=str(e))
        dead.add(ws)

    if dead:
      async with self._lock:
        self._connections[thread_id] -= dead

  async def close_all(self) -> None:
    """Close all active WebSocket connections (called during server shutdown)."""
    async with self._lock:
      all_sockets = {ws for sockets in self._connections.values() for ws in sockets}
    for ws in all_sockets:
      try:
        await ws.close()
      except Exception as e:
        log.debug("ws_close_on_shutdown_failed", error=str(e))


# Module-level singleton used across the application
streaming_manager = StreamingManager()


async def handle_compact_boundary(
    event: dict[str, Any],
    persist_and_broadcast: Callable[[dict[str, Any]], Awaitable[None]],
    log_context: dict[str, Any],
) -> None:
  """Detect compact_boundary system events, log, persist, and broadcast a context_compacted event."""
  if event.get("type") != "system" or event.get("subtype") != "compact_boundary":
    return
  meta = event.get("compact_metadata", {})
  trigger = meta.get("trigger", "unknown")
  pre_tokens = meta.get("pre_tokens")
  log.info("cc_context_compacted", trigger=trigger, pre_tokens=pre_tokens, **log_context)
  compact_event: dict[str, Any] = {
      "type": "context_compacted",
      "trigger": trigger,
      "pre_tokens": pre_tokens,
  }
  await persist_and_broadcast(compact_event)
