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

from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
import pytest_asyncio
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import sessions as sessions_api
from src.api.deps import get_run_store, get_session_manager, get_task_manager
from src.core.models import RunRecord
from src.core.run_token import CallerIdentity
from src.core.runs import _encode_run_cursor
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager

OP = CallerIdentity(kind="operator")
BASE = datetime(2026, 1, 1, tzinfo=UTC)


@pytest_asyncio.fixture
async def store_env(tmp_path: Path):
  from conftest import make_home_config
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  tree = TaskTreeManager(cfg, session_mgr)
  task = await tree.create_task(
      request_id="t", task_parent_id=None, profile="worker", task=None, name="T",
      backend=None, caller=OP)
  return tree, task.id


async def seed_runs(tree: TaskTreeManager, session_id: str, launched: int, queued: int = 2) -> None:
  """``launched`` started runs in chronological order plus ``queued`` reservations."""
  for i in range(launched):
    await tree.runs.register_run(RunRecord(
        id=f"run-{i:04d}", session_id=session_id, kind="work",
        started_at=BASE + timedelta(minutes=i)))
  for q in range(queued):
    await tree.runs.register_run(RunRecord(id=f"queued-{q}", session_id=session_id, kind="work"))


# ---------------------------------------------------------------------------
# RunStore: descending read of the canonical order
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_one_row_descending_page_is_the_latest_started_run(store_env) -> None:
  tree, session_id = store_env
  await seed_runs(tree, session_id, launched=150)
  page = tree.runs.list_runs_page_sync(session_id, limit=1, cursor=None, descending=True)
  assert [r.id for r in page.items] == ["run-0149"], (
    "the newest-first page opens with the most recently started run, never an older page's tail")
  # A queued reservation never replaces a real launch.
  assert page.items[0].started_at is not None


@pytest.mark.asyncio
async def test_queued_only_session_reports_no_launch(store_env) -> None:
  tree, session_id = store_env
  await seed_runs(tree, session_id, launched=0, queued=3)
  page = tree.runs.list_runs_page_sync(session_id, limit=1, cursor=None, descending=True)
  assert len(page.items) == 1
  assert page.items[0].started_at is None, (
    "a queued-only session's newest-first row is a reservation, truthfully never a launch")


@pytest.mark.asyncio
async def test_adding_a_launch_moves_the_latest(store_env) -> None:
  tree, session_id = store_env
  await seed_runs(tree, session_id, launched=0, queued=1)
  before = tree.runs.list_runs_page_sync(session_id, limit=1, cursor=None, descending=True)
  assert before.items[0].started_at is None
  await tree.runs.register_run(RunRecord(
      id="run-first", session_id=session_id, started_at=BASE + timedelta(hours=1)))
  after = tree.runs.list_runs_page_sync(session_id, limit=1, cursor=None, descending=True)
  assert [r.id for r in after.items] == ["run-first"]


@pytest.mark.asyncio
async def test_equal_started_at_ties_use_the_canonical_id_order(store_env) -> None:
  tree, session_id = store_env
  same = BASE + timedelta(minutes=5)
  await tree.runs.register_run(RunRecord(id="run-b", session_id=session_id, started_at=same))
  await tree.runs.register_run(RunRecord(id="run-a", session_id=session_id, started_at=same))
  await tree.runs.register_run(RunRecord(id="run-c", session_id=session_id, started_at=same))
  page = tree.runs.list_runs_page_sync(session_id, limit=1, cursor=None, descending=True)
  assert page.items[0].id == "run-c", (
    "equal started_at ties resolve by the canonical (started_at, id) order's last element")
  whole = tree.runs.list_runs_page_sync(session_id, limit=10, cursor=None, descending=True)
  assert [r.id for r in whole.items] == ["run-c", "run-b", "run-a"]


@pytest.mark.asyncio
async def test_descending_pagination_walks_the_complete_order_without_loss(store_env) -> None:
  tree, session_id = store_env
  await seed_runs(tree, session_id, launched=57, queued=3)
  asc_ids: list[str] = []
  cursor = None
  while True:
    page = tree.runs.list_runs_page_sync(session_id, limit=10, cursor=cursor)
    asc_ids.extend(r.id for r in page.items)
    if page.next_cursor is None:
      break
    cursor = page.next_cursor
  desc_ids: list[str] = []
  cursor = None
  while True:
    page = tree.runs.list_runs_page_sync(session_id, limit=10, cursor=cursor, descending=True)
    desc_ids.extend(r.id for r in page.items)
    if page.next_cursor is None:
      break
    cursor = page.next_cursor
  assert len(asc_ids) == 60 and len(set(asc_ids)) == 60, "ascending pagination still covers every run once"
  assert desc_ids == list(reversed(asc_ids)), (
    "descending is exactly the canonical order read backwards: no missing or duplicated rows")


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


@pytest.mark.asyncio
async def test_pre_order_cursor_still_paginates_ascending(store_env) -> None:
  """A cursor a current client already holds (minted before the order flag
  existed) keeps working under the default ascending read."""
  tree, session_id = store_env
  await seed_runs(tree, session_id, launched=5)
  legacy_cursor = _encode_run_cursor((BASE + timedelta(minutes=1), "run-0001"))
  page = tree.runs.list_runs_page_sync(session_id, limit=10, cursor=legacy_cursor)
  assert [r.id for r in page.items] == ["run-0002", "run-0003", "run-0004"]


# ---------------------------------------------------------------------------
# API surface
# ---------------------------------------------------------------------------


@pytest_asyncio.fixture
async def api_env(store_env):
  tree, session_id = store_env
  app = FastAPI()
  app.include_router(sessions_api.router, prefix="/api/sessions")
  app.dependency_overrides[get_session_manager] = lambda: SessionManager(tree._cfg)
  app.dependency_overrides[get_task_manager] = lambda: tree
  app.dependency_overrides[get_run_store] = lambda: tree.runs
  return tree, session_id, TestClient(app)


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
async def test_runs_api_default_ascending_is_unchanged(api_env) -> None:
  tree, session_id, client = api_env
  await seed_runs(tree, session_id, launched=57, queued=3)
  resp = client.get(f"/api/sessions/{session_id}/runs", params={"limit": 10})
  assert resp.status_code == 200
  body = resp.json()
  assert [r["id"] for r in body["items"]][:4] == ["queued-0", "queued-1", "queued-2", "run-0000"], (
    "the default stays chronological with queued reservations first")
  seen: list[str] = [r["id"] for r in body["items"]]
  cursor = body["next_cursor"]
  while cursor:
    page = client.get(f"/api/sessions/{session_id}/runs", params={"limit": 10, "cursor": cursor}).json()
    seen.extend(r["id"] for r in page["items"])
    cursor = page["next_cursor"]
  assert len(seen) == 60 and len(set(seen)) == 60, "ordinary pagination returns the complete ordered set once"


@pytest.mark.asyncio
async def test_runs_api_order_validation(api_env) -> None:
  tree, session_id, client = api_env
  await seed_runs(tree, session_id, launched=5)
  bad = client.get(f"/api/sessions/{session_id}/runs", params={"order": "newest"})
  assert bad.status_code == 400
  assert "unknown runs order" in bad.json()["detail"]
  asc = client.get(f"/api/sessions/{session_id}/runs", params={"limit": 2}).json()
  mixed = client.get(
      f"/api/sessions/{session_id}/runs",
      params={"limit": 2, "order": "desc", "cursor": asc["next_cursor"]})
  assert mixed.status_code == 400, "an ascending cursor replayed under desc is an explicit 400"
  assert "ascending" in mixed.json()["detail"]
