"""_RequestLogMiddleware tests: one access log line per HTTP response.

The middleware is driven with fake ASGI scopes/apps directly — no uvicorn boot,
no FastAPI app — so each test pins exactly one log line against one response.
The line renders through ``log_http_request_line``; the field contract pins the
dict that helper receives, and the render contract pins the helper's bytes
against the configured structlog chain's own render of the same event.
"""

from __future__ import annotations

import contextlib
import io
from typing import Any

import pytest
import structlog
from starlette.types import Message, Receive, Scope, Send

from server import _RequestLogMiddleware


@pytest.fixture
def logged_fields(monkeypatch: pytest.MonkeyPatch) -> list[dict[str, object]]:
  """The fields dicts the middleware hands to the access-line renderer."""
  captured: list[dict[str, object]] = []
  monkeypatch.setattr("server.log_http_request_line", lambda **fields: captured.append(fields))
  return captured


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


def _rendered_fields(line: str) -> dict[str, str]:
  """The key=value fields of one rendered access line, after the level column."""
  fields_part = line.rstrip("\n")[line.index("]") + 1:].split(None, 1)[1]
  return dict(token.split("=", 1) for token in fields_part.split(" "))


@pytest.mark.asyncio
async def test_200_response_logs_one_line_with_five_fields(logged_fields: list[dict[str, object]]) -> None:
  await _drive(_ok_app, _http_scope())

  assert len(logged_fields) == 1
  fields = logged_fields[0]
  # The middleware passes every field explicitly — error=None on the happy
  # path — and the renderer omits a None error, so the line carries exactly
  # the five contract fields (pinned on the rendered bytes downstream).
  assert set(fields) == {"method", "path", "status", "duration_ms", "client", "error"}
  assert fields["error"] is None
  assert fields["method"] == "GET"
  assert fields["path"] == "/api/sessions/"
  assert fields["status"] == 200
  assert isinstance(fields["status"], int)
  assert isinstance(fields["duration_ms"], int)
  assert fields["client"] == "127.0.0.1"


@pytest.mark.asyncio
async def test_401_response_logs_one_line_with_status_401(logged_fields: list[dict[str, object]]) -> None:
  await _drive(_unauthorized_app, _http_scope(path="/api/chat"))

  assert len(logged_fields) == 1
  assert logged_fields[0]["status"] == 401


@pytest.mark.asyncio
async def test_inner_app_exception_logs_500_and_reraises(logged_fields: list[dict[str, object]]) -> None:
  with pytest.raises(ValueError, match="boom"):
    await _drive(_raising_app, _http_scope(method="POST"))

  assert len(logged_fields) == 1
  fields = logged_fields[0]
  assert fields["status"] == 500
  assert fields["error"] == "ValueError"
  assert fields["method"] == "POST"


@pytest.mark.asyncio
async def test_path_excludes_query_string(logged_fields: list[dict[str, object]]) -> None:
  await _drive(_ok_app, _http_scope(path="/ws/terminal", query_string=b"token=secret-token"))

  assert len(logged_fields) == 1
  assert logged_fields[0]["path"] == "/ws/terminal"
  assert "secret-token" not in str(logged_fields[0])


@pytest.mark.asyncio
async def test_websocket_scope_passes_through_without_logging() -> None:
  seen: list[Scope] = []

  async def ws_app(scope: Scope, receive: Receive, send: Send) -> None:
    seen.append(scope)

  scope: Scope = {"type": "websocket", "path": "/ws/sessions/s1", "headers": []}
  sink = io.StringIO()
  with contextlib.redirect_stdout(sink):
    await _RequestLogMiddleware(ws_app)(scope, _receive, _send)

  assert seen == [scope]
  assert sink.getvalue() == ""


@pytest.mark.asyncio
async def test_streaming_body_logs_exactly_one_line(logged_fields: list[dict[str, object]]) -> None:
  await _drive(_streaming_app, _http_scope(path="/absolute_filepath/big.bin"))

  assert len(logged_fields) == 1
  assert logged_fields[0]["status"] == 200


@pytest.mark.asyncio
async def test_missing_client_falls_back_to_dash(logged_fields: list[dict[str, object]]) -> None:
  await _drive(_ok_app, _http_scope(client=None))

  assert len(logged_fields) == 1
  assert logged_fields[0]["client"] == "-"


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("app", "path", "expected_status"), [
        (_ok_app, "/api/sessions/", 200),
        (_unauthorized_app, "/api/chat", 401),
        (_streaming_app, "/absolute_filepath/big.bin", 200),
    ],
    ids=["200", "401", "streaming"])
async def test_rendered_line_matches_the_configured_chain(app: Any, path: str, expected_status: int) -> None:
  """The direct render's bytes equal the structlog chain's render of the same event."""
  sink = io.StringIO()
  with contextlib.redirect_stdout(sink):
    await _drive(app, _http_scope(path=path))
  middleware_line = sink.getvalue()

  rendered = _rendered_fields(middleware_line)
  assert rendered["status"] == str(expected_status)
  # The parsed values ride the chain render unchanged: ints and bare strings
  # render identically either way, so the parsed line rebuilds the event.
  chain_sink = io.StringIO()
  with contextlib.redirect_stdout(chain_sink):
    structlog.get_logger().info("http_request", **rendered)
  chain_line = chain_sink.getvalue()

  # Both stamps are local wall time in the same format; the comparison starts
  # at the level column, the first renderer-shaped part.
  assert middleware_line[middleware_line.index("["):] == chain_line[chain_line.index("["):]
