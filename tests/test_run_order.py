"""The descending read of the canonical run order: the authoritative latest launch.

The original defect: the Context panel's current-run selection read one
ascending page (``/runs?limit=100``, queued first) and picked the last row
carrying a snapshot ref — a session with more than one page of launched runs
showed an older run, and a page filled by unlaunched rows reported "no run"
even when later pages held real launches. The fix puts latest-Run selection on
the run owner: ``order=desc`` reads the same canonical (started_at, id) order
backwards, so a one-row descending page IS the session's latest started run at
a request cost that never grows with history length. The ascending default and
its cursor semantics stay exactly as they were.
"""

from __future__ import annotations

import datetime
import pathlib

import conftest
import fastapi
import pytest
import pytest_asyncio
from fastapi import testclient

from src.infra import models
from src.runtime import sessions, task_sessions
from src.runtime.api import deps
from src.runtime.api import sessions as sessions_api
from src.runtime.session_store import SessionStore

BASE = datetime.datetime(2026, 1, 1, tzinfo=datetime.UTC)


@pytest_asyncio.fixture
async def store_env(tmp_path: pathlib.Path):
  cfg = conftest.make_home_config(tmp_path)
  session_mgr = sessions.SessionManager(cfg, SessionStore(cfg))
  tree = task_sessions.TaskTreeManager(cfg, session_mgr)
  task = await tree.create_task(
      request_id="t",
      task_parent_id=None,
      profile="worker",
      task=None,
      name="T",
      backend=None,
      caller=conftest.OPERATOR)
  return tree, task.id


async def seed_runs(tree: task_sessions.TaskTreeManager, session_id: str, launched: int, queued: int = 2) -> None:
  """``launched`` started runs in chronological order plus ``queued`` reservations."""
  for i in range(launched):
    await tree.runs.register_run(
        models.RunRecord(
            id=f"run-{i:04d}", session_id=session_id, kind="work", started_at=BASE + datetime.timedelta(minutes=i)))
  for q in range(queued):
    await tree.runs.register_run(models.RunRecord(id=f"queued-{q}", session_id=session_id, kind="work"))


# ---------------------------------------------------------------------------
# RunStore: descending read of the canonical order
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_one_row_descending_page_is_the_latest_started_run(store_env) -> None:
  tree, session_id = store_env
  await seed_runs(tree, session_id, launched=150)
  page = tree.runs.list_runs_page_sync(session_id, limit=1, cursor=None, descending=True)
  assert [r.id for r in page.items
         ] == ["run-0149"
              ], ("the newest-first page opens with the most recently started run, never an older page's tail")
  # A queued reservation never replaces a real launch.
  assert page.items[0].started_at is not None


@pytest.mark.asyncio
async def test_cursor_minted_under_one_order_is_refused_under_the_other(store_env) -> None:
  tree, session_id = store_env
  await seed_runs(tree, session_id, launched=5)
  asc = tree.runs.list_runs_page_sync(session_id, limit=2, cursor=None)
  assert asc.next_cursor is not None
  with pytest.raises(ValueError, match="ascending"):
    tree.runs.list_runs_page_sync(session_id, limit=2, cursor=asc.next_cursor, descending=True)
  desc = tree.runs.list_runs_page_sync(session_id, limit=2, cursor=None, descending=True)
  assert desc.next_cursor is not None
  with pytest.raises(ValueError, match="descending"):
    tree.runs.list_runs_page_sync(session_id, limit=2, cursor=desc.next_cursor)


# ---------------------------------------------------------------------------
# API surface
# ---------------------------------------------------------------------------


@pytest_asyncio.fixture
async def api_env(store_env):
  tree, session_id = store_env
  app = fastapi.FastAPI()
  app.include_router(sessions_api.router, prefix="/api/sessions")
  app.dependency_overrides[
      deps.get_session_manager] = lambda: sessions.SessionManager(tree._cfg, SessionStore(tree._cfg))
  app.dependency_overrides[deps.get_session_store] = lambda: SessionStore(tree._cfg)
  app.dependency_overrides[deps.get_task_manager] = lambda: tree
  app.dependency_overrides[deps.get_run_store] = lambda: tree.runs
  return tree, session_id, testclient.TestClient(app)


@pytest.mark.asyncio
async def test_runs_api_desc_opens_with_the_latest_started_run(api_env) -> None:
  tree, session_id, client = api_env
  await seed_runs(tree, session_id, launched=150)
  resp = client.get(f"/api/sessions/{session_id}/runs", params={"limit": 1, "order": "desc"})
  assert resp.status_code == 200, resp.text
  items = resp.json()["items"]
  assert len(items) == 1 and items[0]["id"] == "run-0149"
  assert items[0]["started_at"] is not None
  # The selection is by launch order, not by snapshot-evidence presence: the
  # newest run carries no snapshot here and is still the one returned.
  assert items[0]["prompt_snapshot_ref"] is None


@pytest.mark.asyncio
async def test_runs_api_order_validation(api_env) -> None:
  tree, session_id, client = api_env
  await seed_runs(tree, session_id, launched=5)
  bad = client.get(f"/api/sessions/{session_id}/runs", params={"order": "newest"})
  assert bad.status_code == 400
  assert "unknown runs order" in bad.json()["detail"]
  asc = client.get(f"/api/sessions/{session_id}/runs", params={"limit": 2}).json()
  mixed = client.get(
      f"/api/sessions/{session_id}/runs", params={
          "limit": 2,
          "order": "desc",
          "cursor": asc["next_cursor"]
      })
  assert mixed.status_code == 400, "an ascending cursor replayed under desc is an explicit 400"
  assert "ascending" in mixed.json()["detail"]
