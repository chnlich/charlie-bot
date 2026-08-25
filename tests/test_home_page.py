"""Tests for the /home page: config-driven cards, live probes, and auth gate."""

from __future__ import annotations

import re
import socket
from pathlib import Path
from types import SimpleNamespace
from urllib.parse import urlparse

import pytest
from conftest import make_page_request
from starlette.requests import Request
from starlette.responses import Response

from src.api import auth, pages
from src.api.auth import AuthMiddleware
from src.core.config import CharlieBotConfig, HomeService


def _cfg(home: Path, services: list[dict[str, str]]) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=home,
      home_services=[HomeService(**service) for service in services],
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
  assert _external_hrefs(empty.body.decode("utf-8")) == []

  services = [
      {"name": f"svc-{i}", "description": f"Service {i}", "url": f"https://127.0.0.1:1/svc{i}"}
      for i in range(3)
  ]
  full = await pages.home_page(make_page_request("/home"), _cfg(tmp_path / "h1", services))
  assert full.status_code == 200
  assert _external_hrefs(full.body.decode("utf-8")) == [s["url"] for s in services]


@pytest.mark.asyncio
async def test_home_probe_reflects_a_real_port(tmp_path: Path) -> None:
  """A live socket renders up; closing it renders down without changing the other card."""
  listener = socket.socket()
  listener.bind(("127.0.0.1", 0))
  listener.listen(1)
  port = listener.getsockname()[1]
  services = [
      {"name": "listener-svc", "description": "TCP listener for the test", "url": f"http://127.0.0.1:{port}/api"},
      {"name": "stub-svc", "description": "Refuses every connect", "url": "http://127.0.0.1:1/"},
  ]
  cfg = _cfg(tmp_path / "h", services)

  up_body = (await pages.home_page(make_page_request("/home"), cfg)).body.decode("utf-8")
  assert _external_statuses(up_body) == ["up", "down"]

  listener.close()
  down_body = (await pages.home_page(make_page_request("/home"), cfg)).body.decode("utf-8")
  assert _external_statuses(down_body) == ["down", "down"]
  # Neither the card set nor what the cards link and say may change between renders.
  assert _external_hrefs(up_body) == _external_hrefs(down_body) == [s["url"] for s in services]
  for service in services:
    for field in ("name", "description"):
      assert up_body.count(service[field]) == down_body.count(service[field]) == 1


@pytest.mark.asyncio
async def test_home_badge_never_labels_a_link_that_cannot_open(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """For each external card, the host:port in its href equals the host:port the probe used."""
  probed: list[str] = []
  real_probe = pages._probe_home_service

  def recording_probe(url: str) -> bool:
    probed.append(url)
    return real_probe(url)

  monkeypatch.setattr(pages, "_probe_home_service", recording_probe)

  services = [
      {"name": "a", "description": "Service a", "url": "https://example.internal:8443/x"},
      {"name": "b", "description": "Service b", "url": "http://127.0.0.1:1/y"},
  ]
  body = (await pages.home_page(make_page_request("/home"), _cfg(tmp_path / "h", services))).body.decode("utf-8")

  hrefs = _external_hrefs(body)
  assert len(hrefs) == len(services) == len(probed)
  for href, probe_url in zip(hrefs, probed):
    href_parts = urlparse(href)
    probe_parts = urlparse(probe_url)
    assert (href_parts.hostname, href_parts.port) == (probe_parts.hostname, probe_parts.port)


@pytest.mark.asyncio
async def test_home_bad_url_entry_does_not_break_the_page(tmp_path: Path) -> None:
  """A service whose url has no parseable host renders down; the page and other cards survive."""
  services = [
      {"name": "broken", "description": "No host", "url": "not-a-url"},
      {"name": "fine", "description": "Has a host", "url": "https://127.0.0.1:1/"},
  ]
  response = await pages.home_page(make_page_request("/home"), _cfg(tmp_path / "h", services))
  assert response.status_code == 200
  body = response.body.decode("utf-8")
  assert _external_hrefs(body) == [s["url"] for s in services]
  assert _external_statuses(body) == ["down", "down"]
  for service in services:
    assert service["name"] in body


@pytest.mark.asyncio
async def test_home_html_navigation_requires_auth(monkeypatch: pytest.MonkeyPatch) -> None:
  """An unauthenticated browser navigation gets 401 with the HTML login page, not bare JSON."""
  assert "/home" not in auth._PUBLIC_PATHS
  assert not any("/home".startswith(prefix) for prefix in auth._PUBLIC_PREFIXES)

  monkeypatch.setattr(auth, "get_config", lambda: SimpleNamespace(charliebot_access_key="secret"))

  async def _call_next(request: Request) -> Response:
    return Response("OK", status_code=200)

  raw_headers = [(b"accept", b"text/html")]
  scope = {"type": "http", "method": "GET", "path": "/home", "headers": raw_headers, "query_string": b""}
  resp = await AuthMiddleware(app=None).dispatch(Request(scope), _call_next)
  assert resp.status_code == 401
  assert resp.media_type == "text/html"
  assert "<form" in resp.body.decode()


@pytest.mark.asyncio
async def test_home_viewers_are_not_dead_links(tmp_path: Path) -> None:
  """The page names the three viewers with what each needs, but links to none of them."""
  body = (await pages.home_page(make_page_request("/home"), _cfg(tmp_path / "h", []))).body.decode("utf-8")
  for dead in ('href="/perfetto', 'href="/ncu', 'href="/sessions/'):
    assert dead not in body
  for name in ("Perfetto", "Nsight Compute", "Session events"):
    assert name in body
  for need in ("trace file", ".ncu-rep", "session"):
    assert need in body


@pytest.mark.asyncio
async def test_home_lists_the_four_server_destinations(tmp_path: Path) -> None:
  body = (await pages.home_page(make_page_request("/home"), _cfg(tmp_path / "h", []))).body.decode("utf-8")
  assert 'href="/"' in body
  assert 'href="/token-usage"' in body
  assert 'href="/diff"' in body
  assert 'href="/files/"' in body
