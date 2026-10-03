import pathlib

import conftest
import pytest

from src.core import models, sessions


def _install_kill_tmux_double(monkeypatch: pytest.MonkeyPatch) -> list[str]:
  """Patch kill_tmux_session to record calls; returns the list it records into."""
  killed: list[str] = []

  async def fake_kill_tmux_session(session_id: str) -> None:
    killed.append(session_id)

  monkeypatch.setattr(conftest.TUI_KILL_TMUX_SESSION_PATCH_TARGET, fake_kill_tmux_session)
  return killed


@pytest.mark.asyncio
async def test_stop_tui_kills_tmux_for_tui_session(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: pathlib.Path,
) -> None:
  cfg = conftest.build_tui_sessions_cfg(tmp_path)
  session_mgr = sessions.SessionManager(cfg)
  meta = models.SessionMetadata(name="TUI", backend="claude-tui")
  await session_mgr.save_metadata(meta)
  killed = _install_kill_tmux_double(monkeypatch)

  with conftest.make_sessions_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{meta.id}/tui/stop")

  assert response.status_code == 200
  assert response.json() == {"stopped": True}
  assert killed == [meta.id]


@pytest.mark.asyncio
async def test_stop_tui_rejects_non_tui_session(tmp_path: pathlib.Path) -> None:
  cfg = conftest.build_tui_sessions_cfg(tmp_path)
  session_mgr = sessions.SessionManager(cfg)
  meta = await session_mgr.create_session(models.CreateSessionRequest(name="SDK"), backend=conftest.OPUS_BACKEND_ID)

  with conftest.make_sessions_client(cfg, session_mgr) as client:
    response = client.post(f"/api/sessions/{meta.id}/tui/stop")

  assert response.status_code == 400
  assert response.json()["detail"] == "Session backend is not tui-cli"


@pytest.mark.asyncio
async def test_archive_tui_session_does_not_kill_tmux(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: pathlib.Path,
) -> None:
  cfg = conftest.build_tui_sessions_cfg(tmp_path)
  session_mgr = sessions.SessionManager(cfg)
  meta = models.SessionMetadata(name="TUI", backend="claude-tui")
  await session_mgr.save_metadata(meta)
  killed = _install_kill_tmux_double(monkeypatch)
  await session_mgr.save_chat_event(meta.id, conftest.user_event("hello"))

  with conftest.make_sessions_client(cfg, session_mgr) as client:
    response = client.delete(f"/api/sessions/{meta.id}")

  assert response.status_code == 200
  assert response.json()["status"] == "archived"
  assert not killed
