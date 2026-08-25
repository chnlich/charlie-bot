from pathlib import Path

import pytest
from conftest import make_sessions_client as _build_client

from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, CreateSessionRequest, SessionStatus
from src.core.sessions import SessionManager


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
      ],
  )


@pytest.mark.asyncio
async def test_archive_empty_session_permanently_deletes_it(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="Empty"), backend="claude-opus-4.6")
  session_dir = cfg.sessions_dir / meta.id

  with _build_client(cfg, session_mgr) as client:
    response = client.delete(f"/api/sessions/{meta.id}")
    get_response = client.get(f"/api/sessions/{meta.id}")

  assert response.status_code == 200
  assert response.json()["id"] == meta.id
  assert response.json()["status"] == "active"
  assert not session_dir.exists()
  assert await session_mgr.get_session(meta.id) is None
  assert get_response.status_code == 404


@pytest.mark.asyncio
async def test_archive_non_empty_session_keeps_files_and_marks_archived(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="Non-empty"), backend="claude-opus-4.6")
  await session_mgr.save_chat_event(meta.id, {"type": "user", "content": "hello"})
  session_dir = cfg.sessions_dir / meta.id
  events_path = session_mgr.get_chat_events_path(meta.id)

  with _build_client(cfg, session_mgr) as client:
    response = client.delete(f"/api/sessions/{meta.id}")

  assert response.status_code == 200
  assert response.json()["status"] == "archived"
  assert session_dir.exists()
  assert events_path.exists()
  fresh = await session_mgr.get_session(meta.id)
  assert fresh is not None
  assert fresh.status == SessionStatus.ARCHIVED
