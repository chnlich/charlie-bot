"""The archive unification: archived is the single end state of a task node.

One rule per shape (a task node is archived exactly when its task state is not
open; a session without a profile keeps the stored-status archive), the
pending-input boundary living in the fold alone (a close clears the candidates
and closes the boundary; a reopen opens a fresh one), the cascade archive (one
archived close fact per open node, no parent report, archived_with naming the
node the user archived, unfinished runs refusing with nothing written), the
chain restore (topmost ancestor down to the target, siblings and descendants
untouched, no round started), and the input table (the user's message restores;
machine input is refused with the one archived sentence; a child report is
history that wakes nobody). Every test drives the real TaskTreeManager and
SessionManager over a temp home.
"""

from __future__ import annotations

from datetime import UTC, datetime
from pathlib import Path
from unittest import mock

import pytest
from conftest import OPERATOR, build_env, create_task

from src.core import event_types as ET
from src.core.models import CreateSessionRequest, RunRecord, SessionStatus
from src.core.run_token import CallerIdentity, RunTokenClaims
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskArchivedError, TaskConflictError, TaskTreeManager

OPUS_BACKEND_ID = "claude-opus-test"
OPUS_BACKEND_OPTION = {
    "id": OPUS_BACKEND_ID,
    "label": "CC · Opus test",
    "type": "cc-claude",
    "model": "claude-opus-test-1",
}


class ScriptedExecutor:
  """The executor seam's double: same signature and return shape as the real
  adapter's launch (``(session_id, pending, launch_run_id=None) -> run id``).

  It registers one run, claims the batch, and finishes it with the scripted
  outcome, recording every (session, batch) it was handed.
  """

  def __init__(self, tree: TaskTreeManager, *, outcome: str = "success", finish: bool = True) -> None:
    self.tree = tree
    self.outcome = outcome
    self.finish = finish
    self.batches: list[tuple[str, list[str]]] = []

  async def __call__(self, session_id: str, pending: list[dict], launch_run_id: str | None = None) -> str:
    run_id = launch_run_id or f"run-{len(self.batches) + 1}"
    batch = [str(e["id"]) for e in pending]
    self.batches.append((session_id, batch))
    await self.tree.runs.register_run(RunRecord(id=run_id, session_id=session_id, kind="work"))
    async with self.tree.control_lock:
      await self.tree.dispatch.claim_input_batch_locked(session_id, run_id)
    if self.finish:
      await self.tree.dispatch.finish_run(session_id, run_id, outcome=self.outcome)
    return run_id


def input_events(tree: TaskTreeManager, session_id: str) -> list[dict]:
  return [
      e for e in tree.events.load_events(session_id) if e["type"] in (ET.USER, ET.AGENT_MESSAGE, ET.SCHEDULED_TRIGGER)
  ]


def close_facts(tree: TaskTreeManager, session_id: str) -> list[dict]:
  return [e for e in tree.events.load_events(session_id) if e["type"] == ET.TASK_CLOSED]


async def build_tree(tree: TaskTreeManager, *, with_old_input: bool = False):
  """root -> mid -> leaf, plus a completed sibling under root. The leaf carries
  one old pending input when asked."""
  root = await create_task(tree, parent=None, request_id="root", name="Root")
  mid = await create_task(tree, parent=root.id, request_id="mid", name="Mid")
  leaf = await create_task(tree, parent=mid.id, request_id="leaf", profile="worker", name="Leaf")
  completed = await create_task(tree, parent=root.id, request_id="done", profile="worker", name="Done")
  await tree.runs.register_run(RunRecord(id="run-done", session_id=completed.id, kind="work"))
  await tree.dispatch.finish_run(completed.id, "run-done", outcome="success")
  assert tree.task_state(completed.id) == "completed"
  if with_old_input:
    await tree.dispatch.admit_input(leaf.id, event_type=ET.USER, content="old instruction", actor="user")
    assert len(tree.dispatch.pending_inputs(leaf.id)) == 1
  return root, mid, leaf, completed


