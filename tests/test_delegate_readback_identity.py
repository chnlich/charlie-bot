"""Delegate readback identity: sent-but-lost binds to the request identity.

The old readback scanned for the first same-description/type sibling; two
intentional identical-spec siblings could report each other's result. The
fixed readback computes the child's stable id from the operation's request
identity (explicit --request-id or the derived default the server computes)
and verifies the parent/profile/spec/type — no fallback to an unrelated
sibling or legacy thread on an ambiguous v2 readback.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from src.cli.common import derive_delegate_request_id, find_local_task_child
from src.core.models import RunRecord, TaskSpec, TaskType
from tests.test_task_execution import build_env


@pytest.mark.asyncio
async def test_two_same_spec_siblings_readback_binds_to_request_identity(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _session_mgr, tree = build_env(tmp_path, monkeypatch)
  manager = await tree.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=TaskSpec(goal="pm"),
      name="PM",
      backend=None,
      caller="operator")
  description = "Fix the flaky test the same way twice"
  first = await tree.create_task(
      request_id="delegate-sibling-1",
      task_parent_id=manager.id,
      profile="worker",
      task=TaskSpec(goal=description, task_type=TaskType.QUICK_EDIT),
      name="sibling-1",
      backend=None,
      caller="operator")
  second = await tree.create_task(
      request_id="delegate-sibling-2",
      task_parent_id=manager.id,
      profile="worker",
      task=TaskSpec(goal=description, task_type=TaskType.QUICK_EDIT),
      name="sibling-2",
      backend=None,
      caller="operator")
  assert first.id != second.id

  # The explicit request identity resolves to ITS OWN child — never the
  # first same-description sibling.
  readback = find_local_task_child(
      manager.id, description=description, task_type="quick-edit", request_id="delegate-sibling-2")
  assert readback is not None
  assert readback["session_id"] == second.id
  assert readback["parent_session_id"] == manager.id
  readback_first = find_local_task_child(
      manager.id, description=description, task_type="quick-edit", request_id="delegate-sibling-1")
  assert readback_first is not None and readback_first["session_id"] == first.id

  # The derived default binds ONE (session, type, spec body) to one operation.
  derived = derive_delegate_request_id(manager.id, "quick-edit", description)
  derived_child = await tree.create_task(
      request_id=derived,
      task_parent_id=manager.id,
      profile="worker",
      task=TaskSpec(goal=description, task_type=TaskType.QUICK_EDIT),
      name="derived",
      backend=None,
      caller="operator")
  readback_derived = find_local_task_child(manager.id, description=description, task_type="quick-edit")
  assert readback_derived is not None and readback_derived["session_id"] == derived_child.id


@pytest.mark.asyncio
async def test_readback_returns_none_without_the_bound_child(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """No bound child (or a mismatched one) is outcome-unknown, never a fallback."""
  _cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
  manager = await tree.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=TaskSpec(goal="pm"),
      name="PM",
      backend=None,
      caller="operator")
  description = "A delegation that never landed"
  # A sibling with a DIFFERENT description exists: the readback must not
  # return it for a missing operation.
  await tree.create_task(
      request_id="delegate-other",
      task_parent_id=manager.id,
      profile="worker",
      task=TaskSpec(goal="a different spec entirely", task_type=TaskType.QUICK_EDIT),
      name="other",
      backend=None,
      caller="operator")
  assert find_local_task_child(
      manager.id, description=description, task_type="quick-edit", request_id="delegate-missing") is None
  # A v1 session (no task-tree child possible) reads back None as well.
  from src.core.models import CreateSessionRequest
  legacy = await session_mgr.create_session(CreateSessionRequest(name="Old"))
  assert find_local_task_child(
      legacy.id, description="whatever", task_type="quick-edit", request_id="delegate-x") is None


@pytest.mark.asyncio
async def test_readback_run_id_is_the_operation_work_run(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The returned Run is the delegation's own work Run — not whichever run
    directory sorts first (a review Run of the same child is a different op)."""
  from src.core.control_events import stable_run_id
  _cfg, _session_mgr, tree = build_env(tmp_path, monkeypatch)
  manager = await tree.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=TaskSpec(goal="pm"),
      name="PM",
      backend=None,
      caller="operator")
  description = "The spec"
  child = await tree.create_task(
      request_id="delegate-runcheck",
      task_parent_id=manager.id,
      profile="worker",
      task=TaskSpec(goal=description, task_type=TaskType.QUICK_EDIT),
      name="c",
      backend=None,
      caller="operator")
  work_run_id = stable_run_id(child.id, "delegate-runcheck:work")
  review_run_id = stable_run_id(child.id, "delegate-runcheck:review")
  await tree.runs.register_run(RunRecord(id=work_run_id, session_id=child.id, kind="work", repo_path="/repo"))
  await tree.runs.register_run(
      RunRecord(id=review_run_id, session_id=child.id, kind="review", review_of_run_id=work_run_id))
  readback = find_local_task_child(
      manager.id, description=description, task_type="quick-edit", request_id="delegate-runcheck")
  assert readback is not None
  assert readback["run_id"] == work_run_id
  assert readback["thread_id"] == work_run_id
