"""WebSocket connection registry for live Worker output streaming."""

import asyncio
from collections import defaultdict
from typing import Any

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

  def subscriber_count(self, thread_id: str) -> int:
    return len(self._connections.get(thread_id, set()))


# Module-level singleton used across the application
streaming_manager = StreamingManager()