# ---------------------------------------------------------------------------
# The fold's input boundary
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_close_clears_candidates_and_reopen_opens_a_fresh_boundary(tmp_path: Path) -> None:
  _cfg, _session_mgr, tree = build_env(tmp_path)
  root, mid, leaf, _completed = await build_tree(tree, with_old_input=True)

  # The archive is the boundary that closes: the old pending input is history.
  assert await tree.archive_subtree(root.id, caller=OPERATOR) == [root.id, mid.id, leaf.id]
  for node in (root.id, mid.id, leaf.id):
    assert tree.dispatch.pending_inputs(node) == []
    assert tree.task_state(node) == "archived"

  # An arrival while closed is history only — the child-report write path
  # records it without making it a candidate, and it never wakes the node.
  report, created = await tree.dispatch.deliver_child_report(
      leaf.id,
      source_event={"id": "report-while-archived"},
      outcome="completed",
      summary="late sibling result",
      result_refs=[],
      recipient=leaf.id)
  assert report is not None and created
  assert tree.dispatch.pending_inputs(leaf.id) == []
  assert (leaf.id, "report-while-archived") in tree.facts_of(leaf.id).delivered_reports

  # The reopen opens a fresh boundary: post-reopen input is a candidate, and
  # everything before it (the old input, the closed-period report) stays history.
  restored = await tree.completion.restore_chain(leaf.id, request_id="restore-1", reason="sidebar unarchive")
  assert restored == [root.id, mid.id, leaf.id]
  await tree.dispatch.admit_input(
      leaf.id, event_type=ET.USER, content="fresh instruction", actor="user", input_id="fresh-1")
  assert [str(e["id"]) for e in tree.dispatch.pending_inputs(leaf.id)] == ["fresh-1"]


# ---------------------------------------------------------------------------
# The cascade archive
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_cascade_archive_writes_one_fact_per_open_node_with_archived_with(tmp_path: Path) -> None:
  _cfg, _session_mgr, tree = build_env(tmp_path)
  root, mid, leaf, completed = await build_tree(tree)

  archived = await tree.archive_subtree(root.id, caller=OPERATOR)

  assert archived == [root.id, mid.id, leaf.id]
  for node in (root.id, mid.id, leaf.id):
    closes = close_facts(tree, node)
    assert len(closes) == 1
    fact = closes[0]
    assert fact["outcome"] == "archived"
    assert fact["actor"] == "user"
    assert fact.get("report_to") is None  # an archive reports to nobody
  # The descendant facts name the node the user archived; the target's own fact
  # carries no archived_with.
  assert close_facts(tree, root.id)[0].get("archived_with") is None
  assert close_facts(tree, mid.id)[0]["archived_with"] == root.id
  assert close_facts(tree, leaf.id)[0]["archived_with"] == root.id
  # The completed sibling is an end state already: unchanged (no archived
  # close fact of ours lands on it).
  assert tree.task_state(completed.id) == "completed"
  assert [c for c in close_facts(tree, completed.id) if c["outcome"] == "archived"] == []
  # The archive delivered no parent report: no child report anywhere references
  # an archive close fact (the completed sibling's own completion report
  # predates the archive and is not the archive's).
  archive_close_ids = {close_facts(tree, n)[0]["id"] for n in (root.id, mid.id, leaf.id)}
  for node in (root.id, mid.id, leaf.id, completed.id):
    reports = [e for e in tree.events.load_events(node) if e["type"] == ET.CHILD_REPORT]
    assert not [r for r in reports if r.get("child_event_id") in archive_close_ids]
  # The whole subtree reads as archived through the one derivation.
  index = await tree._get_index()
  for node in (root.id, mid.id, leaf.id):
    assert tree.archived_of(index, index.metas[node]) is True
  assert tree.archived_of(index, index.metas[completed.id]) is True
  # An already-archived target answers with an empty list and writes nothing.
  assert await tree.archive_subtree(root.id, caller=OPERATOR) == []
  assert len(close_facts(tree, root.id)) == 1


@pytest.mark.asyncio
async def test_cascade_archive_refuses_an_unfinished_run_and_writes_nothing(tmp_path: Path) -> None:
  _cfg, _session_mgr, tree = build_env(tmp_path)
  root, mid, leaf, _completed = await build_tree(tree)
  import subprocess

  from src.core.runs import read_pid_stat
  proc = subprocess.Popen(["/bin/sleep", "30"])
  try:
    pid_start, _state = read_pid_stat(proc.pid)
    await tree.runs.register_run(
        RunRecord(
            id="run-leaf",
            session_id=leaf.id,
            kind="work",
            pid=proc.pid,
            pid_start=pid_start,
            started_at=datetime.now(UTC)))

    with pytest.raises(TaskConflictError) as excinfo:
      await tree.archive_subtree(root.id, caller=OPERATOR)
    blockers = excinfo.value.blockers
    assert any(leaf.id in b and "run run-leaf" in b for b in blockers)

    # Nothing was written: no close fact anywhere in the subtree.
    for node in (root.id, mid.id, leaf.id):
      assert close_facts(tree, node) == []
      assert tree.task_state(node) == "open"
  finally:
    proc.kill()
    proc.wait()


