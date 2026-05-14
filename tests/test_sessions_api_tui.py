from pathlib import Path

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.deps import get_session_manager
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig, get_config
from src.core.models import BackendOption, CreateSessionRequest, SessionMetadata
from src.core.sessions import SessionManager


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
          BackendOption(id="claude-tui", label="Claude TUI", type="tui-cli"),
      ],
  )


def _build_client(cfg: CharlieBotConfig, session_mgr: SessionManager) -> TestClient:
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  return TestClient(app)


@pytest.mark.asyncio
async def test_stop_tui_kills_tmux_for_tui_session(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(name="TUI", backend="claude-tui")
  await session_mgr.save_metadata(meta)
  killed = []

  async def fake_kill_tmux_session(session_id: str) -> None:
    killed.append(session_id)

  monkeypatch.setattr("src.agents.backends.tui.kill_tmux_session", fake_kill_tmux_session)

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{meta.id}/tui/stop")

  assert response.status_code == 200
  assert response.json() == {"stopped": True}
  assert killed == [meta.id]


@pytest.mark.asyncio
async def test_stop_tui_rejects_non_tui_session(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="SDK"), backend="claude-opus-4.6")

  with _build_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{meta.id}/tui/stop")

  assert response.status_code == 400
  assert response.json()["detail"] == "Session backend is not tui-cli"


@pytest.mark.asyncio
async def test_archive_tui_session_does_not_kill_tmux(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = SessionMetadata(name="TUI", backend="claude-tui")
  await session_mgr.save_metadata(meta)
  killed = []

  async def fake_kill_tmux_session(session_id: str) -> None:
    killed.append(session_id)

  monkeypatch.setattr("src.agents.backends.tui.kill_tmux_session", fake_kill_tmux_session)

  with _build_client(cfg, session_mgr) as client:
    response = client.delete(f"/api/sessions/{meta.id}")

  assert response.status_code == 200
  assert response.json()["status"] == "archived"
  assert killed == []


@pytest.mark.asyncio
async def test_tui_status_returns_flat_dict_for_tui_sessions_only(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  cc_meta = await session_mgr.create_session(CreateSessionRequest(name="SDK"), backend="claude-opus-4.6")
  tui_meta = SessionMetadata(name="TUI", backend="claude-tui")
  await session_mgr.save_metadata(tui_meta)
  checked = []

  async def fake_tmux_session_exists(session_id: str) -> bool:
    checked.append(session_id)
    return session_id == tui_meta.id

  monkeypatch.setattr("src.agents.backends.tui.tmux_session_exists", fake_tmux_session_exists)

  with _build_client(cfg, session_mgr) as client:
    response = client.get("/api/sessions/tui/status")

  assert response.status_code == 200
  assert response.json() == {tui_meta.id: True}
  assert checked == [tui_meta.id]
  assert cc_meta.id not in checked
