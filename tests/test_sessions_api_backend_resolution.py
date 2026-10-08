"""Regression tests for fork/Elon-e backend resolution at the API route layer."""

import json
import pathlib
from typing import Any

import conftest
import pytest

from src.infra import config, models
from src.runtime import session_fork


async def _seed_parent(session_blocks: conftest.SessionBlocks, *, backend: str = conftest.OPUS_BACKEND_ID) -> str:
  parent = await conftest.create_root_session(
      session_blocks, models.CreateSessionRequest(name="Parent"), backend=backend)
  events_path = session_blocks.events.get_chat_events_path(parent.id)
  events_path.parent.mkdir(parents=True, exist_ok=True)
  events_path.write_text(
      "\n".join([
          json.dumps(conftest.user_event("hello")),
          json.dumps(conftest.assistant_text_event("world")),
      ]) + "\n",
      encoding="utf-8",
  )
  return parent.id


_RouteEnv = tuple[config.CharlieBotConfig, conftest.SessionBlocks, list[tuple[str, list[str]]]]


@pytest.fixture
def two_backend_env(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> _RouteEnv:
  """(cfg, session_blocks, launches): cfg registers the two backends, the deps singletons are a task tree
  over those session blocks, and launches records (session id, input contents) per dispatched run."""
  cfg = conftest.build_two_backend_cfg(tmp_path)
  session_blocks = conftest.build_session_blocks(cfg)
  tree = conftest.build_task_tree(cfg, session_blocks)
  conftest.bind_deps_blocks(monkeypatch, tree, session_blocks)
  launches: list[tuple[str, list[str]]] = []

  async def executor(session_id: str, pending: list[dict], launch_run_id: str | None = None) -> str:
    launches.append((session_id, [e["content"] for e in pending]))
    return "run-1"

  tree.dispatch.executor = executor
  return cfg, session_blocks, launches


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("route", "payload", "opener"), [
        ("fork", {}, session_fork.FORK_BOOTSTRAP_OPENER),
        ("elone", {
            "event_index": 1
        }, session_fork.ELONE_BOOTSTRAP_OPENER),
    ])
async def test_fork_and_elone_routes_inherit_parent_backend_and_dispatch_their_bootstrap(
    two_backend_env: _RouteEnv, route: str, payload: dict[str, Any], opener: str) -> None:
  cfg, session_blocks, launches = two_backend_env
  parent_id = await _seed_parent(session_blocks, backend=conftest.OPUS_BACKEND_ID)

  with conftest.make_sessions_client(cfg, session_blocks) as client:
    response = client.post(f"/api/sessions/{parent_id}/{route}", json=payload)

  assert response.status_code == 200
  assert response.json()["backend"] == conftest.OPUS_BACKEND_ID
  # The child is a manager root: its bootstrap prompt is its first admitted input, launched as a run.
  [(launched_id, [prompt])] = launches
  assert launched_id == response.json()["id"] and prompt.startswith(opener)


# ------------------------------------------------ validate-or-raise: unresolvable backend

# parent_backend=None is the create route (no parent); the inherited rows seed a parent already
# pinned to the unresolvable id and omit "backend" from the payload so the route inherits it.
_REJECT_UNRESOLVABLE_BACKEND_ROWS = [
    pytest.param("create", None, {"backend": "missing-backend"}, id="create-explicit"),
    pytest.param(
        "fork", conftest.OPUS_BACKEND_ID, {
            "event_index": 1,
            "backend": "missing-backend"
        }, id="fork-explicit"),
    pytest.param("fork", "missing-backend", {"event_index": 1}, id="fork-inherited"),
    pytest.param("elone", "codex-o3", {
        "event_index": 1,
        "backend": "missing-backend"
    }, id="elone-explicit"),
    pytest.param("elone", "missing-backend", {"event_index": 1}, id="elone-inherited"),
]


@pytest.mark.asyncio
@pytest.mark.parametrize(("route", "parent_backend", "payload"), _REJECT_UNRESOLVABLE_BACKEND_ROWS)
async def test_route_rejects_unresolvable_backend_and_persists_nothing(
    two_backend_env: _RouteEnv, route: str, parent_backend: str | None, payload: dict[str, Any]) -> None:
  """Backend validation precedes every side effect: the route returns 400, persists no child
  session, and leaves the parent's status and rating unchanged."""
  cfg, session_blocks, _ = two_backend_env
  parent_id = None
  parent_before = None
  if parent_backend is not None:
    parent_id = await _seed_parent(session_blocks, backend=parent_backend)
    parent_before = await session_blocks.store.get_session(parent_id)
  before = conftest.session_dir_names(cfg)
  url = "/api/sessions/" if route == "create" else f"/api/sessions/{parent_id}/{route}"

  with conftest.make_sessions_client(cfg, session_blocks) as client:
    response = client.post(url, json=payload)

  assert response.status_code == 400
  assert conftest.session_dir_names(cfg) == before
  if parent_id is not None:
    parent_after = await session_blocks.store.get_session(parent_id)
    assert parent_after.status == parent_before.status


# --------------------------------------------------------- store-level property

# ---------------------------------------- regression: documented default carve-out


@pytest.mark.asyncio
async def test_create_route_defaults_to_first_backend_option_when_omitted(two_backend_env: _RouteEnv,) -> None:
  cfg, session_blocks, _ = two_backend_env

  with conftest.make_sessions_client(cfg, session_blocks) as client:
    response = client.post("/api/sessions/", json={})

  assert response.status_code == 200
  assert response.json()["backend"] == cfg.backends.options[0].id