# ---------------------------------------------------------------------------
# The chain restore
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_restore_brings_the_archived_ancestor_chain_and_leaves_the_rest(tmp_path: Path) -> None:
  _cfg, _session_mgr, tree = build_env(tmp_path)
  root, mid, leaf, completed = await build_tree(tree)
  other_leaf = await create_task(tree, parent=mid.id, request_id="sib", profile="worker", name="Sib")
  await tree.archive_subtree(root.id, caller=OPERATOR)

  executor = ScriptedExecutor(tree)
  tree.dispatch.executor = executor
  restored = await tree.completion.restore_chain(leaf.id, request_id="restore-1", reason="sidebar unarchive")

  # The chain only: the target and its archived ancestors, topmost first.
  assert restored == [root.id, mid.id, leaf.id]
  for node in (root.id, mid.id, leaf.id):
    assert tree.task_state(node) == "open"
  # Siblings and descendants stay archived; the completed node stays completed.
  assert tree.task_state(other_leaf.id) == "archived"
  assert tree.task_state(completed.id) == "completed"
  # Each fact names the restore's source and the close it ends.
  for node in (root.id, mid.id, leaf.id):
    reopens = [e for e in tree.events.load_events(node) if e["type"] == ET.TASK_REOPENED]
    assert len(reopens) == 1
    assert reopens[0]["reason"] == "sidebar unarchive"
    assert reopens[0]["closed_event_id"] == close_facts(tree, node)[0]["id"]
  # No round started anywhere.
  assert executor.batches == []
  # A replayed request id restores nothing twice.
  again = await tree.completion.restore_chain(leaf.id, request_id="restore-1", reason="sidebar unarchive")
  assert again == []
  for node in (root.id, mid.id, leaf.id):
    assert len([e for e in tree.events.load_events(node) if e["type"] == ET.TASK_REOPENED]) == 1


@pytest.mark.asyncio
async def test_restore_of_an_open_node_is_a_no_op_and_operator_only(tmp_path: Path) -> None:
  _cfg, _session_mgr, tree = build_env(tmp_path)
  root, _mid, _leaf, _completed = await build_tree(tree)
  assert await tree.completion.restore_chain(root.id, request_id="r-1", reason="sidebar unarchive") == []
  agent = CallerIdentity(kind="agent", claims=RunTokenClaims(run_id="run-1", session_id=root.id, agent="a"))
  from src.core.task_sessions import TaskForbiddenError
  with pytest.raises(TaskForbiddenError):
    await tree.completion.restore_task(root.id, request_id="r-2", reason="x", caller=agent)


# ---------------------------------------------------------------------------
# The input table
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_user_message_restores_and_is_the_rounds_only_input(tmp_path: Path) -> None:
  _cfg, _session_mgr, tree = build_env(tmp_path)
  root, mid, leaf, _completed = await build_tree(tree, with_old_input=True)
  await tree.archive_subtree(root.id, caller=OPERATOR)

  # finish=False: the round claims its batch and stays open, so the assertion
  # reads the round's own input batch instead of racing the completion close.
  executor = ScriptedExecutor(tree, finish=False)
  tree.dispatch.executor = executor
  admitted = await tree.dispatch.admit_input(
      leaf.id, event_type=ET.USER, content="continue from here", actor="user", input_id="msg-1")
  decision = await tree.dispatch.dispatch_pending(leaf.id)

  # The chain reopened topmost-first with the user message as the named source,
  # and the round took exactly one input.
  assert tree.task_state(root.id) == "open"
  assert tree.task_state(mid.id) == "open"
  assert tree.task_state(leaf.id) == "open"
  for node in (root.id, mid.id, leaf.id):
    reopens = [e for e in tree.events.load_events(node) if e["type"] == ET.TASK_REOPENED]
    assert [e["reason"] for e in reopens] == ["user message"]
  assert decision["launch"] is True
  assert executor.batches == [(leaf.id, ["msg-1"])]
  assert str(admitted["id"]) == "msg-1"


