"""Tests for the /home page: config-driven cards, live probes, and auth gate."""

from __future__ import annotations

import re
from pathlib import Path

import pytest
from conftest import (
    _ok_asgi_downstream,
    asgi_response,
    make_page_request,
    run_through_asgi_middleware,
    stub_credentials,
)

from src.api import auth, pages
from src.api.auth import AuthMiddleware
from src.core.config import CharlieBotConfig, HomeService


def _cfg(home: Path, services: list[dict[str, str]]) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=home,
      ui={"home_services": [HomeService(**service) for service in services]},
  )


def _external_hrefs(body: str) -> list[str]:
  """Extract the href of every external-service card in document order."""
  return re.findall(r'<a class="card svc-card" href="([^"]*)"', body)


def _external_statuses(body: str) -> list[str]:
  """Extract the badge text (up/down) of every external-service card in document order."""
  return re.findall(r'class="badge badge-(up|down)', body)


@pytest.mark.asyncio
async def test_home_page_reads_config(tmp_path: Path) -> None:
  """Zero services renders no external cards; N services render exactly N cards."""
  empty = await pages.home_page(make_page_request("/home"), _cfg(tmp_path / "h0", []))
  assert empty.status_code == 200
  assert not _external_hrefs(empty.body.decode("utf-8"))

  services = [
      {
          "name": f"svc-{i}",
          "description": f"Service {i}",
          "url": f"https://127.0.0.1:1/svc{i}"
      } for i in range(3)
  ]
  full = await pages.home_page(make_page_request("/home"), _cfg(tmp_path / "h1", services))
  assert full.status_code == 200
  assert _external_hrefs(full.body.decode("utf-8")) == [s["url"] for s in services]


@pytest.mark.asyncio
async def test_home_bad_url_entry_does_not_break_the_page(tmp_path: Path) -> None:
  """A service whose url has no parseable host renders down; the page and other cards survive."""
  services = [
      {
          "name": "broken",
          "description": "No host",
          "url": "not-a-url"
      },
      {
          "name": "fine",
          "description": "Has a host",
          "url": "https://127.0.0.1:1/"
      },
  ]
  response = await pages.home_page(make_page_request("/home"), _cfg(tmp_path / "h", services))
  assert response.status_code == 200
  body = response.body.decode("utf-8")
  assert _external_hrefs(body) == [s["url"] for s in services]
  assert _external_statuses(body) == ["down", "down"]
  for service in services:
    assert service["name"] in body


@pytest.mark.asyncio
async def test_home_html_navigation_requires_auth() -> None:
  """An unauthenticated browser navigation gets 401 with the HTML login page, not bare JSON."""
  assert "/home" not in auth._PUBLIC_PATHS
  assert not any("/home".startswith(prefix) for prefix in auth._PUBLIC_PREFIXES)

  stub_credentials({"charliebot": {"access_key": "secret"}})

  scope = {
      "type": "http",
      "method": "GET",
      "path": "/home",
      "headers": [(b"accept", b"text/html")],
      "query_string": b""
  }
  sent = await run_through_asgi_middleware(AuthMiddleware(app=_ok_asgi_downstream), scope)
  status, headers, body = asgi_response(sent)
  assert status == 401
  assert b"text/html" in headers[b"content-type"]
  assert "<form" in body.decode()
