"""Regression tests for fork/Elon-e backend resolution at the API route layer."""

import json
from pathlib import Path
from typing import Any

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.deps import get_session_manager
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig, get_config
from src.core.models import BackendOption, CreateSessionRequest
from src.core.sessions import SessionManager


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )


def _build_client(cfg: CharlieBotConfig, session_mgr: SessionManager) -> TestClient:
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  return TestClient(app)


def _session_dir_names(cfg: CharlieBotConfig) -> set[str]:
  """Snapshot the names of session directories on disk (existence, not content)."""
  if not cfg.sessions_dir.exists():
    return set()
  return {d.name for d in cfg.sessions_dir.iterdir() if d.is_dir()}


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

  def fake_create_logged_task(coro) -> None:
    coro.close()

  monkeypatch.setattr("src.api.chat.run_and_finalize", fake_run_and_finalize)
  monkeypatch.setattr("src.core.tasks.create_logged_task", fake_create_logged_task)
  return calls


def _stub_bootstrap(monkeypatch: pytest.MonkeyPatch) -> None:
  _capture_bootstrap(monkeypatch)


@pytest.mark.asyncio
async def test_fork_route_inherits_parent_backend_when_backend_omitted(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  _stub_bootstrap(monkeypatch)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  parent_id = await _seed_parent(session_mgr, backend="claude-opus-4.6")

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{parent_id}/fork")

  assert response.status_code == 200
  assert response.json()["backend"] == "claude-opus-4.6"


@pytest.mark.asyncio
async def test_fork_route_resolves_codex_family_override(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _stub_bootstrap(monkeypatch)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
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
async def test_elone_route_accepts_valid_backend_override(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _stub_bootstrap(monkeypatch)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
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
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  _stub_bootstrap(monkeypatch)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
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
async def test_fork_route_bootstrap_points_at_reference_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  calls = _capture_bootstrap(monkeypatch)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
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
async def test_elone_route_bootstrap_points_at_reference_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  calls = _capture_bootstrap(monkeypatch)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
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
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  _stub_bootstrap(monkeypatch)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  before = _session_dir_names(cfg)

  with _build_client(cfg, session_mgr) as client:
    response = client.post("/api/sessions/", json={"backend": "missing-backend"})

  assert response.status_code == 400
  assert _session_dir_names(cfg) == before


@pytest.mark.asyncio
async def test_fork_route_rejects_unresolvable_explicit_backend_and_persists_nothing(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  _stub_bootstrap(monkeypatch)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
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
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  _stub_bootstrap(monkeypatch)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  parent_id = await _seed_parent(session_mgr, backend="missing-backend")
  before = _session_dir_names(cfg)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{parent_id}/fork", json={"event_index": 1})

  assert response.status_code == 400
  assert _session_dir_names(cfg) == before


@pytest.mark.asyncio
async def test_elone_route_rejects_unresolvable_inherited_backend_and_leaves_parent_untouched(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """Failure must precede the parent archive/thumbs-down side effect."""
  _stub_bootstrap(monkeypatch)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
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
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  _stub_bootstrap(monkeypatch)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
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
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  _stub_bootstrap(monkeypatch)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)

  with _build_client(cfg, session_mgr) as client:
    response = client.post("/api/sessions/", json={})

  assert response.status_code == 200
  assert response.json()["backend"] == cfg.backend_options[0].id
