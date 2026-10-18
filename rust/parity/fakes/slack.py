"""Fake Slack API and Socket Mode server; seeds are channel_name, thread_messages and failures."""

import asyncio
import json
from collections.abc import Callable
from typing import Any

from . import proxy, recorder

_FIRST_REPLY_COUNTER = 200
_SOCKET_MODE_URL = "wss://wss-primary.slack.com/link/00000000000000000000000000000000"
_ACK_TIMEOUT_SECONDS = 5.0


def validate_seed(seed: dict) -> None:
  unknown = set(seed) - {"channel_name", "thread_messages", "failures"}
  if unknown:
    raise ValueError(f"unknown seed keys {sorted(unknown)}")
  if "channel_name" in seed and (not isinstance(seed["channel_name"], str) or not seed["channel_name"]):
    raise ValueError("channel_name must be a non-empty string")
  if "thread_messages" in seed and (not isinstance(seed["thread_messages"], list) or
                                    any(not isinstance(message, dict) for message in seed["thread_messages"])):
    raise ValueError("thread_messages must be a list of message mappings")
  _validate_failures(seed.get("failures", {}))


def _validate_failures(failures: Any) -> None:
  if not isinstance(failures, dict):
    raise ValueError("failures must map API paths to HTTP error statuses")
  for path, status in failures.items():
    if not isinstance(path, str) or not path.startswith("/"):
      raise ValueError("failure keys must be absolute API paths")
    if not isinstance(status, int) or isinstance(status, bool) or not 400 <= status <= 599:
      raise ValueError(f"failure status for {path!r} must be an HTTP error status")


def _response(value: dict, status: int = 200) -> proxy.HTTPResponse:
  return proxy.HTTPResponse(
      status, {"Content-Type": "application/json"},
      json.dumps(value, separators=(",", ":")).encode())


class Fake:
  """The deterministic Slack service used by scenarios that declare ``fakes.slack``."""

  def __init__(self, seed: dict, traffic: recorder.Recorder, mark_ready: Callable[[], None]) -> None:
    validate_seed(seed)
    self.channel_name = seed.get("channel_name", "parity-room")
    self.thread_messages = seed.get("thread_messages", [])
    self.failures = seed.get("failures", {})
    self.traffic = traffic
    self.mark_ready = mark_ready
    self._reply_counter = _FIRST_REPLY_COUNTER
    self._envelope_counter = 0
    self._connection: proxy.WebSocketConnection | None = None
    self._acks: dict[str, asyncio.Event] = {}

  def http(self, request: proxy.Request) -> proxy.HTTPResponse:
    if request.path in self.failures:
      return _response({"ok": False, "error": "seeded_failure"}, self.failures[request.path])
    body = request.body if isinstance(request.body, dict) else {}
    if request.path == "/api/apps.connections.open":
      value = {"ok": True, "url": _SOCKET_MODE_URL}
    elif request.path == "/api/chat.postMessage":
      self._reply_counter += 1
      timestamp = f"1767600000.{self._reply_counter:06d}"
      value = {
          "ok": True,
          "channel": body.get("channel", "C0PARITY"),
          "ts": timestamp,
          "message": {
              "channel": body.get("channel", "C0PARITY"),
              "ts": timestamp,
              "text": body.get("text", "")
          },
      }
    elif request.path == "/api/conversations.replies":
      value = {"ok": True, "messages": self.thread_messages}
    elif request.path == "/api/conversations.info":
      value = {"ok": True, "channel": {"id": _first_query(request, "channel", "C0PARITY"), "name": self.channel_name}}
    elif request.path in ("/api/reactions.add", "/api/reactions.remove"):
      value = {"ok": True}
    else:
      value = {"ok": True}
    return _response(value)

  async def websocket(self, connection: proxy.WebSocketConnection) -> None:
    self._connection = connection
    await connection.send_text(json.dumps({"type": "hello", "num_connections": 1}, separators=(",", ":")))
    self.mark_ready()
    try:
      while (raw := await connection.recv()) is not None:
        if not isinstance(raw, str):
          raise ValueError("Slack Socket Mode received a binary frame")
        envelope = json.loads(raw)
        envelope_id = envelope.get("envelope_id")
        if envelope_id in self._acks:
          self._acks[envelope_id].set()
    finally:
      if self._connection is connection:
        self._connection = None

  async def push(self, action: str, payload: dict) -> None:
    if action != "socket_event":
      raise ValueError(f"unknown Slack fake action {action!r}")
    connection = self._connection
    if connection is None:
      raise RuntimeError("Slack Socket Mode is not connected")
    self._envelope_counter += 1
    envelope_id = f"parity-envelope-{self._envelope_counter:04d}"
    acknowledged = self._acks[envelope_id] = asyncio.Event()
    envelope = {"envelope_id": envelope_id, "type": "events_api", "payload": {"event": payload}}
    await connection.send_text(json.dumps(envelope, separators=(",", ":")))
    await asyncio.wait_for(acknowledged.wait(), _ACK_TIMEOUT_SECONDS)
    del self._acks[envelope_id]


def _first_query(request: proxy.Request, name: str, default: str) -> str:
  return request.query.get(name, [default])[0]
