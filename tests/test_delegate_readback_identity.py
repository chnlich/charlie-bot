"""Delegate readback identity: sent-but-lost binds to the request identity.

The old readback scanned for the first same-description/type sibling; two
intentional identical-spec siblings could report each other's result. The
fixed readback computes the child's stable id from the operation's request
identity (explicit --request-id or the derived default the server computes)
and verifies the parent/profile/spec/type — no fallback to an unrelated
sibling or legacy thread on an ambiguous v2 readback.
"""

from __future__ import annotations

import pathlib

import conftest
import pytest

from src.infra import models
from src.runtime import control_events
from src.runtime.cli import common
from tests import test_task_execution


@pytest.mark.asyncio
async def test_two_same_spec_siblings_readback_binds_to_request_identity(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _session_mgr, tree = test_task_execution.build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(
      tree, parent=None, request_id="root", profile="manager", task=models.TaskSpec(goal="pm"), name="PM")
  description = "Fix the flaky test the same way twice"
  first = await tree.create_task(
      request_id="delegate-sibling-1",
      task_parent_id=manager.id,
      profile="worker",
      task=models.TaskSpec(goal=description, task_type=models.TaskType.QUICK_EDIT),
      name="sibling-1",
      backend=None,
      caller="operator")
  second = await tree.create_task(
      request_id="delegate-sibling-2",
      task_parent_id=manager.id,
      profile="worker",
      task=models.TaskSpec(goal=description, task_type=models.TaskType.QUICK_EDIT),
      name="sibling-2",
      backend=None,
      caller="operator")
  assert first.id != second.id

  # The explicit request identity resolves to ITS OWN child — never the
  # first same-description sibling.
  readback = common.find_local_task_child(
      manager.id, description=description, task_type="quick-edit", request_id="delegate-sibling-2")
  assert readback is not None
  assert readback["session_id"] == second.id
  assert readback["parent_session_id"] == manager.id
  readback_first = common.find_local_task_child(
      manager.id, description=description, task_type="quick-edit", request_id="delegate-sibling-1")
  assert readback_first is not None and readback_first["session_id"] == first.id

  # The derived default binds ONE (session, type, spec body) to one operation.
  derived = control_events.derived_delegate_request_id(manager.id, "quick-edit", description)
  derived_child = await tree.create_task(
      request_id=derived,
      task_parent_id=manager.id,
      profile="worker",
      task=models.TaskSpec(goal=description, task_type=models.TaskType.QUICK_EDIT),
      name="derived",
      backend=None,
      caller="operator")
  readback_derived = common.find_local_task_child(manager.id, description=description, task_type="quick-edit")
  assert readback_derived is not None and readback_derived["session_id"] == derived_child.id


@pytest.mark.asyncio
async def test_readback_returns_none_without_the_bound_child(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """No bound child (or a mismatched one) is outcome-unknown, never a fallback."""
  _cfg, session_mgr, tree = test_task_execution.build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(
      tree, parent=None, request_id="root", profile="manager", task=models.TaskSpec(goal="pm"), name="PM")
  description = "A delegation that never landed"
  # A sibling with a DIFFERENT description exists: the readback must not
  # return it for a missing operation.
  await tree.create_task(
      request_id="delegate-other",
      task_parent_id=manager.id,
      profile="worker",
      task=models.TaskSpec(goal="a different spec entirely", task_type=models.TaskType.QUICK_EDIT),
      name="other",
      backend=None,
      caller="operator")
  assert common.find_local_task_child(
      manager.id, description=description, task_type="quick-edit", request_id="delegate-missing") is None
  # A manager root with no matching child also reads back None.
  root = await conftest.create_root_session(session_mgr, models.CreateSessionRequest(name="Root"))
  assert common.find_local_task_child(
      root.id, description="whatever", task_type="quick-edit", request_id="delegate-x") is None


@pytest.mark.asyncio
async def test_readback_run_id_is_the_operation_work_run(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The returned Run is the delegation's own work Run — not whichever run
    directory sorts first (a review Run of the same child is a different op)."""
  _cfg, _session_mgr, tree = test_task_execution.build_env(tmp_path, monkeypatch)
  manager = await conftest.create_task(
      tree, parent=None, request_id="root", profile="manager", task=models.TaskSpec(goal="pm"), name="PM")
  description = "The spec"
  child = await tree.create_task(
      request_id="delegate-runcheck",
      task_parent_id=manager.id,
      profile="worker",
      task=models.TaskSpec(goal=description, task_type=models.TaskType.QUICK_EDIT),
      name="c",
      backend=None,
      caller="operator")
  work_run_id = control_events.stable_run_id(child.id, "delegate-runcheck:work")
  review_run_id = control_events.stable_run_id(child.id, "delegate-runcheck:review")
  await tree.runs.register_run(models.RunRecord(id=work_run_id, session_id=child.id, kind="work", repo_path="/repo"))
  await tree.runs.register_run(
      models.RunRecord(id=review_run_id, session_id=child.id, kind="review", review_of_run_id=work_run_id))
  readback = common.find_local_task_child(
      manager.id, description=description, task_type="quick-edit", request_id="delegate-runcheck")
  assert readback is not None
  assert readback["run_id"] == work_run_id
  assert readback["thread_id"] == work_run_id
