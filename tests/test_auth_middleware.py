"""Tests for the access-key auth middleware."""

import json
from types import SimpleNamespace

import pytest
from starlette.requests import Request
from starlette.responses import Response

from src.api import auth
from src.api.auth import AuthMiddleware


def _request(
    method: str = "GET",
    path: str = "/api/chat",
    headers: dict[str, str] | None = None,
    cookies: dict[str, str] | None = None,
) -> Request:
  raw_headers: list[tuple[bytes, bytes]] = []
  for name, value in (headers or {}).items():
    raw_headers.append((name.lower().encode(), value.encode()))
  if cookies:
    cookie_str = "; ".join(f"{name}={value}" for name, value in cookies.items())
    raw_headers.append((b"cookie", cookie_str.encode()))
  scope = {
      "type": "http",
      "method": method,
      "path": path,
      "headers": raw_headers,
      "query_string": b"",
  }
  return Request(scope)


async def _call_next(request: Request) -> Response:
  return Response("OK", status_code=200)


def _middleware(monkeypatch: pytest.MonkeyPatch, key: str) -> AuthMiddleware:
  monkeypatch.setattr(auth, "get_config", lambda: SimpleNamespace(charliebot_access_key=key))
  return AuthMiddleware(app=None)


@pytest.mark.asyncio
async def test_empty_key_is_noop(monkeypatch: pytest.MonkeyPatch) -> None:
  mw = _middleware(monkeypatch, key="")
  resp = await mw.dispatch(_request(headers={"accept": "application/json"}), _call_next)
  assert resp.status_code == 200


@pytest.mark.asyncio
async def test_cookie_accepted(monkeypatch: pytest.MonkeyPatch) -> None:
  mw = _middleware(monkeypatch, key="secret")
  resp = await mw.dispatch(_request(cookies={"charliebot_access_key": "secret"}), _call_next)
  assert resp.status_code == 200


@pytest.mark.asyncio
async def test_bearer_header_accepted(monkeypatch: pytest.MonkeyPatch) -> None:
  mw = _middleware(monkeypatch, key="secret")
  resp = await mw.dispatch(_request(headers={"authorization": "Bearer secret"}), _call_next)
  assert resp.status_code == 200


@pytest.mark.asyncio
async def test_html_navigation_returns_login_page(monkeypatch: pytest.MonkeyPatch) -> None:
  mw = _middleware(monkeypatch, key="secret")
  resp = await mw.dispatch(_request(headers={"accept": "text/html"}), _call_next)
  assert resp.status_code == 401
  assert resp.media_type == "text/html"
  body = resp.body.decode()
  assert "<form" in body
  assert "charliebot_access_key" in body


@pytest.mark.asyncio
async def test_api_request_returns_json_401(monkeypatch: pytest.MonkeyPatch) -> None:
  mw = _middleware(monkeypatch, key="secret")
  resp = await mw.dispatch(_request(headers={"accept": "application/json"}), _call_next)
  assert resp.status_code == 401
  assert resp.media_type == "application/json"
  assert json.loads(resp.body) == {"detail": "Unauthorized"}


@pytest.mark.asyncio
async def test_invalid_cookie_returns_json_401(monkeypatch: pytest.MonkeyPatch) -> None:
  mw = _middleware(monkeypatch, key="secret")
  resp = await mw.dispatch(
      _request(headers={"accept": "application/json"}, cookies={"charliebot_access_key": "wrong"}),
      _call_next,
  )
  assert resp.status_code == 401
  assert resp.media_type == "application/json"


@pytest.mark.asyncio
async def test_non_get_html_accept_still_json_401(monkeypatch: pytest.MonkeyPatch) -> None:
  # Only GET navigations get the HTML login page; a POST with text/html does not.
  mw = _middleware(monkeypatch, key="secret")
  resp = await mw.dispatch(_request(method="POST", headers={"accept": "text/html"}), _call_next)
  assert resp.status_code == 401
  assert resp.media_type == "application/json"


@pytest.mark.asyncio
async def test_public_path_passes_without_credential(monkeypatch: pytest.MonkeyPatch) -> None:
  mw = _middleware(monkeypatch, key="secret")
  resp = await mw.dispatch(_request(path="/api/auth/status", headers={"accept": "text/html"}), _call_next)
  assert resp.status_code == 200


@pytest.mark.asyncio
@pytest.mark.parametrize("path", ["/perfetto", "/ncu"])
async def test_viewer_pages_public_without_credential(monkeypatch: pytest.MonkeyPatch, path: str) -> None:
  # The read-only viewer shells are reachable without any Bearer or cookie.
  mw = _middleware(monkeypatch, key="secret")
  resp = await mw.dispatch(_request(path=path, headers={"accept": "text/html"}), _call_next)
  assert resp.status_code == 200


@pytest.mark.asyncio
async def test_viewer_pages_match_exact_path_only(monkeypatch: pytest.MonkeyPatch) -> None:
  # Exact-path matching: query strings are excluded from request.url.path so
  # "/perfetto?trace=..." resolves to "/perfetto", but sibling paths stay gated.
  mw = _middleware(monkeypatch, key="secret")
  resp = await mw.dispatch(_request(path="/perfetto/secret", headers={"accept": "application/json"}), _call_next)
  assert resp.status_code == 401


@pytest.mark.asyncio
async def test_gated_route_still_requires_credential(monkeypatch: pytest.MonkeyPatch) -> None:
  # A representative gated route must remain 401 without a credential.
  mw = _middleware(monkeypatch, key="secret")
  resp = await mw.dispatch(_request(path="/api/sessions", headers={"accept": "application/json"}), _call_next)
  assert resp.status_code == 401
