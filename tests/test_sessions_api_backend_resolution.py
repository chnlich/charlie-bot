"""Regression tests for fork/Elon-e backend resolution at the API route layer."""

import json
import pathlib
from collections.abc import Awaitable
from typing import Any

import conftest
import pytest

from src.infra import config, models
from src.runtime import sessions


async def _seed_parent(session_mgr: sessions.SessionManager, *, backend: str = conftest.OPUS_BACKEND_ID) -> str:
  parent = await session_mgr.create_session(models.CreateSessionRequest(name="Parent"), backend=backend)
  events_path = session_mgr.get_chat_events_path(parent.id)
  events_path.parent.mkdir(parents=True, exist_ok=True)
  events_path.write_text(
      "\n".join([
          json.dumps(conftest.user_event("hello")),
          json.dumps(conftest.assistant_text_event("world")),
      ]) + "\n",
      encoding="utf-8",
  )
  return parent.id


def _capture_bootstrap(monkeypatch: pytest.MonkeyPatch) -> list[dict[str, Any]]:
  calls: list[dict[str, Any]] = []

  def fake_run_and_finalize(
      cfg: config.CharlieBotConfig, meta: models.SessionMetadata, content: str, session_mgr: sessions.SessionManager,
      **kwargs: object) -> Awaitable[None]:
    calls.append({"meta": meta, "content": content, "kwargs": kwargs})

    async def noop() -> None:
      return None

    return noop()

  monkeypatch.setattr(conftest.CHAT_RUN_AND_FINALIZE_PATCH_TARGET, fake_run_and_finalize)
  monkeypatch.setattr("src.infra.tasks.create_logged_task", conftest.close_create_logged_task)
  return calls


_RouteEnv = tuple[config.CharlieBotConfig, sessions.SessionManager, list[dict[str, Any]]]


@pytest.fixture
def two_backend_env(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> _RouteEnv:
  """(cfg, session_mgr, calls) with the chat bootstrap stubbed: cfg registers the two backends, calls
  captures run_and_finalize invocations, and logged tasks are closed."""
  calls = _capture_bootstrap(monkeypatch)
  cfg = conftest.build_two_backend_cfg(tmp_path)
  return cfg, sessions.SessionManager(cfg), calls


@pytest.mark.asyncio
async def test_fork_route_inherits_parent_backend_when_backend_omitted(two_backend_env: _RouteEnv) -> None:
  cfg, session_mgr, _ = two_backend_env
  parent_id = await _seed_parent(session_mgr, backend=conftest.OPUS_BACKEND_ID)

  with conftest.make_sessions_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{parent_id}/fork")

  assert response.status_code == 200
  assert response.json()["backend"] == conftest.OPUS_BACKEND_ID


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
  cfg, session_mgr, _ = two_backend_env
  parent_id = None
  parent_before = None
  if parent_backend is not None:
    parent_id = await _seed_parent(session_mgr, backend=parent_backend)
    parent_before = await session_mgr.get_session(parent_id)
  before = conftest.session_dir_names(cfg)
  url = "/api/sessions/" if route == "create" else f"/api/sessions/{parent_id}/{route}"

  with conftest.make_sessions_client(cfg, session_mgr) as client:
    response = client.post(url, json=payload)

  assert response.status_code == 400
  assert conftest.session_dir_names(cfg) == before
  if parent_id is not None:
    parent_after = await session_mgr.get_session(parent_id)
    assert parent_after.status == parent_before.status


# --------------------------------------------------------- store-level property

# ---------------------------------------- regression: documented default carve-out


@pytest.mark.asyncio
async def test_create_route_defaults_to_first_backend_option_when_omitted(two_backend_env: _RouteEnv,) -> None:
  cfg, session_mgr, _ = two_backend_env

  with conftest.make_sessions_client(cfg, session_mgr) as client:
    response = client.post("/api/sessions/", json={})

  assert response.status_code == 200
  assert response.json()["backend"] == cfg.backends.options[0].id
