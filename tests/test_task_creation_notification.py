"""Task creation reaches already-connected clients.

The original defect: ``TaskTreeManager.create_task`` atomically published
metadata + the ``task_created`` fact, invalidated caches and returned without
the tree notification the control-event sink emits — only the creating
browser's own refetch compensated, so a creation from another browser, the CLI
or an agent stayed invisible in a connected tree until an unrelated later fact
arrived. The repair rides the existing best-effort notification seam after the
durable publication and the cache/index invalidation: an observer receiving
the signal can immediately read the new node, its parent and the refreshed
ancestor counts.
"""

from __future__ import annotations

from pathlib import Path

import pytest
import pytest_asyncio
from conftest import make_home_config

from src.core import event_types as ET
from src.core.models import RunRecord, TaskSpec
from src.core.run_token import CallerIdentity, RunTokenClaims
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskInvalidError, TaskNotFoundError, TaskTreeManager

OP = CallerIdentity(kind="operator")


@pytest_asyncio.fixture
async def env(tmp_path: Path):
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  tree = TaskTreeManager(cfg, session_mgr)
  root = await tree.create_task(
      request_id="root", task_parent_id=None, profile="manager", task=None, name="Root",
      backend=None, caller=OP)
  return tree, session_mgr, root.id


class NotificationSpy:
  """Records the tree notifications create_task emits, capturing what an
  observer can read at signal time."""

  def __init__(self, tree: TaskTreeManager) -> None:
    self.calls: list[tuple[str, str | None]] = []
    self.readable_at_signal: list[dict] = []
    self._tree = tree
    self._orig = tree.events.notify_tree_changed

  async def _spy(self, session_id: str, event_type: str | None) -> None:
    self.calls.append((session_id, event_type))
    meta = await self._tree.load_meta(session_id)
    index = await self._tree._get_index()
    row = self._tree.session_row(index, session_id) if meta is not None else None
    parent_row = (
        self._tree.session_row(index, row.task_parent_id)
        if row is not None and row.task_parent_id else None)
    self.readable_at_signal.append({
        "node": row.model_dump() if row else None,
        "parent": parent_row.model_dump() if parent_row else None,
    })
    await self._orig(session_id, event_type)

  def install(self) -> None:
    self._tree.events.notify_tree_changed = self._spy  # type: ignore[method-assign]


@pytest.mark.asyncio
async def test_create_notifies_with_the_node_readable_at_signal(env) -> None:
  tree, _session_mgr, root_id = env
  spy = NotificationSpy(tree)
  spy.install()
  child = await tree.create_task(
      request_id="c1", task_parent_id=root_id, profile="worker",
      task=TaskSpec(goal="child goal"), name="Child", backend=None, caller=OP)
  assert spy.calls == [(child.id, ET.TASK_CREATED)]
  signal = spy.readable_at_signal[0]
  assert signal["node"] is not None and signal["node"]["id"] == child.id, (
    "the observer can read the new node as soon as the signal arrives")
  assert signal["parent"] is not None and signal["parent"]["id"] == root_id
  assert signal["parent"]["child_count"] == 1, (
    "the parent row already carries the refreshed child count at signal time")
  assert signal["parent"]["open_descendant_count"] == 1


@pytest.mark.asyncio
async def test_root_creation_notifies_and_roots_level_sees_it(env) -> None:
  tree, _session_mgr, _root_id = env
  spy = NotificationSpy(tree)
  spy.install()
  created = await tree.create_task(
      request_id="root-2", task_parent_id=None, profile="manager",
      task=TaskSpec(goal="second tree"), name="Root two", backend=None, caller=OP)
  assert spy.calls == [(created.id, ET.TASK_CREATED)]
  index = await tree._get_index()
  assert created.id in tree._children_of(index, None)


@pytest.mark.asyncio
async def test_deep_creation_counts_are_fresh_at_signal(env) -> None:
  tree, _session_mgr, root_id = env
  mid = await tree.create_task(
      request_id="mid", task_parent_id=root_id, profile="manager", task=None,
      name="Mid", backend=None, caller=OP)
  spy = NotificationSpy(tree)
  spy.install()
  leaf = await tree.create_task(
      request_id="leaf", task_parent_id=mid.id, profile="worker", task=None,
      name="Leaf", backend=None, caller=OP)
  assert spy.calls == [(leaf.id, ET.TASK_CREATED)]
  signal = spy.readable_at_signal[0]
  # The creation is deeper than one level: every ancestor row the signal
  # touches already reflects it.
  assert signal["parent"]["id"] == mid.id and signal["parent"]["child_count"] == 1
  index = await tree._get_index()
  root_row = tree.session_row(index, root_id)
  assert root_row.open_descendant_count == 2 and root_row.child_count == 1


