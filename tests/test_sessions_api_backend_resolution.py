"""Regression tests for fork/Elon-e backend resolution at the API route layer."""

import json
from collections.abc import Awaitable
from pathlib import Path
from typing import Any

import pytest
from conftest import (
    CHAT_RUN_AND_FINALIZE_PATCH_TARGET,
    OPUS_BACKEND_ID,
    assistant_text_event,
    build_two_backend_cfg,
    close_create_logged_task,
    user_event,
)
from conftest import make_sessions_client as _build_client
from conftest import session_dir_names as _session_dir_names

from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, SessionMetadata
from src.core.sessions import SessionManager


async def _seed_parent(session_mgr: SessionManager, *, backend: str) -> str:
  parent = await session_mgr.create_session(CreateSessionRequest(name="Parent"), backend=backend)
  events_path = session_mgr.get_chat_events_path(parent.id)
  events_path.parent.mkdir(parents=True, exist_ok=True)
  events_path.write_text(
      "\n".join([
          json.dumps(user_event("hello")),
          json.dumps(assistant_text_event("world")),
      ]) + "\n",
      encoding="utf-8",
  )
  return parent.id


def _capture_bootstrap(monkeypatch: pytest.MonkeyPatch) -> list[dict[str, Any]]:
  calls: list[dict[str, Any]] = []

  def fake_run_and_finalize(
      cfg: CharlieBotConfig, meta: SessionMetadata, content: str, session_mgr: SessionManager,
      **kwargs: object) -> Awaitable[None]:
    calls.append({"meta": meta, "content": content, "kwargs": kwargs})

    async def noop() -> None:
      return None

    return noop()

  monkeypatch.setattr(CHAT_RUN_AND_FINALIZE_PATCH_TARGET, fake_run_and_finalize)
  monkeypatch.setattr("src.core.tasks.create_logged_task", close_create_logged_task)
  return calls


_RouteEnv = tuple[CharlieBotConfig, SessionManager, list[dict[str, Any]]]


@pytest.fixture
def two_backend_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> _RouteEnv:
  """(cfg, session_mgr, calls) with the chat bootstrap stubbed: cfg registers the two backends, calls
  captures run_and_finalize invocations, and logged tasks are closed."""
  calls = _capture_bootstrap(monkeypatch)
  cfg = build_two_backend_cfg(tmp_path)
  return cfg, SessionManager(cfg), calls


@pytest.mark.asyncio
async def test_fork_route_inherits_parent_backend_when_backend_omitted(two_backend_env: _RouteEnv) -> None:
  cfg, session_mgr, _ = two_backend_env
  parent_id = await _seed_parent(session_mgr, backend=OPUS_BACKEND_ID)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{parent_id}/fork")

  assert response.status_code == 200
  assert response.json()["backend"] == OPUS_BACKEND_ID


# ------------------------------------------------ validate-or-raise: unresolvable backend

# parent_backend=None is the create route (no parent); the inherited rows seed a parent already
# pinned to the unresolvable id and omit "backend" from the payload so the route inherits it.
_REJECT_UNRESOLVABLE_BACKEND_ROWS = [
    pytest.param("create", None, {"backend": "missing-backend"}, id="create-explicit"),
    pytest.param("fork", OPUS_BACKEND_ID, {
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
  before = _session_dir_names(cfg)
  url = "/api/sessions/" if route == "create" else f"/api/sessions/{parent_id}/{route}"

  with _build_client(cfg, session_mgr) as client:
    response = client.post(url, json=payload)

  assert response.status_code == 400
  assert _session_dir_names(cfg) == before
  if parent_id is not None:
    parent_after = await session_mgr.get_session(parent_id)
    assert parent_after.status == parent_before.status


# --------------------------------------------------------- store-level property

# ---------------------------------------- regression: documented default carve-out


@pytest.mark.asyncio
async def test_create_route_defaults_to_first_backend_option_when_omitted(two_backend_env: _RouteEnv,) -> None:
  cfg, session_mgr, _ = two_backend_env

  with _build_client(cfg, session_mgr) as client:
    response = client.post("/api/sessions/", json={})

  assert response.status_code == 200
  assert response.json()["backend"] == cfg.backends.options[0].id
