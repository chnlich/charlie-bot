"""The sidebar lists' row schedule fields (plan 4.1) and the archived page's context rows.

One join (``row_schedule_fields``, keyed on ``bound_task_name``) stamps every
listed row — GET /api/sessions/, /starred, /archived, and the homepage's
server-rendered sidebar: a node a loaded task binds carries ``schedule_task``
plus the four schedule fields computed from that task config; an unbound row
carries ``schedule_task: null`` and none of the four. The Scheduled listing and
its endpoint are gone (404). Each archived page also carries its rows'
unarchived ancestors as ``context_only`` rows — never counted into page size,
cursor, or group aggregates, each archived row returned exactly once across a
full walk — and the cron-subtree exclusion still keeps the legacy cron session
and everything under it out.
"""

from __future__ import annotations

import json
import re
from pathlib import Path

import pytest
from conftest import (
    OPUS_BACKEND_ID,
    apply_config_overrides,
    build_env,
    create_task,
    cron_d_dir,
    dump_yaml,
    make_legacy_cron_session,
)
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.deps import get_config_on_loop, get_session_manager, get_task_manager, get_thread_manager
from src.api.pages import router as pages_router
from src.api.sessions import router as sessions_router
from src.core.models import CreateSessionRequest, RunRecord, SessionStatus
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager
from src.core.threads import ThreadManager


def _write_bound_task(home: Path, name: str, session_id: str, *, enabled: bool = True) -> None:
  """One synthetic cron task bound to *session_id* (the join's input shape).

  A cron.d host file carries ``prompt_file`` — the pointer to the file owning
  the prompt body — never an inline prompt (the loader rejects that shape).
  """
  prompt_path = home / "prompts" / f"{name}.md"
  prompt_path.parent.mkdir(parents=True, exist_ok=True)
  prompt_path.write_text(f"synthetic prompt for {name}\n", encoding="utf-8")
  path = cron_d_dir(home) / f"{name}.yaml"
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(
      dump_yaml(
          {
              "cron": "0 3 * * *",
              "prompt_file": str(prompt_path),
              "timezone": "America/Los_Angeles",
              "enabled": enabled,
              "session_id": session_id,
          }),
      encoding="utf-8")


def _api_client(cfg, session_mgr: SessionManager, tree: TaskTreeManager, thread_mgr: ThreadManager) -> TestClient:
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  apply_config_overrides(app, cfg)
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[get_task_manager] = lambda: tree
  app.dependency_overrides[get_thread_manager] = lambda: thread_mgr
  return TestClient(app)


def _page_client(cfg, session_mgr: SessionManager, tree: TaskTreeManager, thread_mgr: ThreadManager) -> TestClient:
  app = FastAPI()
  app.include_router(pages_router)
  apply_config_overrides(app, cfg)
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[get_task_manager] = lambda: tree
  app.dependency_overrides[get_thread_manager] = lambda: thread_mgr
  return TestClient(app)


def _initial_sessions(page_client: TestClient, session_id: str) -> list[dict]:
  resp = page_client.get("/", params={"session": session_id})
  assert resp.status_code == 200
  match = re.search(r"const INITIAL_SESSIONS = (\[.*?\]);", resp.text)
  assert match is not None
  return json.loads(match.group(1))


def _assert_bound_row(row: dict, task_name: str, *, enabled: bool) -> None:
  """The join's full answer for one bound node's row."""
  assert row["schedule_task"] == task_name
  assert row["schedule_cron"] == "0 3 * * *"
  assert row["schedule_timezone"] == "America/Los_Angeles"
  assert row["schedule_enabled"] is enabled
  assert row["schedule_next_run"]  # ISO 8601, computed the way the Scheduled rows computed it


def _assert_unbound_row(row: dict) -> None:
  """schedule_task null and none of the four schedule fields."""
  assert row["schedule_task"] is None
  for field in ("schedule_cron", "schedule_timezone", "schedule_enabled", "schedule_next_run"):
    assert field not in row, field


