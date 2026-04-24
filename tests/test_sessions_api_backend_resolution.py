"""Regression tests for fork/Elon-e backend resolution at the API route layer."""

import json
from pathlib import Path

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


async def _seed_parent(session_mgr: SessionManager, *, backend: str = "claude-opus-4.6") -> str:
  parent = await session_mgr.create_session(CreateSessionRequest(name="Parent"), backend=backend)
  events_path = session_mgr.get_chat_events_path(parent.id)
  events_path.parent.mkdir(parents=True, exist_ok=True)
  events_path.write_text(
      "\n".join([
          json.dumps({"type": "user", "content": "hello"}),
          json.dumps({"type": "assistant", "message": {"content": [{"type": "text", "text": "world"}]}}),
      ]) + "\n",
      encoding="utf-8",
  )
  return parent.id


def _stub_elone_bootstrap(monkeypatch: pytest.MonkeyPatch) -> None:
  async def fake_run_and_finalize(*args, **kwargs) -> None:
    return None

  def fake_create_logged_task(coro) -> None:
    coro.close()

  monkeypatch.setattr("src.api.chat.run_and_finalize", fake_run_and_finalize)
  monkeypatch.setattr("src.core.tasks.create_logged_task", fake_create_logged_task)


@pytest.mark.asyncio
async def test_fork_route_inherits_parent_backend_when_backend_omitted(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  parent_id = await _seed_parent(session_mgr, backend="claude-opus-4.6")

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{parent_id}/fork")

  assert response.status_code == 200
  assert response.json()["backend"] == "claude-opus-4.6"


@pytest.mark.asyncio
async def test_fork_route_resolves_codex_family_override(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  parent_id = await _seed_parent(session_mgr, backend="claude-opus-4.6")

  with _build_client(cfg, session_mgr) as client:
    response = client.post(
        f"/api/sessions/{parent_id}/fork",
        json={"event_index": 1, "backend": "codex-future"},
    )

  assert response.status_code == 200
  assert response.json()["backend"] == "codex-o3"


@pytest.mark.asyncio
async def test_elone_route_accepts_valid_backend_override(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _stub_elone_bootstrap(monkeypatch)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  parent_id = await _seed_parent(session_mgr, backend="claude-opus-4.6")

  with _build_client(cfg, session_mgr) as client:
    response = client.post(
        f"/api/sessions/{parent_id}/elone",
        json={"event_index": 1, "backend": "codex-o3"},
    )

  assert response.status_code == 200
  assert response.json()["backend"] == "codex-o3"


@pytest.mark.asyncio
async def test_elone_route_falls_back_to_parent_backend_for_invalid_override(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  _stub_elone_bootstrap(monkeypatch)
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  parent_id = await _seed_parent(session_mgr, backend="codex-o3")

  with _build_client(cfg, session_mgr) as client:
    response = client.post(
        f"/api/sessions/{parent_id}/elone",
        json={"event_index": 1, "backend": "missing-backend"},
    )

  assert response.status_code == 200
  assert response.json()["backend"] == "codex-o3"
