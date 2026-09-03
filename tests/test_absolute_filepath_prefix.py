"""The file server answers on two prefixes, and nothing about them differs but the spelling.

The prefixes under test are read off the running app rather than written down here, so the test
follows the mounts instead of restating them.
"""

from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace
from urllib.parse import parse_qs, urlsplit

import pytest
from conftest import make_page_request
from fastapi import FastAPI
from fastapi.testclient import TestClient
from starlette.requests import Request
from starlette.responses import Response

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


def _client() -> TestClient:
  app = FastAPI()
  for prefix in PREFIXES:
    app.include_router(files_api.router, prefix=prefix)
  return TestClient(app)


async def _served(request: Request) -> Response:
  return Response("served", status_code=200)


def _navigation(path: str) -> Request:
  return Request(
      {
          "type": "http",
          "method": "GET",
          "path": path,
          "headers": [(b"accept", b"text/html")],
          "query_string": b"",
      })


def test_one_handler_is_mounted_under_both_prefixes() -> None:
  # Both paths route to the same endpoint object, which is what makes path resolution, artifact
  # injection, directory listing and 404 behavior identical: there is no second copy to drift.
  assert PREFIXES == ["/absolute_filepath", "/files"]


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


def test_both_prefixes_return_the_same_status_and_bytes(targets: dict[str, Path]) -> None:
  client = _client()
  for label, target in targets.items():
    responses = [client.get(f"{prefix}{target}") for prefix in PREFIXES]
    statuses = {response.status_code for response in responses}
    bodies = {response.content for response in responses}
    assert len(statuses) == 1, f"{label}: status differs between prefixes"
    assert len(bodies) == 1, f"{label}: body differs between prefixes"


def test_artifact_injection_decides_the_same_way_under_both_prefixes(
    targets: dict[str, Path],
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  # Injection is anchored on the configured sessions root, which the test owns here —
  # the prefix spelling plays no part in the decision. The client carries the access key
  # cookie so the injected-credential branch is the one under test.
  monkeypatch.setattr(
      files_api, "get_config",
      lambda: SimpleNamespace(sessions_dir=tmp_path / "sessions", charliebot_access_key="secret"))
  client = _client()
  creds = {"cookies": {"charliebot_access_key": "secret"}}
  for label, target in targets.items():
    injected = {ARTIFACT_SCRIPT in client.get(f"{prefix}{target}", **creds).text for prefix in PREFIXES}
    assert len(injected) == 1, f"{label}: injection differs between prefixes"
  # The predicate itself is unchanged: the artifact page gets the review UI, a plain page does not.
  for prefix in PREFIXES:
    assert ARTIFACT_SCRIPT in client.get(f"{prefix}{targets['artifact page']}", **creds).text
    assert ARTIFACT_SCRIPT not in client.get(f"{prefix}{targets['plain HTML file']}", **creds).text


def test_head_answers_the_same_status_as_get_under_both_prefixes(targets: dict[str, Path]) -> None:
  # The render-time probe asks with HEAD, so the marker only ever appears for a path the server
  # answers 404 for: a HEAD that came back 405 would mark nothing at all.
  client = _client()
  for label, target in targets.items():
    for prefix in PREFIXES:
      url = f"{prefix}{target}"
      assert client.head(url).status_code == client.get(url).status_code, f"{label} under {prefix}"
  for prefix in PREFIXES:
    assert client.head(f"{prefix}{targets['non-HTML file']}").status_code == 200
    assert client.head(f"{prefix}{targets['absent path']}").status_code == 404


def test_a_non_html_file_is_served_byte_for_byte_under_both_prefixes(targets: dict[str, Path]) -> None:
  client = _client()
  target = targets["non-HTML file"]
  for prefix in PREFIXES:
    assert client.get(f"{prefix}{target}").content == target.read_bytes()


@pytest.mark.asyncio
@pytest.mark.parametrize("prefix", PREFIXES)
async def test_a_navigation_under_either_prefix_needs_no_token(
    monkeypatch: pytest.MonkeyPatch,
    prefix: str,
) -> None:
  monkeypatch.setattr(auth, "get_config", lambda: SimpleNamespace(charliebot_access_key="secret"))
  middleware = auth.AuthMiddleware(app=None)
  response = await middleware.dispatch(_navigation(f"{prefix}/tmp/trace.json"), _served)
  assert response.status_code == 200


@pytest.mark.parametrize("prefix", PREFIXES)
def test_a_trace_input_under_either_prefix_names_the_same_file(prefix: str, tmp_path: Path) -> None:
  trace = tmp_path / "rank0.json"
  url, path = pages._trace_input(f"{prefix}{trace}")
  assert path == trace
  assert url == f"{prefix}{trace}"


@pytest.mark.asyncio
@pytest.mark.parametrize("prefix", PREFIXES)
async def test_the_viewer_resolves_a_trace_under_either_prefix_to_the_same_url(
    prefix: str,
    tmp_path: Path,
) -> None:
  trace = tmp_path / "rank0.json"
  trace.write_text(json.dumps({"traceEvents": []}), encoding="utf-8")
  response = await pages.perfetto_viewer(
      make_page_request("/perfetto"), trace=[f"{prefix}{trace}"], dir=None, pattern="*.json", title=None, slim=None)

  merged_url = response.context["trace_url"]
  assert urlsplit(merged_url).path == "/perfetto/merged"
  # What the page generates is unchanged: the merge URL carries the bare absolute path.
  assert parse_qs(urlsplit(merged_url).query)["trace"] == [str(trace)]
