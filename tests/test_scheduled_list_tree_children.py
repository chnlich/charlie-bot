"""The scheduled listing carries each cron session's task-tree children.

A cron firing's worker leaf is a real task-tree node parented to the cron
session; the scheduled rows ride their children so the grouped sidebar nests
the leaf under its cron session — a legacy (profile None) session's included.
"""

from pathlib import Path

import pytest
from conftest import make_home_config
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.deps import get_config_on_loop, get_session_manager, get_thread_manager
from src.api.sessions import router as sessions_router
from src.core.models import CreateSessionRequest, TaskSpec
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager
from src.core.threads import ThreadManager


@pytest.mark.asyncio
async def test_scheduled_list_nests_task_children_under_their_cron_session(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  tree = TaskTreeManager(cfg, session_mgr)
  cron_session = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: nightly-sweep", scheduled_task="nightly-sweep"), backend="opus")
  leaf = await tree.create_task(
      request_id="leaf-1",
      task_parent_id=cron_session.id,
      profile="worker",
      task=TaskSpec(goal="sweep"),
      name="nightly-sweep · firing-1",
      backend=None,
      caller="system")
  await tree.create_task(
      request_id="root-other",
      task_parent_id=None,
      profile="manager",
      task=TaskSpec(goal="unrelated"),
      name="Unrelated root",
      backend=None,
      caller="system")

  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_config_on_loop] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[get_thread_manager] = lambda: ThreadManager(cfg)
  client = TestClient(app)

  resp = client.get("/api/sessions/scheduled")
  assert resp.status_code == 200
  rows = resp.json()
  ids = [r["id"] for r in rows]
  # The leaf rides directly after its cron session; the unrelated tree root
  # stays out of the scheduled listing.
  assert ids == [cron_session.id, leaf.id]
  by_id = {r["id"]: r for r in rows}
  assert by_id[leaf.id]["task_parent_id"] == cron_session.id
  assert by_id[cron_session.id]["scheduled_task"] == "nightly-sweep"
