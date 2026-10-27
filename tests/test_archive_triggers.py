"""Scheduled triggers against an archived task node: creation refuses and the
fire-time backstop cancels with the named reason. Real managers drive the tree."""

from __future__ import annotations

import datetime
import pathlib

import conftest
import pytest

from src.infra import models
from src.runtime import triggers


def _trigger(session_id: str, trigger_id: str = "trig-archived-1") -> models.PendingTrigger:
  return models.PendingTrigger(
      id=trigger_id, session_id=session_id, fire_at=datetime.datetime.now(datetime.UTC), message="wake")


@pytest.mark.asyncio
async def test_create_trigger_on_an_archived_task_node_is_refused(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  conftest.bind_deps_blocks(monkeypatch, tree, session_blocks)
  node = await conftest.create_task(tree, parent=None, request_id="node")
  await tree.completion.archive_subtree(
      node.id,
      request_id="archive-test",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR)

  trigger_mgr = triggers.TriggerManager(cfg, tree)
  with pytest.raises(triggers.ArchivedSessionError, match="target task is archived"):
    await trigger_mgr.create_trigger(node.id, 60, "check the run")

  # A pending trigger on the still-open node is untouched; the refusal wrote
  # nothing for the archived target.
  assert list(await trigger_mgr.list_triggers(node.id)) == []


@pytest.mark.asyncio
async def test_fire_time_backstop_cancels_with_the_archived_reason(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  conftest.bind_deps_blocks(monkeypatch, tree, session_blocks)
  node = await conftest.create_task(tree, parent=None, request_id="node")
  trigger_mgr = triggers.TriggerManager(cfg, tree)
  trigger = _trigger(node.id)
  await trigger_mgr._save_trigger(trigger)
  # The archive lands mid-wait: the fire-time re-check must cancel, not fire.
  await tree.completion.archive_subtree(
      node.id,
      request_id="archive-test",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR)

  await trigger_mgr._wait_and_fire(trigger)

  fresh = await trigger_mgr._load_trigger(node.id, trigger.id)
  assert fresh is not None
  assert fresh.status.value == "cancelled"
  assert fresh.fire_reason == "target task is archived"
  # No scheduled input was delivered to the archived node.
  assert not [e for e in tree.events.load_events(node.id) if e["type"] == "scheduled_trigger"]


@pytest.mark.asyncio
async def test_watchdog_reason_carries_into_the_cancel(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, session_blocks, tree = conftest.build_env(tmp_path)
  conftest.bind_deps_blocks(monkeypatch, tree, session_blocks)
  node = await conftest.create_task(tree, parent=None, request_id="node")
  await tree.completion.archive_subtree(
      node.id,
      request_id="archive-test",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR)

  trigger_mgr = triggers.TriggerManager(cfg, tree)
  # The dormancy judgment is the one predicate both racers read.
  assert await trigger_mgr._dormancy_reason(node.id) == "target task is archived"
  # An open manager root remains eligible for trigger registration.
  open_root = await conftest.create_root_session(
      session_blocks, models.CreateSessionRequest(name="Open root"), backend=None)
  assert open_root.profile == "manager"
  assert await trigger_mgr._dormancy_reason(open_root.id) is None
