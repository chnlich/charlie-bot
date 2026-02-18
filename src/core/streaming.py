"""WebSocket connection registry for live Worker output streaming."""

import asyncio
from collections import defaultdict
from typing import Any

from fastapi import WebSocket


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
      except Exception:
        dead.add(ws)

    if dead:
      async with self._lock:
        self._connections[thread_id] -= dead

  def subscriber_count(self, thread_id: str) -> int:
    return len(self._connections.get(thread_id, set()))


# Module-level singleton used across the application
streaming_manager = StreamingManager()
