"""Regression tests for fork/Elon-e backend resolution at the API route layer."""

import json
from pathlib import Path
from typing import Any

import pytest
from conftest import (
  CHAT_RUN_AND_FINALIZE_PATCH_TARGET,
  build_two_backend_cfg,
  close_create_logged_task,
)
from conftest import make_sessions_client as _build_client
from conftest import session_dir_names as _session_dir_names

from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest
from src.core.sessions import SessionManager


async def _seed_parent(session_mgr: SessionManager, *, backend: str = "claude-opus-4.6") -> str:
  parent = await session_mgr.create_session(CreateSessionRequest(name="Parent"), backend=backend)
  events_path = session_mgr.get_chat_events_path(parent.id)
  events_path.parent.mkdir(parents=True, exist_ok=True)
  events_path.write_text(
      "\n".join(
          [
              json.dumps({
                  "type": "user",
                  "content": "hello"
              }),
              json.dumps({
                  "type": "assistant",
                  "message": {
                      "content": [{
                          "type": "text",
                          "text": "world"
                      }]
                  }
              }),
          ]) + "\n",
      encoding="utf-8",
  )
  return parent.id


def _capture_bootstrap(monkeypatch: pytest.MonkeyPatch) -> list[dict[str, Any]]:
  calls: list[dict[str, Any]] = []

  def fake_run_and_finalize(cfg, meta, content, session_mgr, **kwargs):
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
  parent_id = await _seed_parent(session_mgr, backend="claude-opus-4.6")

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{parent_id}/fork")

  assert response.status_code == 200
  assert response.json()["backend"] == "claude-opus-4.6"


@pytest.mark.asyncio
async def test_fork_route_resolves_codex_family_override(two_backend_env: _RouteEnv) -> None:
  cfg, session_mgr, _ = two_backend_env
  parent_id = await _seed_parent(session_mgr, backend="claude-opus-4.6")

  with _build_client(cfg, session_mgr) as client:
    response = client.post(
        f"/api/sessions/{parent_id}/fork",
        json={
            "event_index": 1,
            "backend": "codex-future"
        },
    )

  assert response.status_code == 200
  assert response.json()["backend"] == "codex-o3"


@pytest.mark.asyncio
async def test_elone_route_accepts_valid_backend_override(two_backend_env: _RouteEnv) -> None:
  cfg, session_mgr, _ = two_backend_env
  parent_id = await _seed_parent(session_mgr, backend="claude-opus-4.6")

  with _build_client(cfg, session_mgr) as client:
    response = client.post(
        f"/api/sessions/{parent_id}/elone",
        json={
            "event_index": 1,
            "backend": "codex-o3"
        },
    )

  assert response.status_code == 200
  assert response.json()["backend"] == "codex-o3"


@pytest.mark.asyncio
async def test_elone_route_rejects_unresolvable_explicit_backend_and_persists_nothing(
    two_backend_env: _RouteEnv,
) -> None:
  cfg, session_mgr, _ = two_backend_env
  parent_id = await _seed_parent(session_mgr, backend="codex-o3")
  before = _session_dir_names(cfg)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(
        f"/api/sessions/{parent_id}/elone",
        json={
            "event_index": 1,
            "backend": "missing-backend"
        },
    )

  assert response.status_code == 400
  assert _session_dir_names(cfg) == before


@pytest.mark.asyncio
async def test_fork_route_bootstrap_points_at_reference_file(two_backend_env: _RouteEnv) -> None:
  cfg, session_mgr, calls = two_backend_env
  parent_id = await _seed_parent(session_mgr, backend="claude-opus-4.6")

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{parent_id}/fork", json={"event_index": 0})

  assert response.status_code == 200
  child_id = response.json()["id"]
  reference_path = session_mgr.get_chat_events_path(child_id).parent / "parent_reference.jsonl"
  assert len(calls) == 1
  prompt = calls[0]["content"]
  assert str(reference_path) in prompt
  assert "chronological" in prompt
  assert "newest entries at the end" in prompt
  assert "reconstructed recent context" not in prompt
  assert "recap" not in prompt.lower()


@pytest.mark.asyncio
async def test_elone_route_bootstrap_points_at_reference_file(two_backend_env: _RouteEnv) -> None:
  cfg, session_mgr, calls = two_backend_env
  parent_id = await _seed_parent(session_mgr, backend="claude-opus-4.6")

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{parent_id}/elone", json={"event_index": 1})

  assert response.status_code == 200
  child_id = response.json()["id"]
  reference_path = session_mgr.get_chat_events_path(child_id).parent / "parent_reference.jsonl"
  assert len(calls) == 1
  prompt = calls[0]["content"]
  assert str(reference_path) in prompt
  assert "wasn't satisfied" in prompt
  assert "Confirm with the user before acting." in prompt
  assert "reconstructed recent context" not in prompt
  assert "recap" not in prompt.lower()


# ------------------------------------------------- validate-or-raise: explicit id