@pytest.mark.asyncio
async def test_bound_and_unbound_rows_carry_the_join_answer_in_every_list(tmp_path: Path, temp_home: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  thread_mgr = ThreadManager(cfg)

  async def manager(name: str, request_id: str, group: str | None = None):
    node = await create_task(tree, parent=None, request_id=request_id, profile="manager", name=name)
    if group is not None:
      node.group = group
      await session_mgr.save_metadata(node)
    return node

  bound = await manager("synthetic-daily", "bind-1", "Synthetic")
  disabled_node = await manager("synthetic-paused", "bind-2", "Synthetic")
  unbound = await manager("Plain root", "plain-1", "Synthetic")
  _write_bound_task(temp_home, "synthetic-daily", bound.id)
  _write_bound_task(temp_home, "synthetic-paused", disabled_node.id, enabled=False)
  client = _api_client(cfg, session_mgr, tree, thread_mgr)

  for url in ("/api/sessions/", "/api/sessions/starred"):
    if url.endswith("starred"):
      for node in (bound, disabled_node, unbound):
        await session_mgr.star_session(node.id)
    resp = client.get(url)
    assert resp.status_code == 200, (url, resp.text)
    by_id = {row["id"]: row for row in resp.json()}
    assert {bound.id, disabled_node.id, unbound.id} <= set(by_id), url
    _assert_bound_row(by_id[bound.id], "synthetic-daily", enabled=True)
    _assert_bound_row(by_id[disabled_node.id], "synthetic-paused", enabled=False)
    _assert_unbound_row(by_id[unbound.id])

  # The homepage's server-rendered sidebar carries the same answer.
  rows = {row["id"]: row for row in _initial_sessions(_page_client(cfg, session_mgr, tree, thread_mgr), unbound.id)}
  _assert_bound_row(rows[bound.id], "synthetic-daily", enabled=True)
  _assert_bound_row(rows[disabled_node.id], "synthetic-paused", enabled=False)
  _assert_unbound_row(rows[unbound.id])


@pytest.mark.asyncio
async def test_archived_bound_row_keeps_the_join_and_the_scheduled_endpoint_is_gone(
    tmp_path: Path, temp_home: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  thread_mgr = ThreadManager(cfg)
  bound = await create_task(tree, parent=None, request_id="bind-1", profile="manager", name="synthetic-daily")
  _write_bound_task(temp_home, "synthetic-daily", bound.id)
  await session_mgr.archive_session(bound.id)
  client = _api_client(cfg, session_mgr, tree, thread_mgr)

  resp = client.get("/api/sessions/archived")
  assert resp.status_code == 200
  page = resp.json()
  row = next(r for r in page["sessions"] if r["id"] == bound.id)
  _assert_bound_row(row, "synthetic-daily", enabled=True)
  assert "context_only" not in row  # the mark exists only on context rows

  # The Scheduled listing and its endpoint are deleted.
  assert client.get("/api/sessions/scheduled").status_code == 404


@pytest.mark.asyncio
async def test_archived_pages_carry_active_ancestors_as_context_only_rows(tmp_path: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  thread_mgr = ThreadManager(cfg)
  root = await create_task(tree, parent=None, request_id="root-1", profile="manager", name="Synthetic root")
  root.group = "Alpha"
  await session_mgr.save_metadata(root)
  # A derived-archived intermediate (presentation hidden): it is itself an
  # archived row, and the walk climbs past it to the active root.
  hidden = await create_task(
      tree, parent=root.id, request_id="mid-1", profile="manager", name="Synthetic hidden middle")
  await tree.set_presentation(hidden.id, "hidden")
  delivered = await create_task(
      tree, parent=hidden.id, request_id="leaf-1", profile="worker", name="Synthetic delivered")
  await tree.runs.register_run(RunRecord(id="run-1", session_id=delivered.id, kind="work"))
  await tree.dispatch.finish_run(delivered.id, "run-1", outcome="success")
  assert tree.task_state(delivered.id) == "completed"  # archived by derivation

  # A stored-archived legacy child directly under the active root.
  legacy_child = await session_mgr.create_session(
      CreateSessionRequest(name="Synthetic legacy child"), backend=OPUS_BACKEND_ID)
  legacy_child.task_parent_id = root.id
  await session_mgr.save_metadata(legacy_child)
  await session_mgr.archive_session(legacy_child.id)

  # Fillers push the archived rows past one small page.
  fillers = []
  for i in range(3):
    filler = await session_mgr.create_session(CreateSessionRequest(name=f"Filler {i}"), backend=OPUS_BACKEND_ID)
    await session_mgr.archive_session(filler.id)
    fillers.append(filler)

  client = _api_client(cfg, session_mgr, tree, thread_mgr)
  archived: list[str] = []
  context: list[str] = []
  before = None
  before_id = None
  page: dict = {}
  for _ in range(10):
    params: dict = {"limit": 2}
    if before is not None:
      params.update({"before": before, "before_id": before_id})
    resp = client.get("/api/sessions/archived", params=params)
    assert resp.status_code == 200
    page = resp.json()
    rows = page["sessions"]
    real = [r for r in rows if not r.get("context_only")]
    # Page size stays an archived-row count: a page never fills with context.
    if page["has_more"]:
      assert len(real) == 2
    archived.extend(r["id"] for r in real)
    page_context = [r["id"] for r in rows if r.get("context_only")]
    # One page never repeats a context row; across pages one repeats (the
    # server cannot see the client's merged tree), and the client dedups by id.
    assert len(page_context) == len(set(page_context))
    for row in rows:
      if row.get("context_only"):
        assert row["status"] != SessionStatus.ARCHIVED
    context.extend(page_context)
    if not page["has_more"]:
      break
    before, before_id = page["next_before"], page["next_before_id"]

  # Every archived row exactly once — the walk's cursor never repeats a row.
  assert len(archived) == len(set(archived))
  assert set(archived) == {delivered.id, hidden.id, legacy_child.id, *(f.id for f in fillers)}
  # The active ancestors ride as context_only rows: the root for every branch,
  # climbed past the derived-archived intermediate.
  assert set(context) == {root.id}
  # The aggregates describe the archived rows alone.
  assert page["groups"] == [{"group": None, "total": 6}]


@pytest.mark.asyncio
async def test_archived_context_walk_keeps_the_cron_subtree_out(tmp_path: Path, temp_home: Path) -> None:
  cfg, session_mgr, tree = build_env(tmp_path)
  thread_mgr = ThreadManager(cfg)
  cron = await make_legacy_cron_session(session_mgr, "synthetic-cron")
  active_root = await create_task(tree, parent=None, request_id="root-1", profile="manager", name="Synthetic root")
  firing = await create_task(
      tree, parent=cron.id, request_id="firing-1", profile="worker", name="synthetic-cron · firing-1")
  await session_mgr.archive_session(firing.id)
  # An archived row under the ACTIVE root: the walk climbs to the root, while
  # the firing's chain reaches the cron session, so it never becomes a page row.
  delivered = await create_task(
      tree, parent=active_root.id, request_id="leaf-2", profile="worker", name="Synthetic delivered")
  await tree.runs.register_run(RunRecord(id="run-2", session_id=delivered.id, kind="work"))
  await tree.dispatch.finish_run(delivered.id, "run-2", outcome="success")

  client = _api_client(cfg, session_mgr, tree, thread_mgr)
  seen: dict[str, dict] = {}
  before = None
  before_id = None
  for _ in range(10):
    params: dict = {"limit": 2}
    if before is not None:
      params.update({"before": before, "before_id": before_id})
    resp = client.get("/api/sessions/archived", params=params)
    assert resp.status_code == 200
    page = resp.json()
    for row in page["sessions"]:
      seen[row["id"]] = row
    if not page["has_more"]:
      break
    before, before_id = page["next_before"], page["next_before_id"]

  # The cron-subtree exclusion holds: neither the firing nor its cron session
  # rides any page, as an archived row or as context.
  assert firing.id not in seen and cron.id not in seen
  assert delivered.id in seen and "context_only" not in seen[delivered.id]
  context_rows = [row for row in seen.values() if row.get("context_only")]
  assert [row["id"] for row in context_rows] == [active_root.id]
  # The join answers on context rows too: the root is unbound here.
  assert context_rows[0]["schedule_task"] is None
