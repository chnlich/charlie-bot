"""Tests for cron-task deletion: the yaml goes, everything else stays."""

from pathlib import Path

import pytest
from conftest import (
    OPUS_BACKEND_ID,
    append_events,
    apply_config_overrides,
    cron_d_dir,
    dump_yaml,
    make_legacy_cron_session,
    make_scheduler_setup,
    read_chat_events,
    user_event,
    write_cron_task,
    write_nightly_prompt,
)
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.cron import router as cron_router
from src.api.deps import get_session_manager
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig
from src.core.models import SessionMetadata, SessionStatus
from src.core.sessions import SessionManager


def make_cron_sessions_client(cfg: CharlieBotConfig, session_mgr: SessionManager) -> TestClient:
  """TestClient mounting the cron router plus the sessions router (the scheduled listing and
  unarchive endpoints) with cfg/session_mgr as dependency overrides."""
  app = FastAPI()
  app.include_router(cron_router, prefix="/api/cron")
  app.include_router(sessions_router, prefix="/api/sessions")
  apply_config_overrides(app, cfg)
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  return TestClient(app)


def write_nightly_task(home: Path) -> Path:
  """Seed one healthy 'nightly' cron job (pointer-backed host file, as production files look)
  and return its yaml path."""
  prompt_path = write_nightly_prompt(home, "nightly prompt")
  return write_cron_task(
      home,
      "nightly",
      dump_yaml(
          {
              "cron": "0 3 * * *",
              "prompt_file": str(prompt_path),
              "timezone": "America/Los_Angeles",
              "enabled": True,
          }),
  )


@pytest.mark.asyncio
async def test_delete_unlinks_the_yaml_and_leaves_the_bound_node_untouched(
    tmp_path: Path, temp_home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Deleting a task archives nothing: the yaml goes, the task's bound node
  stays exactly as it is (it is the user's task-tree node, not the deletion's
  product), and no legacy cron session is archived either."""
  cfg, session_mgr, scheduler = make_scheduler_setup(tmp_path)
  from src.api import deps
  from src.core.task_sessions import TaskTreeManager
  tree = TaskTreeManager(cfg, session_mgr)
  monkeypatch.setattr(deps, "_task_manager", tree)
  monkeypatch.setattr(deps, "_session_manager", session_mgr)
  write_nightly_task(temp_home)
  node = await tree.create_task(
      request_id="scheduled-node:nightly",
      task_parent_id=None,
      profile="manager",
      task=None,
      name="nightly",
      backend=OPUS_BACKEND_ID,
      caller="system")
  cron_session = await make_legacy_cron_session(session_mgr, "nightly")

  with make_cron_sessions_client(cfg, session_mgr) as client:
    response = client.delete("/api/cron/tasks/nightly")

  assert response.status_code == 200
  assert response.json() == {"ok": True}
  assert not (cron_d_dir(temp_home) / "nightly.yaml").exists()
  fresh_node = await session_mgr.get_session(node.id)
  assert fresh_node is not None
  assert fresh_node.status == SessionStatus.ACTIVE
  fresh_cron = await session_mgr.get_session(cron_session.id)
  assert fresh_cron is not None
  assert fresh_cron.status == SessionStatus.ACTIVE


@pytest.mark.asyncio
async def test_delete_keeps_the_node_dir_and_history(tmp_path: Path, temp_home: Path) -> None:
  """The delete removes only the config file: the bound node's directory and
  chat history stay on disk."""
  cfg, session_mgr, _scheduler = make_scheduler_setup(tmp_path)
  from src.core.task_sessions import TaskTreeManager
  tree = TaskTreeManager(cfg, session_mgr)
  write_nightly_task(temp_home)
  node = await tree.create_task(
      request_id="scheduled-node:nightly",
      task_parent_id=None,
      profile="manager",
      task=None,
      name="nightly",
      backend=OPUS_BACKEND_ID,
      caller="system")
  events_path = session_mgr.get_chat_events_path(node.id)
  append_events(events_path, [user_event("e0")])

  with make_cron_sessions_client(cfg, session_mgr) as client:
    response = client.delete("/api/cron/tasks/nightly")

  assert response.status_code == 200
  assert cfg.sessions_dir.joinpath(node.id).is_dir()
  events = read_chat_events(temp_home / "charliebot-home", node.id)
  assert user_event("e0") in events  # the node's history is intact
