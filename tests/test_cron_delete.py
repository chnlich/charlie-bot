"""Tests for cron-task deletion: the yaml goes, everything else stays."""

import pathlib

import conftest
import pytest

from src.infra import models
from src.runtime import task_sessions


@pytest.mark.asyncio
async def test_delete_unlinks_the_yaml_and_leaves_the_bound_node_untouched(
    tmp_path: pathlib.Path, temp_home: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Deleting a task archives nothing: the yaml goes, the task's bound node
  stays exactly as it is (it is the user's task-tree node, not the deletion's
  product), and no legacy cron session is archived either."""
  cfg, session_mgr, _scheduler = conftest.make_scheduler_setup(tmp_path)
  tree = task_sessions.TaskTreeManager(cfg, session_mgr)
  conftest.bind_deps_managers(monkeypatch, tree, session_mgr)
  conftest.write_nightly_task(temp_home)
  node = await conftest.create_scheduled_node(tree, name="nightly", backend=conftest.OPUS_BACKEND_ID)
  cron_session = await conftest.make_cron_session(session_mgr, "nightly")

  with conftest.make_cron_sessions_client(cfg, session_mgr, tree) as client:
    response = client.delete("/api/cron/tasks/nightly")

  assert response.status_code == 200
  assert response.json() == {"ok": True}
  assert not (conftest.cron_d_dir(temp_home) / "nightly.yaml").exists()
  fresh_node = await session_mgr.store.get_session(node.id)
  assert fresh_node is not None
  assert fresh_node.status == models.SessionStatus.ACTIVE
  fresh_cron = await session_mgr.store.get_session(cron_session.id)
  assert fresh_cron is not None
  assert fresh_cron.status == models.SessionStatus.ACTIVE


@pytest.mark.asyncio
async def test_delete_keeps_the_node_dir_and_history(tmp_path: pathlib.Path, temp_home: pathlib.Path) -> None:
  """The delete removes only the config file: the bound node's directory and
  chat history stay on disk."""
  cfg, session_mgr, _scheduler = conftest.make_scheduler_setup(tmp_path)
  tree = task_sessions.TaskTreeManager(cfg, session_mgr)
  conftest.write_nightly_task(temp_home)
  node = await conftest.create_scheduled_node(tree, name="nightly", backend=conftest.OPUS_BACKEND_ID)
  events_path = session_mgr.get_chat_events_path(node.id)
  conftest.append_events(events_path, [conftest.user_event("e0")])

  with conftest.make_cron_sessions_client(cfg, session_mgr, tree) as client:
    response = client.delete("/api/cron/tasks/nightly")

  assert response.status_code == 200
  assert cfg.sessions_dir.joinpath(node.id).is_dir()
  events = conftest.read_chat_events(temp_home / "charliebot-home", node.id)
  assert conftest.user_event("e0") in events  # the node's history is intact
