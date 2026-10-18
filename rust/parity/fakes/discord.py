"""Fake Discord v10 REST and gateway servers; seeds are bot_user_id, application_flags, guilds, channels, messages and failures."""

import json
from collections.abc import Callable
from typing import Any

from . import proxy, recorder

_FIRST_SNOWFLAKE = 900000000000000100
_BOT_USER_ID = "900000000000000001"
_GATEWAY_URL = "wss://gateway.discord.gg"
_MESSAGE_CONTENT_INTENT = 1 << 18


def validate_seed(seed: dict) -> None:
  allowed = {"bot_user_id", "application_flags", "guilds", "channels", "messages", "failures"}
  unknown = set(seed) - allowed
  if unknown:
    raise ValueError(f"unknown seed keys {sorted(unknown)}")
  if "bot_user_id" in seed and (not isinstance(seed["bot_user_id"], str) or not seed["bot_user_id"]):
    raise ValueError("bot_user_id must be a non-empty string")
  flags = seed.get("application_flags", _MESSAGE_CONTENT_INTENT)
  if not isinstance(flags, int) or isinstance(flags, bool) or flags < 0:
    raise ValueError("application_flags must be a non-negative integer")
  if "guilds" in seed and (not isinstance(seed["guilds"], list) or
                           any(not isinstance(guild, dict) for guild in seed["guilds"])):
    raise ValueError("guilds must be a list of guild mappings")
  for name in ("channels", "messages"):
    value = seed.get(name, {})
    if not isinstance(value, dict) or any(not isinstance(key, str) for key in value):
      raise ValueError(f"{name} must map ids to mappings or message lists")
  for channel, messages in seed.get("messages", {}).items():
    if not isinstance(messages, list) or any(not isinstance(message, dict) for message in messages):
      raise ValueError(f"messages[{channel!r}] must be a list of message mappings")
  _validate_failures(seed.get("failures", {}))


def _validate_failures(failures: Any) -> None:
  if not isinstance(failures, dict):
    raise ValueError("failures must map API paths to HTTP error statuses")
  for path, status in failures.items():
    if not isinstance(path, str) or not path.startswith("/"):
      raise ValueError("failure keys must be absolute API paths")
    if not isinstance(status, int) or isinstance(status, bool) or not 400 <= status <= 599:
      raise ValueError(f"failure status for {path!r} must be an HTTP error status")


def _json_response(value: Any, status: int = 200) -> proxy.HTTPResponse:
  return proxy.HTTPResponse(
      status, {"Content-Type": "application/json"},
      json.dumps(value, separators=(",", ":")).encode())


class Fake:
  """The deterministic Discord service used by scenarios that declare ``fakes.discord``."""

  def __init__(self, seed: dict, traffic: recorder.Recorder, mark_ready: Callable[[], None]) -> None:
    validate_seed(seed)
    self.bot_user_id = seed.get("bot_user_id", _BOT_USER_ID)
    self.application_flags = seed.get("application_flags", _MESSAGE_CONTENT_INTENT)
    self.guilds = seed.get("guilds", [])
    self.channels = seed.get("channels", {})
    self.messages = seed.get("messages", {})
    self.failures = seed.get("failures", {})
    self.traffic = traffic
    self.mark_ready = mark_ready
    self._message_counter = _FIRST_SNOWFLAKE
    self._sequence = 0
    self._connection: proxy.WebSocketConnection | None = None

  def http(self, request: proxy.Request) -> proxy.HTTPResponse:
    if request.path in self.failures:
      return _json_response({"code": 9000, "message": "seeded failure"}, self.failures[request.path])
    if request.path == "/api/v10/gateway/bot":
      return _json_response({"url": _GATEWAY_URL})
    if request.path == "/api/v10/applications/@me":
      return _json_response({"id": self.bot_user_id, "flags": self.application_flags})
    if request.path == "/api/v10/users/@me":
      return _json_response({"id": self.bot_user_id, "username": "parity-bot", "bot": True})
    if request.path == "/api/v10/users/@me/guilds":
      return _json_response(self.guilds)
    channel_id = _channel_id(request.path)
    if request.method == "GET" and channel_id is not None and request.path.endswith("/messages"):
      return _json_response(self.messages.get(channel_id, []))
    if request.method == "GET" and channel_id is not None and request.path == f"/api/v10/channels/{channel_id}":
      return _json_response(
          self.channels.get(channel_id, {
              "id": channel_id,
              "type": 0,
              "name": "parity-room",
              "parent_id": None,
          }))
    if request.method in ("PUT", "DELETE") and "/reactions/" in request.path:
      return proxy.HTTPResponse(204, {}, b"")
    if request.path.endswith("/threads") and request.method == "POST":
      return _json_response({"id": self._next_snowflake(), "type": 11, "name": "parity-thread"}, 201)
    if request.path.endswith("/messages") and request.method == "POST":
      body = request.body if isinstance(request.body, dict) else {}
      return _json_response({"id": self._next_snowflake(), "content": body.get("content", "")}, 200)
    if "/messages/" in request.path and request.method == "GET":
      return _json_response({"id": request.path.rsplit("/", 1)[-1], "content": ""})
    return _json_response({})

  async def websocket(self, connection: proxy.WebSocketConnection) -> None:
    self._connection = connection
    await connection.send_text(json.dumps({"op": 10, "d": {"heartbeat_interval": 60000}}, separators=(",", ":")))
    try:
      while (raw := await connection.recv()) is not None:
        if not isinstance(raw, str):
          raise ValueError("Discord gateway received a binary frame")
        payload = json.loads(raw)
        if payload.get("op") == 1:
          await connection.send_text(json.dumps({"op": 11, "d": None}, separators=(",", ":")))
        elif payload.get("op") == 2:
          self._sequence += 1
          ready = {
              "op": 0,
              "s": self._sequence,
              "t": "READY",
              "d": {
                  "user": {
                      "id": self.bot_user_id,
                      "username": "parity-bot",
                      "bot": True
                  }
              },
          }
          await connection.send_text(json.dumps(ready, separators=(",", ":")))
          self.mark_ready()
    finally:
      if self._connection is connection:
        self._connection = None

  async def push(self, action: str, payload: dict) -> None:
    if action != "dispatch":
      raise ValueError(f"unknown Discord fake action {action!r}")
    connection = self._connection
    if connection is None:
      raise RuntimeError("Discord gateway is not connected")
    self._sequence += 1
    dispatch = {"op": 0, "s": self._sequence, "t": payload["t"], "d": payload["d"]}
    await connection.send_text(json.dumps(dispatch, separators=(",", ":")))

  def _next_snowflake(self) -> str:
    self._message_counter += 1
    return str(self._message_counter)


def _channel_id(path: str) -> str | None:
  parts = path.split("/")
  if len(parts) >= 5 and parts[:3] == ["", "api", "v10"] and parts[3] == "channels":
    return parts[4]
  return None
