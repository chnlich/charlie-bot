"""_RequestLogMiddleware tests: one structlog http_request event per HTTP response.

The middleware is driven with fake ASGI scopes/apps directly — no uvicorn boot,
no FastAPI app — so each test pins exactly one log event against one response.
"""

from __future__ import annotations

from typing import Any

import pytest
from starlette.types import Message, Receive, Scope, Send
from structlog.testing import capture_logs

from server import _RequestLogMiddleware


def _http_scope(
    method: str = "GET",
    path: str = "/api/sessions/",
    query_string: bytes = b"",
    client: tuple[str, int] | None = ("127.0.0.1", 12345),
) -> Scope:
  scope: Scope = {
      "type": "http",
      "method": method,
      "path": path,
      "query_string": query_string,
      "headers": [],
  }
  if client is not None:
    scope["client"] = client
  return scope


async def _receive() -> Message:
  return {"type": "http.request", "body": b"", "more_body": False}


async def _send(message: Message) -> None:
  return None


async def _ok_app(scope: Scope, receive: Receive, send: Send) -> None:
  await send({"type": "http.response.start", "status": 200, "headers": []})
  await send({"type": "http.response.body", "body": b"ok"})


async def _unauthorized_app(scope: Scope, receive: Receive, send: Send) -> None:
  await send({"type": "http.response.start", "status": 401, "headers": []})
  await send({"type": "http.response.body", "body": b'{"detail": "Unauthorized"}'})


async def _raising_app(scope: Scope, receive: Receive, send: Send) -> None:
  raise ValueError("boom")


async def _streaming_app(scope: Scope, receive: Receive, send: Send) -> None:
  await send({"type": "http.response.start", "status": 200, "headers": []})
  await send({"type": "http.response.body", "body": b"chunk-1", "more_body": True})
  await send({"type": "http.response.body", "body": b"chunk-2", "more_body": False})


async def _drive(app: Any, scope: Scope) -> None:
  await _RequestLogMiddleware(app)(scope, _receive, _send)


@pytest.mark.asyncio
async def test_200_response_logs_one_event_with_five_fields() -> None:
  with capture_logs() as events:
    await _drive(_ok_app, _http_scope())

  assert len(events) == 1
  event = events[0]
  # log_level is capture_logs' own addition; the middleware itself contributes
  # the event name plus exactly the five contract fields.
  assert set(event) == {"event", "log_level", "method", "path", "status", "duration_ms", "client"}
  assert event["event"] == "http_request"
  assert event["method"] == "GET"
  assert event["path"] == "/api/sessions/"
  assert event["status"] == 200
  assert isinstance(event["status"], int)
  assert isinstance(event["duration_ms"], int)
  assert event["client"] == "127.0.0.1"


@pytest.mark.asyncio
async def test_401_response_logs_one_event_with_status_401() -> None:
  with capture_logs() as events:
    await _drive(_unauthorized_app, _http_scope(path="/api/chat"))

  assert len(events) == 1
  assert events[0]["event"] == "http_request"
  assert events[0]["status"] == 401


@pytest.mark.asyncio
async def test_inner_app_exception_logs_500_and_reraises() -> None:
  with pytest.raises(ValueError, match="boom"):
    with capture_logs() as events:
      await _drive(_raising_app, _http_scope(method="POST"))

  assert len(events) == 1
  event = events[0]
  assert event["event"] == "http_request"
  assert event["status"] == 500
  assert event["error"] == "ValueError"
  assert event["method"] == "POST"


@pytest.mark.asyncio
async def test_path_excludes_query_string() -> None:
  with capture_logs() as events:
    await _drive(_ok_app, _http_scope(path="/ws/terminal", query_string=b"token=secret-token"))

  assert len(events) == 1
  assert events[0]["path"] == "/ws/terminal"
  assert "secret-token" not in str(events[0])


@pytest.mark.asyncio
async def test_websocket_scope_passes_through_without_logging() -> None:
  seen: list[Scope] = []

  async def ws_app(scope: Scope, receive: Receive, send: Send) -> None:
    seen.append(scope)

  scope: Scope = {"type": "websocket", "path": "/ws/sessions/s1", "headers": []}
  with capture_logs() as events:
    await _RequestLogMiddleware(ws_app)(scope, _receive, _send)

  assert seen == [scope]
  assert events == []


@pytest.mark.asyncio
async def test_streaming_body_logs_exactly_one_event() -> None:
  with capture_logs() as events:
    await _drive(_streaming_app, _http_scope(path="/files/big.bin"))

  assert len(events) == 1
  assert events[0]["event"] == "http_request"
  assert events[0]["status"] == 200


@pytest.mark.asyncio
async def test_missing_client_falls_back_to_dash() -> None:
  with capture_logs() as events:
    await _drive(_ok_app, _http_scope(client=None))

  assert len(events) == 1
  assert events[0]["client"] == "-"
