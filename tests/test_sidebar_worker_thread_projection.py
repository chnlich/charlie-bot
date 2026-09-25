"""The sidebar's projected legacy worker-thread rows (project_worker_threads).

A legacy session (metadata profile None) stays a root row of every sidebar
list; the worker threads its old delegations left under
threads/*/metadata.json project as read-only worker-leaf rows. The contract
pinned here: the projected row's fields (description prefix, parent status,
thread times, running flag, backend, worker_thread origin), the full thread
scan (a thread older than the 30-day badge window still projects), the
transient exclusion (a later metadata save of the parent never carries
worker_thread to disk), and the list endpoint serving the leaves.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import OPUS_BACKEND_ID, make_home_config

from src.api.sessions import project_worker_threads
from src.core.models import CreateSessionRequest, SessionMetadata, SessionStatus, ThreadStatus
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager


async def build_env(tmp_path: Path):
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"), backend=OPUS_BACKEND_ID)
  return cfg, session_mgr, thread_mgr, legacy


async def write_thread_meta(thread_mgr: ThreadManager, thread, *, status: ThreadStatus,
                            started_at: datetime | None, completed_at: datetime | None,
                            backend: str | None = None) -> None:
  """Rewrite one thread's metadata.json with the wanted shape (the writers' file)."""
  thread.status = status
  thread.started_at = started_at
  thread.completed_at = completed_at
  if backend is not None:
    thread.backend = backend
  await thread_mgr._save_metadata(thread)


@pytest.mark.asyncio
async def test_projected_row_fields(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, legacy = await build_env(tmp_path)
  created = datetime.now(UTC) - timedelta(days=40)
  thread = await thread_mgr.create_thread(legacy, "Fix the login flow and document it")
  await write_thread_meta(
      thread_mgr, thread, status=ThreadStatus.COMPLETED, started_at=created + timedelta(minutes=1),
      completed_at=created + timedelta(minutes=9), backend=None)

  rows = await project_worker_threads([legacy], cfg, thread_mgr)

  assert [r.id for r in rows] == [legacy.id, thread.id]
  leaf = rows[1]
  assert leaf.name == "Fix the login flow and document it"[:80]
  assert leaf.profile == "worker"
  assert leaf.task_parent_id == legacy.id
  assert leaf.status == SessionStatus.ACTIVE  # the parent's status
  # Timestamps ride the list rows' epoch-ms wire form (ms precision).
  assert int(leaf.created_at.timestamp() * 1000) == int(thread.created_at.timestamp() * 1000)
  assert int(leaf.updated_at.timestamp() * 1000) == int(thread.completed_at.timestamp() * 1000)
  assert leaf.has_running_tasks is False
  assert leaf.backend == legacy.backend  # thread has none: the parent's rides
  assert leaf.worker_thread is not None
  assert leaf.worker_thread.session_id == legacy.id
  assert leaf.worker_thread.thread_id == thread.id


@pytest.mark.asyncio
async def test_projected_row_running_thread_and_name_prefix(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, legacy = await build_env(tmp_path)
  thread = await thread_mgr.create_thread(legacy, "x" * 120)
  await write_thread_meta(
      thread_mgr, thread, status=ThreadStatus.RUNNING, started_at=thread.created_at,
      completed_at=None, backend="codex-main")

  rows = await project_worker_threads([legacy], cfg, thread_mgr)

  leaf = rows[1]
  assert len(leaf.name) == 80
  # no completed_at: started_at wins the chain (ms precision)
  assert int(leaf.updated_at.timestamp() * 1000) == int(thread.started_at.timestamp() * 1000)
  assert leaf.has_running_tasks is True
  assert leaf.backend == "codex-main"  # the thread's own backend wins


@pytest.mark.asyncio
async def test_projection_scans_every_thread_no_time_window(tmp_path: Path) -> None:
  """A thread older than the 30-day badge window still projects: the scan is
  every threads/*/metadata.json, never the windowed recovery badge scan."""
  cfg, _session_mgr, thread_mgr, legacy = await build_env(tmp_path)
  old = await thread_mgr.create_thread(legacy, "ancient delegation")
  recent = await thread_mgr.create_thread(legacy, "recent delegation")
  for thread in (old, recent):
    await write_thread_meta(
        thread_mgr, thread, status=ThreadStatus.FAILED, started_at=thread.created_at,
        completed_at=thread.created_at)
  # Push the "ancient" thread's created_at behind the 30-day badge window.
  ancient = datetime.now(UTC) - timedelta(days=45)
  old.created_at = ancient
  await thread_mgr._save_metadata(old)

  rows = await project_worker_threads([legacy], cfg, thread_mgr)

  assert rows[0].id == legacy.id
  assert {r.id for r in rows[1:]} == {old.id, recent.id}
  old_leaf = next(r for r in rows if r.id == old.id)
  assert old_leaf.name == "ancient delegation"
  assert int(old_leaf.created_at.timestamp()) == int(ancient.timestamp())


@pytest.mark.asyncio
async def test_projection_writes_nothing_and_never_reaches_disk(tmp_path: Path) -> None:
  """Projection is read-only: the parent's metadata.json is byte-identical
  after it, and a later save of the parent (even one carrying a projected
  worker_thread in memory) persists no worker_thread key."""
  cfg, session_mgr, thread_mgr, legacy = await build_env(tmp_path)
  await thread_mgr.create_thread(legacy, "one delegation")
  meta_path = cfg.sessions_dir / legacy.id / "metadata.json"
  before = meta_path.read_bytes()

  rows = await project_worker_threads([legacy], cfg, thread_mgr)
  assert meta_path.read_bytes() == before

  parent_row = rows[0]
  parent_row.worker_thread = rows[1].worker_thread  # the worst in-memory shape
  await session_mgr.save_metadata(parent_row)
  stored = json.loads(meta_path.read_text(encoding="utf-8"))
  assert "worker_thread" not in stored


@pytest.mark.asyncio
async def test_non_legacy_rows_never_project(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, legacy = await build_env(tmp_path)
  await thread_mgr.create_thread(legacy, "one delegation")
  manager = SessionMetadata(id="task-node", name="Node", profile="manager")
  await thread_mgr.create_thread(manager, "node thread")  # shape only; never read

  rows = await project_worker_threads([manager], cfg, thread_mgr)

  assert [r.id for r in rows] == [manager.id]


@pytest.mark.asyncio
async def test_sessions_list_endpoint_serves_projected_leaves(tmp_path: Path) -> None:
  from fastapi import FastAPI
  from fastapi.testclient import TestClient

  from src.api import deps
  from src.api import sessions as sessions_api

  cfg, session_mgr, thread_mgr, legacy = await build_env(tmp_path)
  thread = await thread_mgr.create_thread(legacy, "endpoint delegation")
  await write_thread_meta(
      thread_mgr, thread, status=ThreadStatus.RUNNING, started_at=thread.created_at,
      completed_at=None)

  app = FastAPI()
  app.include_router(sessions_api.router, prefix="/api/sessions")
  app.dependency_overrides[deps.get_config] = lambda: cfg
  app.dependency_overrides[deps.get_config_on_loop] = lambda: cfg
  app.dependency_overrides[deps.get_session_manager] = lambda: session_mgr
  app.dependency_overrides[deps.get_thread_manager] = lambda: thread_mgr
  with TestClient(app) as client:
    body = client.get("/api/sessions/").json()
  ids = [row["id"] for row in body]
  assert ids == [legacy.id, thread.id]
  leaf = body[1]
  assert leaf["profile"] == "worker"
  assert leaf["task_parent_id"] == legacy.id
  assert leaf["has_running_tasks"] is True
  assert leaf["worker_thread"] == {"session_id": legacy.id, "thread_id": thread.id}
  parent = body[0]
  assert parent["profile"] is None
  assert "worker_thread" not in parent or parent["worker_thread"] is None

@pytest.mark.asyncio
async def test_projection_fanout_repeat_serves_the_same_rows(tmp_path: Path) -> None:
  """A fan-out wider than one single-session consumer serves the same row
  objects on the next projection.

  The sidebar list and the search route project every legacy row of one
  response, so the per-session row memos behind view_thread_rows hold dozens of
  sessions at once; a cap below that working set evicts between two requests,
  the re-walk rebuilds the row dicts, and the identity checks the projected-row
  memo and the search route's whole-body memo stand on fail on every request.
  """
  cfg, session_mgr, thread_mgr, _legacy = await build_env(tmp_path)
  parents = []
  for i in range(12):
    parent = await session_mgr.create_session(
        CreateSessionRequest(name=f"Legacy {i}"), backend=OPUS_BACKEND_ID)
    thread = await thread_mgr.create_thread(parent, f"Worker {i}")
    await write_thread_meta(
        thread_mgr, thread, status=ThreadStatus.COMPLETED, started_at=thread.created_at,
        completed_at=thread.created_at)
    parents.append(parent)

  first = await project_worker_threads(parents, cfg, thread_mgr)
  second = await project_worker_threads(parents, cfg, thread_mgr)

  assert [r.id for r in first] == [r.id for r in second]
  assert all(a is b for a, b in zip(first, second, strict=True)), \
      "a repeat projection rebuilt row objects; the consumers' identity-keyed memos cannot serve"
