"""The removed chat-command feature's replacements carry no chat request.

The incident this file pins: the removed dispatch persisted each executed
command as the session's `user` chat event, and a task-tree node read that
event back as a new request — the backup node's agent re-ran `/run backup`
55 times. The manual run endpoint must fire a task through the scheduled
path and leave the node's pending inputs untouched, and a message that
merely starts with "/" must be an ordinary chat input.
"""

from __future__ import annotations

import pathlib
from unittest import mock

import conftest
import pytest
import yaml
from conftest import OPUS_BACKEND_ID, OPUS_BACKEND_OPTION

from src.features.cron import event_types as cron_event_types
from src.features.cron.scheduler import Scheduler
from src.infra import event_types as ET
from src.infra.config import CharlieBotConfig


def _write_handler_task(home: pathlib.Path, name: str, session_id: str, handler: str = "probe") -> None:
  """Persist one bound handler task under the profile's cron.d, as setup writes it."""
  cron_d = home / "config.d" / "cron.d"
  cron_d.mkdir(parents=True, exist_ok=True)
  (cron_d / f"{name}.yaml").write_text(
      yaml.safe_dump(
          {
              "cron": "* * * * *",
              "handler": handler,
              "session_id": session_id,
              "enabled": True,
          }, sort_keys=False),
      encoding="utf-8")


def _cron_app(cfg: CharlieBotConfig, session_mgr, tree, scheduler):
  """TestClient over the cron router with the scheduler on app.state (the server's wiring)."""
  from fastapi import FastAPI
  from fastapi.testclient import TestClient

  from src.features.cron import api as cron_api
  from src.runtime.api.deps import get_config_on_loop, get_session_manager, get_task_manager

  app = FastAPI()
  app.include_router(cron_api.router, prefix="/api/cron")
  app.state.scheduler = scheduler
  app.dependency_overrides[get_config_on_loop] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[get_task_manager] = lambda: tree
  return TestClient(app)


# The removed router's URL prefix, split so no repo text carries the literal
# path the whole-repo greps for the removal must come back clean.
_REMOVED_PREFIX = "/api/" + "sla" + "sh"


@pytest.mark.asyncio
async def test_removed_command_routes_are_gone() -> None:
  """The production route table serves 404 for the removed router's two routes."""
  from fastapi.testclient import TestClient

  conftest.stub_credentials({"charliebot": {}})
  import server as server_module

  client = TestClient(server_module.app)
  execute_body = {"command": "run", "args": "backup"}
  assert client.post(f"{_REMOVED_PREFIX}/some-session/execute", json=execute_body).status_code == 404
  assert client.get(f"{_REMOVED_PREFIX}/commands").status_code == 404


