import pathlib

import conftest
import pytest

from src.infra import event_types as ET


@pytest.mark.asyncio
async def test_archive_empty_session_permanently_deletes_it(tmp_path: pathlib.Path) -> None:
  from tests.test_task_execution import make_api_client

  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  meta = await conftest.create_task(tree, parent=None, request_id="empty", name="Empty")
  session_dir = cfg.sessions_dir / meta.id

  with make_api_client(cfg, session_blocks, tree) as client:
    response = client.delete(f"/api/sessions/{meta.id}/permanent")
    get_response = client.get(f"/api/sessions/{meta.id}")

  assert response.status_code == 204
  assert not session_dir.exists()
  assert await session_blocks.store.get_session(meta.id) is None
  assert get_response.status_code == 404


@pytest.mark.asyncio
async def test_archive_non_empty_session_keeps_files_and_marks_archived(tmp_path: pathlib.Path) -> None:
  from tests.test_task_execution import make_api_client

  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  meta = await conftest.create_task(tree, parent=None, request_id="non-empty", name="Non-empty")
  await tree.dispatch.admit_input(meta.id, event_type=ET.USER, content="hello", actor="user")
  session_dir = cfg.sessions_dir / meta.id
  events_path = session_blocks.events.get_chat_events_path(meta.id)

  with make_api_client(cfg, session_blocks, tree) as client:
    response = client.delete(f"/api/sessions/{meta.id}")

  assert response.status_code == 200
  assert response.json() == {"archived": [meta.id]}
  assert session_dir.exists()
  assert events_path.exists()
  fresh = await session_blocks.store.get_session(meta.id)
  assert fresh is not None
  assert tree.task_state(meta.id) == "archived"
