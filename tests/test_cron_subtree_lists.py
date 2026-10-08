"""The cron subtree (every session whose parent chain reaches a scheduled_task session)
rides no sidebar listing.

All (route + homepage render) and Archived return no cron-subtree rows, and
the active cron session itself (the ``scheduled`` listing filter keeps
its root out of All) — while the archived cron sessions keep their rows in
Archived and the keyset pagination and group aggregates describe the rows
actually returned. Search and Starred keep their behavior, and
``list_sessions(scheduled=True)`` still returns exactly the cron sessions (the
scheduler's lookup depends on it).
"""

from __future__ import annotations

import dataclasses
import pathlib

import conftest
import pytest

from src.infra import config, models
from src.runtime import sessions, task_sessions


@dataclasses.dataclass
class Fixture:
  """One corpus exercising every cron-subtree classification the sidebar lists make."""

  cfg: config.CharlieBotConfig
  session_mgr: sessions.SessionManager
  tree: task_sessions.TaskTreeManager
  cron: models.SessionMetadata  # active cron session (scheduled_task set)
  manager: models.SessionMetadata  # active manager child of the cron session
  worker: models.SessionMetadata  # active direct worker child of the cron session
  grandchild: models.SessionMetadata  # active worker under the manager child
  delivered: models.SessionMetadata  # worker under the manager child, archived by derivation
  cron_archived: models.SessionMetadata  # archived cron session
  plain_archived: models.SessionMetadata  # archived non-cron session
  plain_active: models.SessionMetadata  # active non-cron session
  ordinary: models.SessionMetadata  # active plain session
  fillers: list[models.SessionMetadata]  # archived plain sessions padding the keyset walk


async def _build_fixture(tmp_path: pathlib.Path) -> Fixture:
  cfg, session_mgr, tree = conftest.build_env(tmp_path)
  cron = await conftest.make_cron_session(session_mgr, "nightly")
  manager = await conftest.create_task(
      tree, parent=cron.id, request_id="mgr-1", profile="manager", name="nightly · manager")
  worker = await conftest.create_task(
      tree, parent=cron.id, request_id="leaf-42", profile="worker", name="nightly · firing-42")
  grandchild = await conftest.create_task(
      tree, parent=manager.id, request_id="leaf-43", profile="worker", name="nightly · firing-43")
  # The delivered leaf stays a cron-subtree row through its manager parent.
  delivered = await conftest.create_task(
      tree, parent=manager.id, request_id="leaf-44", profile="worker", name="nightly · firing-44")
  await tree.runs.register_run(models.RunRecord(id="run-44", session_id=delivered.id, kind="work"))
  await tree.dispatch.finish_run(delivered.id, "run-44", outcome="success")
  assert tree.task_state(delivered.id) == "completed"  # the derived archive hides it while active

  cron_archived = await conftest.make_cron_session(session_mgr, "nightly-archived")
  await session_mgr.archive_session(cron_archived.id)
  plain_archived = await conftest.create_root_session(
      session_mgr, models.CreateSessionRequest(name="Plain archived"), backend=conftest.OPUS_BACKEND_ID)
  await session_mgr.archive_session(plain_archived.id)
  plain_active = await conftest.create_root_session(
      session_mgr, models.CreateSessionRequest(name="Plain active"), backend=conftest.OPUS_BACKEND_ID)
  ordinary = await conftest.create_root_session(
      session_mgr, models.CreateSessionRequest(name="Ordinary"), backend=conftest.OPUS_BACKEND_ID)
  fillers = []
  for i in range(3):
    filler = await conftest.create_root_session(
        session_mgr, models.CreateSessionRequest(name=f"Filler {i}"), backend=conftest.OPUS_BACKEND_ID)
    await session_mgr.archive_session(filler.id)
    fillers.append(filler)
  return Fixture(
      cfg=cfg,
      session_mgr=session_mgr,
      tree=tree,
      cron=cron,
      manager=manager,
      worker=worker,
      grandchild=grandchild,
      delivered=delivered,
      cron_archived=cron_archived,
      plain_archived=plain_archived,
      plain_active=plain_active,
      ordinary=ordinary,
      fillers=fillers)


# Every row the cron-subtree rule excludes from All and Archived, by fixture name.
_CRON_SUBTREE_ROWS = ("cron", "manager", "worker", "grandchild", "delivered", "cron_archived")


def _subtree_ids(fx: Fixture) -> set[str]:
  return {getattr(fx, name).id for name in _CRON_SUBTREE_ROWS}


