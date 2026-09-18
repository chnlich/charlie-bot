"""The file server answers on the one canonical prefix, /absolute_filepath.

The prefix under test is read off the running app rather than written down here, so the test
follows the mounts instead of restating it. The credential gate itself lives in the auth
middleware, asserted here at the boundary the browser meets: an unauthenticated navigation
under the prefix is 401 (login page for a browser Accept, JSON otherwise).
"""

from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace
from urllib.parse import parse_qs, urlsplit

import pytest
from conftest import _ok_asgi_downstream, make_page_request, run_through_asgi_middleware, stub_credentials
from fastapi import FastAPI
from fastapi.testclient import TestClient

import server
from src.api import auth, pages
from src.api import files as files_api

ARTIFACT_SCRIPT = "artifact-comments.js"


def _mounted_prefixes() -> list[str]:
  """Every prefix `server.py` mounts the file router under, taken from the app's routes."""
  return sorted(
      route.path.removesuffix("/{path:path}")
      for route in server.app.routes
      if getattr(route, "endpoint", None) is files_api.serve_file)


PREFIXES = _mounted_prefixes()


def _client(access_key: str | None) -> TestClient:
  """A client carrying *access_key* as the charliebot_access_key cookie; None sends
  no credential. The cookie rides the client because httpx deprecates per-request
  cookies."""
  app = FastAPI()
  for prefix in PREFIXES:
    app.include_router(files_api.router, prefix=prefix)
  cookies = {"charliebot_access_key": access_key} if access_key is not None else None
  return TestClient(app, cookies=cookies)


def test_one_handler_is_mounted_under_the_canonical_prefix() -> None:
  # One path routes to the one endpoint object. The legacy /files and /file spellings are
  # mounted nowhere, so nothing but this prefix reaches the handler.
  assert PREFIXES == ["/absolute_filepath"]


@pytest.fixture
def targets(tmp_path: Path) -> dict[str, Path]:
  artifacts = tmp_path / "sessions" / "abc" / "artifacts"
  artifacts.mkdir(parents=True)
  (artifacts / "plan_01.html").write_text("<html><body><h1>Plan</h1></body></html>", encoding="utf-8")
  (tmp_path / "notes.txt").write_text("plain bytes", encoding="utf-8")
  (tmp_path / "page.html").write_text("<html><body>standalone</body></html>", encoding="utf-8")
  return {
      "artifact page": artifacts / "plan_01.html",
      "non-HTML file": tmp_path / "notes.txt",
      "plain HTML file": tmp_path / "page.html",
      "directory": tmp_path,
      "absent path": tmp_path / "gone.txt",
  }


