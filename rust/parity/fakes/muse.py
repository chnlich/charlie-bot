"""Fake Muse realtime WebSocket; seeds are transcript and handshake_error."""

import json
from collections.abc import Callable

from . import proxy, recorder


def validate_seed(seed: dict) -> None:
  unknown = set(seed) - {"transcript", "handshake_error"}
  if unknown:
    raise ValueError(f"unknown seed keys {sorted(unknown)}")
  if "transcript" in seed and not isinstance(seed["transcript"], str):
    raise ValueError("transcript must be a string")
  if "handshake_error" in seed and not isinstance(seed["handshake_error"], str):
    raise ValueError("handshake_error must be a string")


class Fake:
  """The deterministic Muse endpoint used by upload and preview scenarios."""

  def __init__(self, seed: dict, traffic: recorder.Recorder, mark_ready: Callable[[], None]) -> None:
    validate_seed(seed)
    self.transcript = seed.get("transcript", "Synthetic Muse transcript.")
    self.handshake_error = seed.get("handshake_error")
    self.traffic = traffic
    self.mark_ready = mark_ready

  def http(self, request: proxy.Request) -> proxy.HTTPResponse:
    return proxy.HTTPResponse(200, {"Content-Type": "application/json"}, b'{"ok":true}')

  async def websocket(self, connection: proxy.WebSocketConnection) -> None:
    handshake = await connection.recv()
    if not isinstance(handshake, str):
      raise ValueError("Muse expected its handshake as a text frame")
    json.loads(handshake)
    if self.handshake_error is not None:
      await connection.send_text(json.dumps({"type": "error", "message": self.handshake_error}, separators=(",", ":")))
      return
    await connection.send_text('{"type":"connected"}')
    while (raw := await connection.recv()) is not None:
      if isinstance(raw, str) and json.loads(raw).get("type") == "endStream":
        final = {"type": "transcript", "final": True, "text": self.transcript}
        complete = {"type": "speechComplete", "transcript": self.transcript}
        await connection.send_text(json.dumps(final, separators=(",", ":")))
        await connection.send_text(json.dumps(complete, separators=(",", ":")))
        return
