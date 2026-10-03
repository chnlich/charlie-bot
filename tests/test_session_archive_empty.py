import pathlib

import conftest
import pytest

from src.core import models, sessions


@pytest.mark.asyncio
async def test_archive_empty_session_permanently_deletes_it(tmp_path: pathlib.Path) -> None:
  cfg = conftest.build_sessions_cfg(tmp_path)
  session_mgr = sessions.SessionManager(cfg)
  meta = await session_mgr.create_session(models.CreateSessionRequest(name="Empty"), backend=conftest.OPUS_BACKEND_ID)
  session_dir = cfg.sessions_dir / meta.id

  with conftest.make_sessions_client(cfg, session_mgr) as client:
    response = client.delete(f"/api/sessions/{meta.id}")
    get_response = client.get(f"/api/sessions/{meta.id}")

  assert response.status_code == 200
  assert response.json()["id"] == meta.id
  assert response.json()["status"] == "active"
  assert not session_dir.exists()
  assert await session_mgr.get_session(meta.id) is None
  assert get_response.status_code == 404


@pytest.mark.asyncio
async def test_archive_non_empty_session_keeps_files_and_marks_archived(tmp_path: pathlib.Path) -> None:
  cfg = conftest.build_sessions_cfg(tmp_path)
  session_mgr = sessions.SessionManager(cfg)
  meta = await session_mgr.create_session(
      models.CreateSessionRequest(name="Non-empty"), backend=conftest.OPUS_BACKEND_ID)
  await session_mgr.save_chat_event(meta.id, conftest.user_event("hello"))
  session_dir = cfg.sessions_dir / meta.id
  events_path = session_mgr.get_chat_events_path(meta.id)

  with conftest.make_sessions_client(cfg, session_mgr) as client:
    response = client.delete(f"/api/sessions/{meta.id}")

  assert response.status_code == 200
  assert response.json()["status"] == "archived"
  assert session_dir.exists()
  assert events_path.exists()
  fresh = await session_mgr.get_session(meta.id)
  assert fresh is not None
  assert fresh.status == models.SessionStatus.ARCHIVED
