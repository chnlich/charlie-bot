"""The cron subtree (every session whose parent chain reaches a scheduled_task session)
rides only the Scheduled listing, nested under its own cron session.

All (route + homepage render) and Archived return no cron-subtree rows — the
projected legacy worker-thread rows under cron sessions included — while the
archived cron sessions themselves keep their rows and the keyset pagination and
group aggregates describe the rows actually returned. Search and Starred keep
today's behavior, and ``list_sessions(scheduled=True)`` still returns exactly the
cron sessions (the scheduler's lookup depends on it).
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass
from pathlib import Path

import pytest
from conftest import OPUS_BACKEND_ID, apply_config_overrides, build_env, create_task, seed_thread
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.deps import get_config_on_loop, get_session_manager, get_task_manager, get_thread_manager
from src.api.pages import router as pages_router
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig
from src.core.models import (
    CreateSessionRequest,
    RunRecord,
    SessionMetadata,
    SessionStatus,
    ThreadMetadata,
)
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager
from src.core.threads import ThreadManager


@dataclass
class Fixture:
  """One corpus exercising every cron-subtree classification the sidebar lists make."""

  cfg: CharlieBotConfig
  session_mgr: SessionManager
  tree: TaskTreeManager
  thread_mgr: ThreadManager
  cron: SessionMetadata                   # active cron session (scheduled_task set)
  manager: SessionMetadata                # active manager child of the cron session
  worker: SessionMetadata                 # active direct worker child of the cron session
  grandchild: SessionMetadata             # active worker under the manager child
  delivered: SessionMetadata              # worker under the manager child, archived by derivation
  cron_archived: SessionMetadata          # archived cron session with a legacy worker thread
  cron_archived_thread: ThreadMetadata    # its projected leaf row (excluded from Archived)
  plain_archived: SessionMetadata         # archived non-cron session with a legacy worker thread
  plain_archived_thread: ThreadMetadata   # its projected leaf row (stays in Archived)
  plain_active: SessionMetadata           # active non-cron session with a legacy worker thread
  plain_active_thread: ThreadMetadata     # its projected leaf row (stays in All)
  ordinary: SessionMetadata               # active plain session
  fillers: list[SessionMetadata]          # archived plain sessions padding the keyset walk


async def _build_fixture(tmp_path: Path) -> Fixture:
  cfg, session_mgr, tree = build_env(tmp_path)
  thread_mgr = ThreadManager(cfg)
  cron = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: nightly", scheduled_task="nightly"),
      backend=OPUS_BACKEND_ID)
  manager = await create_task(tree, parent=cron.id, request_id="mgr-1", profile="manager",
                              name="nightly · manager")
  worker = await create_task(tree, parent=cron.id, request_id="leaf-42", profile="worker",
                             name="nightly · firing-42")
  grandchild = await create_task(tree, parent=manager.id, request_id="leaf-43", profile="worker",
                                 name="nightly · firing-43")
  # The delivered leaf parents under the manager child: a legacy parent's wake
  # launches its master turn (a real backend spawn), a task-tree parent's wake
  # rides the dispatcher. The leaf stays a cron-subtree row — its chain still
  # reaches the cron session through the manager.
  delivered = await create_task(tree, parent=manager.id, request_id="leaf-44", profile="worker",
                                name="nightly · firing-44")
  await tree.runs.register_run(RunRecord(id="run-44", session_id=delivered.id, kind="work"))
  await tree.dispatch.finish_run(delivered.id, "run-44", outcome="success")
  assert tree.task_state(delivered.id) == "completed"  # the derived archive hides it while active

  cron_archived = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: legacy-task", scheduled_task="legacy-task"),
      backend=OPUS_BACKEND_ID)
  cron_archived_thread = await seed_thread(thread_mgr, cron_archived, "legacy cron thread")
  await session_mgr.archive_session(cron_archived.id)
  plain_archived = await session_mgr.create_session(
      CreateSessionRequest(name="Plain archived"), backend=OPUS_BACKEND_ID)
  plain_archived_thread = await seed_thread(thread_mgr, plain_archived, "plain archived thread")
  await session_mgr.archive_session(plain_archived.id)
  plain_active = await session_mgr.create_session(
      CreateSessionRequest(name="Plain active"), backend=OPUS_BACKEND_ID)
  plain_active_thread = await seed_thread(thread_mgr, plain_active, "plain active thread")
  ordinary = await session_mgr.create_session(
      CreateSessionRequest(name="Ordinary"), backend=OPUS_BACKEND_ID)
  fillers = []
  for i in range(3):
    filler = await session_mgr.create_session(
        CreateSessionRequest(name=f"Filler {i}"), backend=OPUS_BACKEND_ID)
    await session_mgr.archive_session(filler.id)
    fillers.append(filler)
  return Fixture(
      cfg=cfg, session_mgr=session_mgr, tree=tree, thread_mgr=thread_mgr,
      cron=cron, manager=manager, worker=worker, grandchild=grandchild, delivered=delivered,
      cron_archived=cron_archived, cron_archived_thread=cron_archived_thread,
      plain_archived=plain_archived, plain_archived_thread=plain_archived_thread,
      plain_active=plain_active, plain_active_thread=plain_active_thread,
      ordinary=ordinary, fillers=fillers)


# Every row the cron-subtree rule excludes from All and Archived, by fixture name.
_CRON_SUBTREE_ROWS = ("cron", "manager", "worker", "grandchild", "delivered",
                      "cron_archived", "cron_archived_thread")


def _subtree_ids(fx: Fixture) -> set[str]:
  return {getattr(fx, name).id for name in _CRON_SUBTREE_ROWS}


def _api_client(fx: Fixture) -> TestClient:
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_config_on_loop] = lambda: fx.cfg
  app.dependency_overrides[get_session_manager] = lambda: fx.session_mgr
  app.dependency_overrides[get_task_manager] = lambda: fx.tree
  app.dependency_overrides[get_thread_manager] = lambda: fx.thread_mgr
  return TestClient(app)


def _page_client(fx: Fixture) -> TestClient:
  app = FastAPI()
  app.include_router(pages_router)
  apply_config_overrides(app, fx.cfg)
  app.dependency_overrides[get_session_manager] = lambda: fx.session_mgr
  app.dependency_overrides[get_task_manager] = lambda: fx.tree
  app.dependency_overrides[get_thread_manager] = lambda: fx.thread_mgr
  return TestClient(app)


def _initial_sessions(page_client: TestClient, session_id: str) -> list[dict]:
  resp = page_client.get("/", params={"session": session_id})
  assert resp.status_code == 200
  match = re.search(r"const INITIAL_SESSIONS = (\[.*?\]);", resp.text)
  assert match is not None
  return json.loads(match.group(1))


@pytest.mark.asyncio
async def test_all_list_and_homepage_exclude_the_cron_subtree(tmp_path: Path) -> None:
  fx = await _build_fixture(tmp_path)
  client = _api_client(fx)
  resp = client.get("/api/sessions/")
  assert resp.status_code == 200
  all_ids = {row["id"] for row in resp.json()}
  # The active rows that never belong to a cron subtree stay.
  assert {fx.ordinary.id, fx.plain_active.id} <= all_ids
  assert fx.plain_active_thread.id in all_ids  # a non-cron legacy thread leaf keeps its row
  # No cron-subtree row: neither the cron sessions nor any of their descendants.
  assert not (all_ids & _subtree_ids(fx))

  # The homepage's first-paint list carries the same membership, and the
  # auto-redirect never lands on a cron-subtree row.
  page_client = _page_client(fx)
  redirect = page_client.get("/", follow_redirects=False)
  assert redirect.status_code in (301, 302, 307)
  assert redirect.headers["location"].split("session=")[1] not in _subtree_ids(fx)
  row_ids = {row["id"] for row in _initial_sessions(page_client, fx.ordinary.id)}
  assert {fx.ordinary.id, fx.plain_active.id} <= row_ids
  assert not (row_ids & _subtree_ids(fx))


@pytest.mark.asyncio
async def test_scheduled_list_nests_the_full_active_subtree(tmp_path: Path) -> None:
  fx = await _build_fixture(tmp_path)
  client = _api_client(fx)
  resp = client.get("/api/sessions/scheduled")
  assert resp.status_code == 200
  rows = resp.json()
  ids = [row["id"] for row in rows]
  # The active cron session rides first, then its whole active subtree at any
  # depth (the manager child, the direct leaf, and the grandchild); the
  # delivered (derived-archived) leaf and the archived cron session stay out.
  assert ids[0] == fx.cron.id
  assert set(ids[1:]) == {fx.manager.id, fx.worker.id, fx.grandchild.id}
  by_id = {row["id"]: row for row in rows}
  assert by_id[fx.worker.id]["task_parent_id"] == fx.cron.id
  assert by_id[fx.grandchild.id]["task_parent_id"] == fx.manager.id
  assert by_id[fx.manager.id]["scheduled_task"] is None  # children never carry the mark


@pytest.mark.asyncio
async def test_archived_list_excludes_cron_subtree_rows_and_paginates(tmp_path: Path) -> None:
  fx = await _build_fixture(tmp_path)
  client = _api_client(fx)
  resp = client.get("/api/sessions/archived")
  assert resp.status_code == 200
  page = resp.json()
  ids = {row["id"] for row in page["sessions"]}
  # The archived cron session itself and the non-cron legacy thread row stay.
  assert fx.cron_archived.id in ids
  assert fx.plain_archived.id in ids
  assert fx.plain_archived_thread.id in ids
  # No cron-subtree row: neither the delivered child (archived by derivation)
  # nor the archived cron session's own legacy thread row.
  assert not (ids & {fx.delivered.id, fx.cron_archived_thread.id})
  # The aggregates describe the rows actually returned: the five archived
  # sessions that survive the exclusion (the delivered child stays out).
  assert page["groups"] == [{"group": None, "total": 5}]

  # A small-limit walk returns every non-excluded archived row exactly once,
  # with every page but the last full.
  expected = {
      fx.cron_archived.id, fx.plain_archived.id, fx.plain_archived_thread.id,
      *(filler.id for filler in fx.fillers)}
  walk: list[str] = []
  before = None
  before_id = None
  for _ in range(10):
    params: dict = {"limit": 2}
    if before is not None:
      params.update({"before": before, "before_id": before_id})
    resp = client.get("/api/sessions/archived", params=params)
    assert resp.status_code == 200
    page = resp.json()
    real_rows = [row for row in page["sessions"] if row["worker_thread"] is None]
    if page["has_more"]:
      assert len(real_rows) == 2  # a page stays full when more rows exist
    walk.extend(row["id"] for row in page["sessions"])
    if not page["has_more"]:
      break
    before, before_id = page["next_before"], page["next_before_id"]
  assert len(walk) == len(set(walk))  # the cursor never repeats a row
  assert set(walk) == expected


@pytest.mark.asyncio
async def test_search_and_starred_keep_their_rows(tmp_path: Path) -> None:
  fx = await _build_fixture(tmp_path)
  client = _api_client(fx)
  resp = client.get("/api/sessions/search", params={"q": "firing-42"})
  assert resp.status_code == 200
  assert fx.worker.id in {row["id"] for row in resp.json()}
  # The empty-query path still serves the unfiltered active list.
  resp = client.get("/api/sessions/search")
  assert resp.status_code == 200
  assert {fx.cron.id, fx.worker.id} <= {row["id"] for row in resp.json()}
  # Starred keeps today's behavior: a starred cron session still lists.
  await fx.session_mgr.star_session(fx.cron.id)
  resp = client.get("/api/sessions/starred")
  assert resp.status_code == 200
  assert fx.cron.id in {row["id"] for row in resp.json()}


@pytest.mark.asyncio
async def test_scheduled_filter_returns_only_cron_sessions(tmp_path: Path) -> None:
  fx = await _build_fixture(tmp_path)
  rows = await fx.session_mgr.list_sessions(status=SessionStatus.ACTIVE, scheduled=True)
  assert [row.id for row in rows] == [fx.cron.id]
  assert all(row.scheduled_task is not None for row in rows)
