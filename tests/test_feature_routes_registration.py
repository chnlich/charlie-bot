"""Feature routes: each package serves its HTTP routes through the router its register() adds.

The improve, Slack, Discord, plan and explain routes live in ``src/features/<package>/api.py``.
The checks here pin what moving them out of the runtime routers must not change: the access key
still gates every moved path, no earlier route answers a moved path, and deleting a package's line
in PACKAGES removes exactly the routes its api module serves.
"""

import json
import subprocess
import sys
from typing import Any, NamedTuple

import conftest
import pytest
from fastapi.testclient import TestClient
from starlette.routing import Match

import server
from src.features.artifacts import api as artifacts_api
from src.infra.config import CharlieBotConfig
from src.runtime.api import deps

ACCESS_KEY = "op-secret"
SESSION = "no-such-session"
SESSION_NOT_FOUND = deps.SESSION_NOT_FOUND_DETAIL
# The 404 body of a path no route serves; an endpoint's own refusal carries another detail.
NO_ROUTE_DETAIL = "Not Found"


class MovedRoute(NamedTuple):
  package: str
  method: str
  path: str
  body: dict[str, Any]
  # The status the endpoint answers for an unknown session, and its detail ("" when the detail is not pinned).
  status: int
  detail: str


def _session_body(**fields: Any) -> dict[str, Any]:
  return {"session_id": SESSION, **fields}


IMPROVE = "src.features.improve"
SLACK = "src.features.slack"
DISCORD = "src.features.discord"
ARTIFACTS = "src.features.artifacts"
EXPLAIN = "src.features.explain"

MOVED_ROUTES = [
    MovedRoute(
        IMPROVE, "POST", "/api/internal/improve", _session_body(goal="g", repo_path="/tmp/repo", base_branch="main"),
        404, SESSION_NOT_FOUND),
    MovedRoute(IMPROVE, "POST", "/api/internal/improve/stop", _session_body(), 404, SESSION_NOT_FOUND),
    MovedRoute(SLACK, "POST", "/api/internal/slack/reply", _session_body(text="t"), 404, SESSION_NOT_FOUND),
    MovedRoute(SLACK, "POST", "/api/internal/slack/ack", _session_body(message_ids=["1.0"]), 404, SESSION_NOT_FOUND),
    MovedRoute(DISCORD, "POST", "/api/internal/discord/reply", _session_body(text="t"), 404, SESSION_NOT_FOUND),
    MovedRoute(DISCORD, "POST", "/api/internal/discord/read", _session_body(), 404, SESSION_NOT_FOUND),
    MovedRoute(DISCORD, "POST", "/api/internal/discord/check", {}, 409, ""),
    MovedRoute(
        ARTIFACTS, "POST", "/api/internal/plan/present", _session_body(file="/tmp/plan_01.html", title="t"), 404,
        SESSION_NOT_FOUND),
    MovedRoute(
        ARTIFACTS, "POST", "/api/internal/plan/amend", _session_body(file="/tmp/plan_02.html", note="n"), 404,
        SESSION_NOT_FOUND),
    MovedRoute(ARTIFACTS, "POST", "/api/internal/plan/approve", _session_body(), 404, SESSION_NOT_FOUND),
    MovedRoute(
        ARTIFACTS, "POST", "/api/internal/plan/close", _session_body(plan_id=1, close_as="abandoned"), 404,
        SESSION_NOT_FOUND),
    MovedRoute(ARTIFACTS, "GET", "/api/sessions/{session_id}/plans", {}, 404, SESSION_NOT_FOUND),
    MovedRoute(
        EXPLAIN, "POST", "/api/sessions/{session_id}/explain", {
            "event_index": 0,
            "backend": "b"
        }, 404, SESSION_NOT_FOUND),
    MovedRoute(EXPLAIN, "GET", "/api/sessions/{session_id}/explain?upto=0", {}, 404, SESSION_NOT_FOUND),
    MovedRoute(EXPLAIN, "GET", "/api/sessions/{session_id}/explain/status", {}, 404, SESSION_NOT_FOUND),
]
PACKAGES = sorted({route.package for route in MOVED_ROUTES})


def _request_path(route: MovedRoute) -> str:
  return route.path.format(session_id=SESSION)


