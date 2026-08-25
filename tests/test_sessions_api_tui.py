from pathlib import Path

import pytest
from conftest import OPUS_BACKEND_OPTION
from conftest import make_sessions_client as _build_client

from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, CreateSessionRequest, SessionMetadata
from src.core.sessions import SessionManager


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          OPUS_BACKEND_OPTION,
          BackendOption(id="claude-tui", label="Claude TUI", type="tui-cli"),
      ],
  )


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
  await session_mgr.save_chat_event(meta.id, {"type": "user", "content": "hello"})

  with _build_client(cfg, session_mgr) as client:
    response = client.delete(f"/api/sessions/{meta.id}")

  assert response.status_code == 200
  assert response.json()["status"] == "archived"
  assert killed == []


@pytest.mark.asyncio
async def test_tui_status_returns_running_busy_dict_for_tui_sessions_only(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  cc_meta = await session_mgr.create_session(CreateSessionRequest(name="SDK"), backend="claude-opus-4.6")
  running_tui_meta = SessionMetadata(name="TUI running", backend="claude-tui")
  stopped_tui_meta = SessionMetadata(name="TUI stopped", backend="claude-tui")
  await session_mgr.save_metadata(running_tui_meta)
  await session_mgr.save_metadata(stopped_tui_meta)
  checked = []
  busy_checked = []

  async def fake_tmux_session_exists(session_id: str) -> bool:
    checked.append(session_id)
    return session_id == running_tui_meta.id

  def fake_claude_jsonl_busy(session_id: str) -> bool:
    busy_checked.append(session_id)
    return session_id == running_tui_meta.id

  monkeypatch.setattr("src.agents.backends.tui.tmux_session_exists", fake_tmux_session_exists)
  monkeypatch.setattr("src.agents.backends.tui._claude_jsonl_busy", fake_claude_jsonl_busy)

  with _build_client(cfg, session_mgr) as client:
    ids = ",".join([cc_meta.id, running_tui_meta.id, stopped_tui_meta.id])
    response = client.get(f"/api/sessions/tui/status?ids={ids}")

  assert response.status_code == 200
  assert response.json() == {
      running_tui_meta.id: {"running": True, "busy": True},
      stopped_tui_meta.id: {"running": False, "busy": False},
  }
  assert set(checked) == {running_tui_meta.id, stopped_tui_meta.id}
  assert busy_checked == [running_tui_meta.id]
  assert cc_meta.id not in checked