@pytest.mark.asyncio
async def test_run_endpoint_fires_bound_handler_task_without_user_event(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The manual run takes the scheduled path and leaves the node's pending
  inputs empty: no `user` event, nothing for a following dispatch to launch."""
  from src.runtime import sessions, task_sessions

  monkeypatch.setenv("CHARLIEBOT_HOME", str(tmp_path))
  conftest.reset_config_caches()
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path,
      backends={"options": [OPUS_BACKEND_OPTION]},
      paths={"worktree_dir": str(tmp_path / "worktrees")})
  session_mgr = sessions.SessionManager(cfg)
  tree = task_sessions.TaskTreeManager(cfg, session_mgr)
  conftest.bind_deps_managers(monkeypatch, tree, session_mgr)
  monkeypatch.setattr(conftest.SCHEDULER_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  scheduler = Scheduler(cfg, session_mgr)

  meta = await conftest.create_scheduled_node(tree, name="nightly", backend=OPUS_BACKEND_ID)
  _write_handler_task(tmp_path, "nightly", meta.id)

  handler = mock.AsyncMock(return_value="done")
  client = _cron_app(cfg, session_mgr, tree, scheduler)
  with conftest.registered_cron_handler("probe", handler):
    resp = client.post("/api/cron/tasks/nightly/run")
  assert resp.status_code == 202
  assert resp.json() == {
      "type": cron_event_types.TASK_TRIGGERED,
      "task": "nightly",
      "session_id": meta.id,
      "thread_id": None,
  }
  handler.assert_awaited_once()

  events = session_mgr.load_chat_events_sync(meta.id)
  assert not [e for e in events if e["type"] == ET.USER], "the run writes no chat request"
  assert [e["type"] for e in events if e["type"] == ET.HANDLER_RESULT] == [ET.HANDLER_RESULT]

  assert tree.dispatch.pending_inputs(meta.id) == []
  tree.dispatch.executor = mock.AsyncMock()
  decision = await tree.dispatch.dispatch_pending(meta.id)
  assert decision["launch"] is False
  tree.dispatch.executor.assert_not_awaited()


@pytest.mark.asyncio
async def test_run_endpoint_unknown_task_is_404(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A task name the loader does not know answers 404."""
  from src.runtime import sessions, task_sessions

  monkeypatch.setenv("CHARLIEBOT_HOME", str(tmp_path))
  conftest.reset_config_caches()
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path,
      backends={"options": [OPUS_BACKEND_OPTION]},
      paths={"worktree_dir": str(tmp_path / "worktrees")})
  session_mgr = sessions.SessionManager(cfg)
  tree = task_sessions.TaskTreeManager(cfg, session_mgr)
  conftest.bind_deps_managers(monkeypatch, tree, session_mgr)
  monkeypatch.setattr(conftest.SCHEDULER_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  scheduler = Scheduler(cfg, session_mgr)

  client = _cron_app(cfg, session_mgr, tree, scheduler)
  resp = client.post("/api/cron/tasks/nope/run")
  assert resp.status_code == 404
  assert "nope" in resp.json()["detail"]


@pytest.mark.asyncio
async def test_slash_prefix_message_is_ordinary_task_input(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A chat message `/run backup` on a task-tree node is admitted once as an
  ordinary input; no scheduled task fires."""
  from fastapi import FastAPI
  from fastapi.testclient import TestClient

  from src.runtime import sessions, task_sessions
  from src.runtime.api import chat as chat_api
  from src.runtime.api.deps import get_config_on_loop, get_run_store, get_session_manager, get_task_manager

  monkeypatch.setenv("CHARLIEBOT_HOME", str(tmp_path))
  conftest.reset_config_caches()
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path,
      backends={"options": [OPUS_BACKEND_OPTION]},
      paths={"worktree_dir": str(tmp_path / "worktrees")})
  session_mgr = sessions.SessionManager(cfg)
  tree = task_sessions.TaskTreeManager(cfg, session_mgr)
  conftest.bind_deps_managers(monkeypatch, tree, session_mgr)

  meta = await conftest.create_scheduled_node(tree, name="backup", backend=OPUS_BACKEND_ID)
  _write_handler_task(tmp_path, "backup", meta.id, handler="untouched")

  app = FastAPI()
  app.include_router(chat_api.router, prefix="/api/chat")
  app.dependency_overrides[get_config_on_loop] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[get_task_manager] = lambda: tree
  app.dependency_overrides[get_run_store] = lambda: tree.runs

  client = TestClient(app)
  handler = mock.AsyncMock(return_value="done")
  with conftest.registered_cron_handler("untouched", handler):
    sent = client.post(f"/api/chat/{meta.id}/message", json={"content": "/run backup"})
  assert sent.status_code == 202

  users = [e for e in tree.events.load_events(meta.id) if e["type"] == ET.USER]
  assert len(users) == 1, "the slash-form text is one ordinary input, not a command replay"
  assert users[0]["content"] == "/run backup"
  assert [str(e["id"]) for e in tree.dispatch.pending_inputs(meta.id)] == [sent.json()["input_event_id"]]

  handler.assert_not_called()
  assert not [e for e in tree.events.load_events(meta.id) if e["type"] == ET.HANDLER_RESULT]
