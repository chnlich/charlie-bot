"""Versioned static assets serve as content-addressed responses.

Every template-referenced asset URL carries ?v=<static_asset_version> — the
runtime git version plus the served tree's content digest, refreshed per page
render, so the token tracks the bytes the URL serves even when a working-tree
edit lands between restarts. Such responses carry an immutable Cache-Control
so the browser skips the per-asset revalidation round trip on later page
loads; a request without a version parameter keeps default caching because
its URL can outlive its content.
"""

import asyncio
from pathlib import Path
from typing import Any

import pytest
from conftest import make_http_scope

import server
from src.api import pages


def _drive(url: str) -> dict[str, str]:
  headers: dict[str, str] = {}
  scope = make_http_scope(url, headers=[(b"host", b"t")])

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


def test_asset_token_tracks_the_served_tree(tmp_path: Path) -> None:
  static = tmp_path / "web" / "static"
  static.mkdir(parents=True)
  (static / "a.js").write_text("one", encoding="utf-8")
  with pytest.MonkeyPatch.context() as mp:
    mp.setattr(pages, "_REPO_ROOT", tmp_path)
    pages._ASSET_DIGEST_STATE.update(sig=(), digests={}, digest="")
    first = pages._asset_tree_digest()
    again = pages._asset_tree_digest()
    assert first == again  # an unchanged walk serves the memoized digest
    (static / "a.js").write_text("two", encoding="utf-8")
    second = pages._asset_tree_digest()
    assert second != first  # a tree edit moves the token on the next render
  pages._ASSET_DIGEST_STATE.update(sig=(), digests={}, digest="")


def test_asset_token_composes_git_version_and_digest() -> None:
  version = pages._static_asset_version()
  assert version.endswith(f"-{pages._ASSET_DIGEST_STATE['digest']}")
  assert version.startswith(pages._RUNTIME_GIT_VERSION.replace(" · ", "-").replace(" ", "-"))