@pytest.fixture
def isolated_config(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Own the config and credentials the file router reads: sessions root under tmp_path,
  empty access key (the gate is a no-op). Without this the route reads the host profile,
  which is not a test fixture."""
  monkeypatch.setattr(files_api, "get_config", lambda: SimpleNamespace(sessions_dir=tmp_path / "sessions"))
  stub_credentials({"charliebot": {"access_key": ""}})


def test_artifact_injection_is_anchored_on_the_sessions_root(
    targets: dict[str, Path],
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  # Injection is anchored on the configured sessions root, which the test owns here —
  # the credential plays no part in the decision any more. The client carries no
  # credential at all: the router injects unconditionally, the middleware owns the gate.
  monkeypatch.setattr(files_api, "get_config", lambda: SimpleNamespace(sessions_dir=tmp_path / "sessions"))
  stub_credentials({"charliebot": {"access_key": "secret"}})
  client = _client(None)
  # The predicate itself is unchanged: the artifact page gets the review UI, a plain page does not.
  assert ARTIFACT_SCRIPT in client.get(f"{PREFIXES[0]}{targets['artifact page']}").text
  assert ARTIFACT_SCRIPT not in client.get(f"{PREFIXES[0]}{targets['plain HTML file']}").text


def test_head_answers_the_same_status_as_get(targets: dict[str, Path], isolated_config: None) -> None:
  # The render-time probe asks with HEAD, so the marker only ever appears for a path the server
  # answers 404 for: a HEAD that came back 405 would mark nothing at all.
  client = _client(None)
  for label, target in targets.items():
    url = f"{PREFIXES[0]}{target}"
    assert client.head(url).status_code == client.get(url).status_code, label
  assert client.head(f"{PREFIXES[0]}{targets['non-HTML file']}").status_code == 200
  assert client.head(f"{PREFIXES[0]}{targets['absent path']}").status_code == 404


def test_a_non_html_file_is_served_byte_for_byte(targets: dict[str, Path], isolated_config: None) -> None:
  client = _client(None)
  target = targets["non-HTML file"]
  assert client.get(f"{PREFIXES[0]}{target}").content == target.read_bytes()


@pytest.mark.asyncio
@pytest.mark.parametrize("accept", [b"text/html", b"application/json"])
async def test_an_unauthenticated_get_under_the_prefix_is_401(accept: bytes) -> None:
  stub_credentials({"charliebot": {"access_key": "secret"}})
  scope = {
      "type": "http",
      "method": "GET",
      "path": "/absolute_filepath/tmp/trace.json",
      "headers": [(b"accept", accept)],
      "query_string": b"",
  }
  sent = await run_through_asgi_middleware(auth.AuthMiddleware(app=_ok_asgi_downstream), scope)
  start = next(m for m in sent if m["type"] == "http.response.start")
  assert start["status"] == 401
  content_type = dict(start["headers"])[b"content-type"]
  if accept == b"text/html":
    # A browser navigation gets the unlock form, not JSON.
    assert content_type.startswith(b"text/html")
    body = b"".join(m.get("body", b"") for m in sent if m["type"] == "http.response.body")
    assert b"<form" in body
  else:
    assert content_type == b"application/json"


@pytest.mark.asyncio
async def test_an_authenticated_get_under_the_prefix_reaches_the_route() -> None:
  stub_credentials({"charliebot": {"access_key": "secret"}})
  scope = {
      "type": "http",
      "method": "GET",
      "path": "/absolute_filepath/tmp/trace.json",
      "headers": [(b"accept", b"text/html"), (b"cookie", b"charliebot_access_key=secret")],
      "query_string": b"",
  }
  sent = await run_through_asgi_middleware(auth.AuthMiddleware(app=_ok_asgi_downstream), scope)
  start = next(m for m in sent if m["type"] == "http.response.start")
  assert start["status"] == 200


def test_the_legacy_files_and_file_prefixes_answer_404() -> None:
  # Hard-offline: driven through the real app (the suite's profile isolation leaves
  # the access key empty, so the middleware is a no-op here), the retired aliases
  # answer FastAPI's routing 404 — the file server never sees the request.
  client = TestClient(server.app)
  assert client.get("/files/tmp/trace.json").status_code == 404
  assert client.get("/file/tmp/trace.json").status_code == 404


@pytest.mark.parametrize("prefix", PREFIXES)
def test_a_trace_input_under_the_prefix_names_the_file(prefix: str, tmp_path: Path) -> None:
  trace = tmp_path / "rank0.json"
  url, path = pages._trace_input(f"{prefix}{trace}")
  assert path == trace
  assert url == f"{prefix}{trace}"


@pytest.mark.asyncio
@pytest.mark.parametrize("prefix", PREFIXES)
async def test_the_viewer_resolves_a_trace_under_the_prefix_to_the_same_url(
    prefix: str,
    tmp_path: Path,
) -> None:
  trace = tmp_path / "rank0.json"
  trace.write_text(json.dumps({"traceEvents": []}), encoding="utf-8")
  response = await pages.perfetto_viewer(
      make_page_request("/perfetto"),
      trace=[f"{prefix}{trace}"],
      dir_path=None,
      pattern="*.json",
      title=None,
      slim=None)

  merged_url = response.context["trace_url"]
  assert urlsplit(merged_url).path == "/perfetto/merged"
  # What the page generates is unchanged: the merge URL carries the bare absolute path.
  assert parse_qs(urlsplit(merged_url).query)["trace"] == [str(trace)]
