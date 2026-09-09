"""Versioned static assets serve as content-addressed responses.

Every template-referenced asset URL carries ?v=<static_asset_version>, the
server's pinned runtime git version: the token moves only at a restart and a
restart changes the URL. Such responses carry an immutable Cache-Control so
the browser skips the per-asset revalidation round trip on later page loads;
a request without a version parameter keeps default caching because its URL
can outlive its content.
"""

import asyncio
from typing import Any

import server


def _drive(url: str) -> dict[str, str]:
  path, _, qs = url.partition("?")
  headers: dict[str, str] = {}
  scope: dict[str, Any] = {
      "type": "http",
      "asgi": {"version": "3.0", "spec_version": "2.3"},
      "http_version": "1.1",
      "method": "GET",
      "scheme": "http",
      "path": path,
      "raw_path": url.encode(),
      "query_string": qs.encode(),
      "root_path": "",
      "headers": [(b"host", b"t")],
      "client": ("t", 1),
      "server": ("t", 80),
  }

  async def receive() -> dict[str, Any]:
    return {"type": "http.request", "body": b"", "more_body": False}

  async def send(message: dict[str, Any]) -> None:
    if message["type"] == "http.response.start":
      for name, value in message["headers"]:
        headers[name.decode().lower()] = value.decode()

  asyncio.run(server.app(scope, receive, send))
  return headers


def test_versioned_asset_serves_immutable_cache_control() -> None:
  headers = _drive("/static/js/usage.js?v=abc123")
  assert headers["cache-control"] == "public, max-age=31536000, immutable"
  assert headers["etag"]


def test_unversioned_asset_keeps_default_caching() -> None:
  assert "cache-control" not in _drive("/static/js/usage.js")
  assert "cache-control" not in _drive("/static/js/usage.js?v=")


def test_failed_lookup_never_serves_cache_control() -> None:
  assert "cache-control" not in _drive("/static/js/missing.js?v=abc123")