def _send(client: TestClient, route: MovedRoute, headers: dict[str, str]) -> Any:
  kwargs: dict[str, Any] = {"headers": headers}
  if route.method == "POST":
    kwargs["json"] = route.body
  return client.request(route.method, _request_path(route), **kwargs)


@pytest.fixture
def keyed_client(tmp_path: Any, monkeypatch: pytest.MonkeyPatch) -> TestClient:
  """The real app under an access key, over session blocks that know no session."""
  conftest.stub_credentials({"charliebot": {"access_key": ACCESS_KEY}})
  cfg = CharlieBotConfig(charliebot_home=tmp_path)
  session_blocks = conftest.build_session_blocks(cfg)
  conftest.bind_deps_blocks(monkeypatch, conftest.build_task_tree(cfg, session_blocks), session_blocks)
  monkeypatch.setattr(artifacts_api, "_plan_manager", None)
  return TestClient(server.app)


def _served_by_feature_apis() -> set[tuple[str, str, str]]:
  """(package, method, path template) of every app route whose endpoint is defined in a ``<package>.api`` module."""
  served = set()
  for route in server.app.routes:
    module = getattr(getattr(route, "endpoint", None), "__module__", "")
    if module.removesuffix(".api") in PACKAGES:
      served.update((module.removesuffix(".api"), method, route.path) for method in route.methods - {"HEAD"})
  return served


def test_the_table_names_every_route_the_feature_api_modules_serve() -> None:
  in_table = {(r.package, r.method, r.path.partition("?")[0]) for r in MOVED_ROUTES}

  assert _served_by_feature_apis() == in_table


@pytest.mark.parametrize("route", MOVED_ROUTES, ids=lambda r: f"{r.method} {r.path}")
def test_the_access_key_gates_a_moved_route_and_a_keyed_request_reaches_its_endpoint(
    keyed_client: TestClient, route: MovedRoute) -> None:
  no_credential = _send(keyed_client, route, {})
  wrong_credential = _send(keyed_client, route, {"Authorization": "Bearer wrong"})
  keyed = _send(keyed_client, route, {"Authorization": f"Bearer {ACCESS_KEY}"})

  assert (no_credential.status_code, wrong_credential.status_code) == (401, 401)
  assert keyed.status_code == route.status
  assert keyed.json()["detail"] != NO_ROUTE_DETAIL
  if route.detail:
    assert keyed.json()["detail"] == route.detail


@pytest.mark.parametrize("route", MOVED_ROUTES, ids=lambda r: f"{r.method} {r.path}")
def test_a_moved_path_resolves_to_its_own_feature_endpoint(route: MovedRoute) -> None:
  scope = {
      "type": "http",
      "method": route.method,
      "path": _request_path(route).partition("?")[0],
      "root_path": "",
      "query_string": b"",
      "headers": [],
  }

  first = next(r for r in server.app.routes if r.matches(scope)[0] == Match.FULL)

  assert first.endpoint.__module__ == f"{route.package}.api"


# The probe process deletes one package line, registers, then imports the server module,
# which calls register_all() again (a no-op) and assembles the app from the registry.
PROBE = """
import json
from src.app import registrations
registrations.PACKAGES = tuple(p for p in registrations.PACKAGES if p != {deleted!r})
registrations.register_all()
import server
print(json.dumps([[route.path, sorted(getattr(route, "methods", None) or [])] for route in server.app.routes]))
"""


@pytest.mark.parametrize("package", PACKAGES)
def test_deleting_a_package_line_removes_exactly_the_routes_its_api_module_serves(package: str) -> None:
  probe = subprocess.run(
      [sys.executable, "-c", PROBE.format(deleted=package)],
      cwd=conftest.ROOT,
      capture_output=True,
      text=True,
      timeout=60,
      check=False)
  assert probe.returncode == 0, probe.stderr
  without_package = json.loads(probe.stdout.splitlines()[-1])
  own = {route.path.partition("?")[0] for route in MOVED_ROUTES if route.package == package}
  with_package = [[route.path, sorted(getattr(route, "methods", None) or [])] for route in server.app.routes]

  assert any(path in own for path, _ in with_package)
  assert not any(path in own for path, _ in without_package)
  assert without_package == [[path, methods] for path, methods in with_package if path not in own]