@pytest.mark.asyncio
async def test_all_list_and_homepage_exclude_the_cron_subtree(tmp_path: pathlib.Path) -> None:
  fx = await _build_fixture(tmp_path)
  client = conftest.make_sessions_listing_client(fx.cfg, fx.session_mgr, fx.tree)
  resp = client.get("/api/sessions/")
  assert resp.status_code == 200
  all_ids = {row["id"] for row in resp.json()}
  # The active rows that never belong to a cron subtree stay.
  assert {fx.ordinary.id, fx.plain_active.id} <= all_ids
  # No cron-subtree row: neither the cron sessions nor any of their descendants.
  assert not (all_ids & _subtree_ids(fx))

  # The homepage's first-paint list carries the same membership, and the
  # auto-redirect never lands on a cron-subtree row.
  page_client = conftest.make_sessions_listing_page_client(fx.cfg, fx.session_mgr, fx.tree)
  redirect = page_client.get("/", follow_redirects=False)
  assert redirect.status_code in (301, 302, 307)
  assert redirect.headers["location"].split("session=")[1] not in _subtree_ids(fx)
  row_ids = {row["id"] for row in conftest.page_initial_sessions(page_client, fx.ordinary.id)}
  assert {fx.ordinary.id, fx.plain_active.id} <= row_ids
  assert not (row_ids & _subtree_ids(fx))


@pytest.mark.asyncio
async def test_archived_list_excludes_cron_subtree_rows_and_paginates(tmp_path: pathlib.Path) -> None:
  fx = await _build_fixture(tmp_path)
  client = conftest.make_sessions_listing_client(fx.cfg, fx.session_mgr, fx.tree)
  resp = client.get("/api/sessions/archived")
  assert resp.status_code == 200
  page = resp.json()
  ids = {row["id"] for row in page["sessions"]}
  # The archived cron session itself and the ordinary archived session stay.
  assert fx.cron_archived.id in ids
  assert fx.plain_archived.id in ids
  # No cron-subtree row, including the delivered child archived by derivation.
  assert fx.delivered.id not in ids
  # The aggregates describe the rows actually returned: the five archived
  # sessions that survive the exclusion (the delivered child stays out).
  assert page["groups"] == [{"group": None, "total": 5}]

  # A small-limit walk returns every non-excluded archived row exactly once,
  # with every page but the last full.
  expected = {fx.cron_archived.id, fx.plain_archived.id, *(filler.id for filler in fx.fillers)}
  walk: list[str] = []
  for page in conftest.walk_archived_pages(client):
    if page["has_more"]:
      assert len(page["sessions"]) == 2  # a page stays full when more rows exist
    walk.extend(row["id"] for row in page["sessions"])
  assert len(walk) == len(set(walk))  # the cursor never repeats a row
  assert set(walk) == expected


@pytest.mark.asyncio
async def test_search_and_starred_keep_their_rows(tmp_path: pathlib.Path) -> None:
  fx = await _build_fixture(tmp_path)
  client = conftest.make_sessions_listing_client(fx.cfg, fx.session_mgr, fx.tree)
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
async def test_scheduled_filter_returns_only_cron_sessions(tmp_path: pathlib.Path) -> None:
  fx = await _build_fixture(tmp_path)
  rows = await fx.session_mgr.listing.list_sessions(status=models.SessionStatus.ACTIVE, scheduled=True)
  assert [row.id for row in rows] == [fx.cron.id]
  assert all(row.scheduled_task is not None for row in rows)


@pytest.mark.asyncio
async def test_subtree_map_rederives_after_a_metadata_write(tmp_path: pathlib.Path) -> None:
  """The derived map rides the listings memo's staleness bounds, so a write
  between two reads must be visible to the second read and not to a stale map."""
  fx = await _build_fixture(tmp_path)
  warmed = await fx.session_mgr.listing.sequence_subtree_roots()
  assert fx.worker.id in warmed and fx.ordinary.id not in warmed
  # Attach the plain session under the cron session: the write funnel bumps the
  # listings revision, the next listing rebuilds its list, and the map keyed on
  # that list's identity re-derives with the new member.
  meta = await fx.session_mgr.store.get_session(fx.ordinary.id)
  meta.task_parent_id = fx.cron.id
  await fx.session_mgr.store.save_metadata(meta)
  attached = await fx.session_mgr.listing.sequence_subtree_roots()
  assert attached[fx.ordinary.id] == fx.cron.id
  # Detach again: the map drops the member on the next read.
  meta = await fx.session_mgr.store.get_session(fx.ordinary.id)
  meta.task_parent_id = None
  await fx.session_mgr.store.save_metadata(meta)
  detached = await fx.session_mgr.listing.sequence_subtree_roots()
  assert fx.ordinary.id not in detached