@pytest.mark.asyncio
async def test_replayed_create_yields_one_task_one_fact_and_one_signal(env) -> None:
  tree, _session_mgr, root_id = env
  spy = NotificationSpy(tree)
  spy.install()
  body = {"goal": "worker goal"}
  first = await tree.create_task(
      request_id="same", task_parent_id=root_id, profile="worker",
      task=TaskSpec(**body), name="W", backend=None, caller=OP)
  replay = await tree.create_task(
      request_id="same", task_parent_id=root_id, profile="worker",
      task=TaskSpec(**body), name="W", backend=None, caller=OP)
  assert replay.id == first.id
  facts = [e for e in tree.events.load_events(first.id) if e.get("type") == ET.TASK_CREATED]
  assert len(facts) == 1, "a replayed request never appends a second creation fact"
  assert len(spy.calls) == 1, "only the publication signals; the replay publishes nothing"


@pytest.mark.asyncio
async def test_notification_failure_after_publish_does_not_fail_the_creation(
    env, monkeypatch: pytest.MonkeyPatch) -> None:
  tree, session_mgr, root_id = env

  async def boom(session_id: str, event_type: str | None) -> None:
    raise RuntimeError("broadcast socket exploded")

  monkeypatch.setattr(session_mgr, "broadcast_task_tree_changed", boom)
  created = await tree.create_task(
      request_id="c2", task_parent_id=root_id, profile="worker", task=None,
      name="Child", backend=None, caller=OP)
  # The creation itself succeeded and stays recoverable from durable facts.
  meta = await tree.load_meta(created.id)
  assert meta is not None and meta.name == "Child"
  facts = [e for e in tree.events.load_events(created.id) if e.get("type") == ET.TASK_CREATED]
  assert len(facts) == 1


@pytest.mark.asyncio
async def test_failed_prepublication_create_emits_no_signal(env) -> None:
  tree, _session_mgr, root_id = env
  spy = NotificationSpy(tree)
  spy.install()
  with pytest.raises(TaskInvalidError):
    await tree.create_task(
        request_id="bad", task_parent_id=root_id, profile="boss", task=None,
        name="Bad", backend=None, caller=OP)
  assert spy.calls == [], "a failed create never emits a successful-node signal"
  # A worker parent (not a manager) is refused before any publication.
  worker = await tree.create_task(
      request_id="w0", task_parent_id=root_id, profile="worker", task=None,
      name="W0", backend=None, caller=OP)
  with pytest.raises(TaskInvalidError):
    await tree.create_task(
        request_id="under-worker", task_parent_id=worker.id, profile="worker",
        task=None, name="Nope", backend=None, caller=OP)
  # An unknown parent is refused at the lookup guard.
  with pytest.raises(TaskNotFoundError):
    await tree.create_task(
        request_id="under-missing", task_parent_id="00000000-0000-0000-0000-00000000dead",
        profile="worker", task=None, name="Nope", backend=None, caller=OP)
  assert spy.calls == [(worker.id, ET.TASK_CREATED)], (
    "only the successful publication signaled; every refused create emitted nothing")


@pytest.mark.asyncio
async def test_agent_scoped_creation_notifies_the_tree(env) -> None:
  tree, session_mgr, root_id = env
  import os

  from src.core.runs import read_pid_stat
  pid_start, _state = read_pid_stat(os.getpid())
  await tree.runs.register_run(
      RunRecord(id="agent-run", session_id=root_id, pid=os.getpid(), pid_start=pid_start))
  claims = RunTokenClaims(session_id=root_id, run_id="agent-run", agent="subagent")
  agent = CallerIdentity(kind="agent", claims=claims)
  # The agent's own manager task must carry a real user instruction with the
  # takeoff authorization (the nearest-real-user-ancestor gate).
  await tree.events.append(root_id, {
      "id": "u1", "type": ET.USER, "timestamp": "2026-01-01T00:00:00+00:00",
      "content": "take off and build the thing"})
  spy = NotificationSpy(tree)
  spy.install()
  child = await tree.create_task(
      request_id="agent-child", task_parent_id=root_id, profile="worker",
      task=TaskSpec(goal="agent worker goal"), name=None, backend=None, caller=agent)
  assert spy.calls == [(child.id, ET.TASK_CREATED)], (
    "an agent-created worker publishes the same creation signal to connected clients")
