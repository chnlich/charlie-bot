"""Tests for the access-key auth middleware."""

import json
from types import SimpleNamespace

import pytest
from conftest import (
    _ok_asgi_downstream,
    asgi_downstream_called,
    run_through_asgi_middleware,
)

from src.api import auth
from src.api.auth import AuthMiddleware


def _scope(
    method: str = "GET",
    path: str = "/api/chat",
    headers: dict[str, str] | None = None,
    cookies: dict[str, str] | None = None,
) -> dict:
  raw_headers: list[tuple[bytes, bytes]] = []
  for name, value in (headers or {}).items():
    raw_headers.append((name.lower().encode(), value.encode()))
  if cookies:
    cookie_str = "; ".join(f"{name}={value}" for name, value in cookies.items())
    raw_headers.append((b"cookie", cookie_str.encode()))
  return {
      "type": "http",
      "method": method,
      "path": path,
      "headers": raw_headers,
      "query_string": b"",
  }


def _middleware(monkeypatch: pytest.MonkeyPatch, key: str) -> AuthMiddleware:
  monkeypatch.setattr(auth, "get_config", lambda: SimpleNamespace(charliebot_access_key=key))
  return AuthMiddleware(app=_ok_asgi_downstream)


def _response(sent: list[dict]) -> tuple[int, str, str]:
  """Flatten the ASGI messages into (status, content-type, body)."""
  start = next(m for m in sent if m["type"] == "http.response.start")
  headers = dict(start["headers"])
  body = b"".join(m.get("body", b"") for m in sent if m["type"] == "http.response.body")
  return start["status"], headers.get(b"content-type", b"").decode(), body.decode()


@pytest.mark.asyncio
async def test_empty_key_is_noop(monkeypatch: pytest.MonkeyPatch) -> None:
  mw = _middleware(monkeypatch, key="")
  sent = await run_through_asgi_middleware(mw, _scope(headers={"accept": "application/json"}))
  status, _, _ = _response(sent)
  assert status == 200
  assert asgi_downstream_called()


@pytest.mark.asyncio
async def test_cookie_accepted(monkeypatch: pytest.MonkeyPatch) -> None:
  mw = _middleware(monkeypatch, key="secret")
  sent = await run_through_asgi_middleware(mw, _scope(cookies={"charliebot_access_key": "secret"}))
  status, _, _ = _response(sent)
  assert status == 200
  assert asgi_downstream_called()


@pytest.mark.asyncio
async def test_bearer_header_accepted(monkeypatch: pytest.MonkeyPatch) -> None:
  mw = _middleware(monkeypatch, key="secret")
  sent = await run_through_asgi_middleware(mw, _scope(headers={"authorization": "Bearer secret"}))
  status, _, _ = _response(sent)
  assert status == 200
  assert asgi_downstream_called()


@pytest.mark.asyncio
async def test_html_navigation_returns_login_page(monkeypatch: pytest.MonkeyPatch) -> None:
  mw = _middleware(monkeypatch, key="secret")
  sent = await run_through_asgi_middleware(mw, _scope(headers={"accept": "text/html"}))
  status, content_type, body = _response(sent)
  assert status == 401
  assert "text/html" in content_type
  assert "<form" in body
  assert "charliebot_access_key" in body


@pytest.mark.asyncio
async def test_api_request_returns_json_401(monkeypatch: pytest.MonkeyPatch) -> None:
  mw = _middleware(monkeypatch, key="secret")
  sent = await run_through_asgi_middleware(mw, _scope(headers={"accept": "application/json"}))
  status, content_type, body = _response(sent)
  assert status == 401
  assert content_type == "application/json"
  assert json.loads(body) == {"detail": "Unauthorized"}


@pytest.mark.asyncio
async def test_invalid_cookie_returns_json_401(monkeypatch: pytest.MonkeyPatch) -> None:
  mw = _middleware(monkeypatch, key="secret")
  sent = await run_through_asgi_middleware(
      mw, _scope(headers={"accept": "application/json"}, cookies={"charliebot_access_key": "wrong"}))
  status, content_type, _ = _response(sent)
  assert status == 401
  assert content_type == "application/json"


@pytest.mark.asyncio
async def test_non_get_html_accept_still_json_401(monkeypatch: pytest.MonkeyPatch) -> None:
  # Only GET navigations get the HTML login page; a POST with text/html does not.
  mw = _middleware(monkeypatch, key="secret")
  sent = await run_through_asgi_middleware(mw, _scope(method="POST", headers={"accept": "text/html"}))
  status, content_type, _ = _response(sent)
  assert status == 401
  assert content_type == "application/json"


@pytest.mark.asyncio
async def test_public_path_passes_without_credential(monkeypatch: pytest.MonkeyPatch) -> None:
  mw = _middleware(monkeypatch, key="secret")
  sent = await run_through_asgi_middleware(mw, _scope(path="/api/auth/status", headers={"accept": "text/html"}))
  status, _, _ = _response(sent)
  assert status == 200
  assert asgi_downstream_called()


@pytest.mark.asyncio
@pytest.mark.parametrize("path", ["/perfetto", "/perfetto/merged", "/ncu"])
async def test_viewer_pages_public_without_credential(monkeypatch: pytest.MonkeyPatch, path: str) -> None:
  # The read-only viewer shells are reachable without any Bearer or cookie.
  mw = _middleware(monkeypatch, key="secret")
  sent = await run_through_asgi_middleware(mw, _scope(path=path, headers={"accept": "text/html"}))
  status, _, _ = _response(sent)
  assert status == 200
  assert asgi_downstream_called()


@pytest.mark.asyncio
async def test_viewer_pages_match_exact_path_only(monkeypatch: pytest.MonkeyPatch) -> None:
  # Exact-path matching: query strings are excluded from request.url.path so
  # "/perfetto?trace=..." resolves to "/perfetto", but sibling paths stay gated.
  mw = _middleware(monkeypatch, key="secret")
  sent = await run_through_asgi_middleware(mw, _scope(path="/perfetto/secret", headers={"accept": "application/json"}))
  status, _, _ = _response(sent)
  assert status == 401


@pytest.mark.asyncio
async def test_gated_route_still_requires_credential(monkeypatch: pytest.MonkeyPatch) -> None:
  # A representative gated route must remain 401 without a credential.
  mw = _middleware(monkeypatch, key="secret")
  sent = await run_through_asgi_middleware(mw, _scope(path="/api/sessions", headers={"accept": "application/json"}))
  status, _, _ = _response(sent)
  assert status == 401