@pytest.mark.asyncio
async def test_create_route_rejects_unresolvable_backend_and_persists_nothing(
    two_backend_env: _RouteEnv,
) -> None:
  cfg, session_mgr, _ = two_backend_env
  before = _session_dir_names(cfg)

  with _build_client(cfg, session_mgr) as client:
    response = client.post("/api/sessions/", json={"backend": "missing-backend"})

  assert response.status_code == 400
  assert _session_dir_names(cfg) == before


@pytest.mark.asyncio
async def test_fork_route_rejects_unresolvable_explicit_backend_and_persists_nothing(
    two_backend_env: _RouteEnv,
) -> None:
  cfg, session_mgr, _ = two_backend_env
  parent_id = await _seed_parent(session_mgr, backend="claude-opus-4.6")
  before = _session_dir_names(cfg)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(
        f"/api/sessions/{parent_id}/fork",
        json={
            "event_index": 1,
            "backend": "missing-backend"
        },
    )

  assert response.status_code == 400
  assert _session_dir_names(cfg) == before


# ---------------------------------------------- validate-or-raise: inherited id


@pytest.mark.asyncio
async def test_fork_route_rejects_unresolvable_inherited_backend_and_persists_nothing(
    two_backend_env: _RouteEnv,
) -> None:
  cfg, session_mgr, _ = two_backend_env
  parent_id = await _seed_parent(session_mgr, backend="missing-backend")
  before = _session_dir_names(cfg)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{parent_id}/fork", json={"event_index": 1})

  assert response.status_code == 400
  assert _session_dir_names(cfg) == before


@pytest.mark.asyncio
async def test_elone_route_rejects_unresolvable_inherited_backend_and_leaves_parent_untouched(
    two_backend_env: _RouteEnv,
) -> None:
  """Failure must precede the parent archive/thumbs-down side effect."""
  cfg, session_mgr, _ = two_backend_env
  parent_id = await _seed_parent(session_mgr, backend="missing-backend")
  parent_before = await session_mgr.get_session(parent_id)
  before = _session_dir_names(cfg)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{parent_id}/elone", json={"event_index": 1})

  assert response.status_code == 400
  assert _session_dir_names(cfg) == before
  parent_after = await session_mgr.get_session(parent_id)
  assert parent_after.status == parent_before.status
  assert parent_after.rating == parent_before.rating


# --------------------------------------------------------- store-level property


@pytest.mark.asyncio
async def test_persisted_backend_stays_within_backend_options_across_mixed_calls(
    two_backend_env: _RouteEnv,
) -> None:
  cfg, session_mgr, _ = two_backend_env
  valid_ids = {opt.id for opt in cfg.backend_options}
  fork_parent_id = await _seed_parent(session_mgr, backend="claude-opus-4.6")
  elone_ok_parent_id = await _seed_parent(session_mgr, backend="claude-opus-4.6")
  elone_bad_parent_id = await _seed_parent(session_mgr, backend="missing-backend")
  # The bad-parent seed intentionally simulates a pre-existing session pinned to a
  # since-removed id (out of scope to migrate); exclude pre-existing dirs from the
  # property check below and only look at sessions the routes under test produce.
  before_dirs = _session_dir_names(cfg)

  with _build_client(cfg, session_mgr) as client:
    assert client.post("/api/sessions/", json={"name": "A"}).status_code == 200
    assert client.post("/api/sessions/", json={"name": "B", "backend": "nope"}).status_code == 400
    assert client.post(
        f"/api/sessions/{fork_parent_id}/fork",
        json={
            "event_index": 1,
            "backend": "codex-future"
        },
    ).status_code == 200
    assert client.post(
        f"/api/sessions/{fork_parent_id}/fork",
        json={
            "event_index": 1,
            "backend": "still-nope"
        },
    ).status_code == 400
    assert client.post(
        f"/api/sessions/{elone_ok_parent_id}/elone",
        json={
            "event_index": 1,
            "backend": "codex-o3"
        },
    ).status_code == 200
    assert client.post(
        f"/api/sessions/{elone_bad_parent_id}/elone",
        json={"event_index": 1},
    ).status_code == 400

  new_session_ids = _session_dir_names(cfg) - before_dirs
  assert new_session_ids  # sanity: the successful calls above did create sessions
  for session_id in new_session_ids:
    metadata_path = cfg.sessions_dir / session_id / "metadata.json"
    backend = json.loads(metadata_path.read_text(encoding="utf-8"))["backend"]
    assert backend in valid_ids


# ---------------------------------------- regression: documented default carve-out


@pytest.mark.asyncio
async def test_create_route_defaults_to_first_backend_option_when_omitted(
    two_backend_env: _RouteEnv,
) -> None:
  cfg, session_mgr, _ = two_backend_env

  with _build_client(cfg, session_mgr) as client:
    response = client.post("/api/sessions/", json={})

  assert response.status_code == 200
  assert response.json()["backend"] == cfg.backend_options[0].id