@pytest.mark.asyncio
async def test_user_message_announces_every_restored_node(tmp_path: Path) -> None:
  """The restore the user message rides announces one reopened fact per
  restored node from its own node, after the control lock releases — the
  same after-lock announcement restore_chain makes on the sidebar path."""
  _cfg, session_mgr, tree = build_env(tmp_path)
  root, mid, leaf, _completed = await build_tree(tree)
  await tree.archive_subtree(root.id, caller=OPERATOR)

  announce = mock.AsyncMock()
  with mock.patch.object(session_mgr, "announce_appended_event", new=announce):
    await tree.dispatch.admit_input(leaf.id, event_type=ET.USER, content="resume", actor="user")

  announced = [(call.args[0], call.args[1]["type"]) for call in announce.await_args_list]
  assert announced == [
      (root.id, ET.TASK_REOPENED),
      (mid.id, ET.TASK_REOPENED),
      (leaf.id, ET.TASK_REOPENED),
      (leaf.id, ET.USER),
  ]
  # Each announced reopen fact is the node's own durable fact, not a stand-in.
  for node_id in (root.id, mid.id, leaf.id):
    reopens = [e for e in tree.events.load_events(node_id) if e["type"] == ET.TASK_REOPENED]
    assert len(reopens) == 1
    announced_event = next(c.args[1] for c in announce.await_args_list if c.args[0] == node_id)
    assert str(announced_event["id"]) == str(reopens[0]["id"])


@pytest.mark.asyncio
async def test_agent_message_to_an_archived_node_is_refused_with_the_archived_sentence(tmp_path: Path) -> None:
  _cfg, _session_mgr, tree = build_env(tmp_path)
  root, mid, leaf, _completed = await build_tree(tree)
  await tree.archive_subtree(root.id, caller=OPERATOR)

  with pytest.raises(TaskArchivedError) as excinfo:
    await tree.dispatch.admit_input(
        leaf.id, event_type=ET.AGENT_MESSAGE, content="any instruction", actor="agent", from_session=mid.id)
  assert str(excinfo.value) == f"task {leaf.id} is archived"
  assert excinfo.value.blockers == [f"task {leaf.id} is archived"]
  # Nothing was admitted and nothing woke.
  assert input_events(tree, leaf.id) == []


@pytest.mark.asyncio
async def test_child_report_into_an_archived_parent_is_history_and_wakes_nobody(tmp_path: Path) -> None:
  _cfg, _session_mgr, tree = build_env(tmp_path)
  root, mid, _leaf, _completed = await build_tree(tree)
  await tree.archive_subtree(root.id, caller=OPERATOR)

  executor = ScriptedExecutor(tree)
  tree.dispatch.executor = executor
  report, created = await tree.dispatch.deliver_child_report(
      mid.id,
      source_event={"id": "late-result"},
      outcome="completed",
      summary="finished anyway",
      result_refs=[],
      recipient=mid.id)

  # Delivered (the stable id is in the parent's history), never a candidate,
  # and no wake: the executor saw no dispatch for the archived parent.
  assert report is not None and created
  assert (mid.id, "late-result") in tree.facts_of(mid.id).delivered_reports
  assert tree.dispatch.pending_inputs(mid.id) == []
  assert executor.batches == []


# ---------------------------------------------------------------------------
# The stored-status path for sessions without a profile
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_legacy_session_keeps_its_archive_and_unarchive_behavior(tmp_path: Path) -> None:
  _cfg, session_mgr, _tree = build_env(tmp_path)
  legacy = await session_mgr.create_session(CreateSessionRequest(name="Legacy"), backend=OPUS_BACKEND_ID)

  assert legacy.profile is None
  archived = await session_mgr.archive_session(legacy.id)
  assert archived is not None and archived.status == SessionStatus.ARCHIVED
  restored = await session_mgr.unarchive_session(legacy.id)
  assert restored is not None and restored.status == SessionStatus.ACTIVE

  # An empty legacy session still takes the permanent-delete path.
  empty = await session_mgr.create_session(CreateSessionRequest(name="Empty"), backend=OPUS_BACKEND_ID)
  assert session_mgr.get_chat_event_count_sync(empty.id, empty) == 0
  await session_mgr.delete_session_permanently(empty.id)
  assert await session_mgr.get_session(empty.id) is None
  assert SessionManager is not None  # the real manager drove every step
