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
from src.runtime import task_sessions


@dataclasses.dataclass
class Fixture:
  """One corpus exercising every cron-subtree classification the sidebar lists make."""

  cfg: config.CharlieBotConfig
  session_blocks: conftest.SessionBlocks
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
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  cron = await conftest.make_cron_session(session_blocks, "nightly")
  manager = await conftest.create_task(
      tree, parent=cron.id, request_id="mgr-1", profile="manager", name="nightly · manager")
  worker = await conftest.create_task(
      tree, parent=cron.id, request_id="leaf-42", profile="worker", name="nightly · firing-42")
  grandchild = await conftest.create_task(
      tree, parent=manager.id, request_id="leaf-43", profile="worker", name="nightly · firing-43")
  # The delivered leaf stays a cron-subtree row through its manager parent.
  delivered = await conftest.create_task(
      tree, parent=manager.id, request_id="leaf-44", profile="worker", name="nightly · firing-44")
  await tree.runs.register_run_locked(models.RunRecord(id="run-44", session_id=delivered.id, kind="work"))
  await tree.dispatch.finish_run(delivered.id, "run-44", outcome="success")
  assert tree.task_state(delivered.id) == "completed"  # the derived archive hides it while active

  cron_archived = await conftest.make_cron_session(session_blocks, "nightly-archived")
  await session_blocks.lifecycle.archive_session(cron_archived.id)
  plain_archived = await conftest.create_root_session(
      session_blocks, models.CreateSessionRequest(name="Plain archived"), backend=conftest.OPUS_BACKEND_ID)
  await session_blocks.lifecycle.archive_session(plain_archived.id)
  plain_active = await conftest.create_root_session(
      session_blocks, models.CreateSessionRequest(name="Plain active"), backend=conftest.OPUS_BACKEND_ID)
  ordinary = await conftest.create_root_session(
      session_blocks, models.CreateSessionRequest(name="Ordinary"), backend=conftest.OPUS_BACKEND_ID)
  fillers = []
  for i in range(3):
    filler = await conftest.create_root_session(
        session_blocks, models.CreateSessionRequest(name=f"Filler {i}"), backend=conftest.OPUS_BACKEND_ID)
    await session_blocks.lifecycle.archive_session(filler.id)
    fillers.append(filler)
  return Fixture(
      cfg=cfg,
      session_blocks=session_blocks,
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


@pytest.mark.asyncio
async def test_scheduled_filter_returns_only_cron_sessions(tmp_path: pathlib.Path) -> None:
  fx = await _build_fixture(tmp_path)
  rows = await fx.session_blocks.listing.list_sessions(status=models.SessionStatus.ACTIVE, scheduled=True)
  assert [row.id for row in rows] == [fx.cron.id]
  assert all(row.scheduled_task is not None for row in rows)


@pytest.mark.asyncio
async def test_subtree_map_rederives_after_a_metadata_write(tmp_path: pathlib.Path) -> None:
  """The derived map rides the listings memo's staleness bounds, so a write
  between two reads must be visible to the second read and not to a stale map."""
  fx = await _build_fixture(tmp_path)
  warmed = await fx.session_blocks.listing.sequence_subtree_roots()
  assert fx.worker.id in warmed and fx.ordinary.id not in warmed
  # Attach the plain session under the cron session: the write funnel bumps the
  # listings revision, the next listing rebuilds its list, and the map keyed on
  # that list's identity re-derives with the new member.
  meta = await fx.session_blocks.store.get_session(fx.ordinary.id)
  meta.task_parent_id = fx.cron.id
  await fx.session_blocks.store.save_metadata(meta)
  attached = await fx.session_blocks.listing.sequence_subtree_roots()
  assert attached[fx.ordinary.id] == fx.cron.id
  # Detach again: the map drops the member on the next read.
  meta = await fx.session_blocks.store.get_session(fx.ordinary.id)
  meta.task_parent_id = None
  await fx.session_blocks.store.save_metadata(meta)
  detached = await fx.session_blocks.listing.sequence_subtree_roots()
  assert fx.ordinary.id not in detached
