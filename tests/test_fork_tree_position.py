"""Clone and elone keep the source node's position in the task tree.

An elone replaces the source in place: the new node takes over the source's
decomposition edge and its whole subtree, and the source closes with the
archive format. A clone adds a childless sibling under the same parent. The
change adds no persistent format: only existing metadata fields and existing
event types carry it. Every tree assertion here runs against the real
``SessionFork`` and ``TaskTreeManager`` blocks.
"""

from __future__ import annotations

import pathlib
import subprocess

import conftest
import pytest
import yaml
from conftest import ScriptedRelayBackend, temp_home, write_nightly_task  # noqa: F401
from tests.test_task_execution import (
    OP_HEADERS,
    SpawningScriptedBackend,
    _adapter_with_silent_broadcast,
    build_env,
    make_api_client,
    result_event,
    stub_credentials,
    wait_for_terminal_run,
)

from src.features.cron import cron_files, cron_sequence
from src.features.cron import loader as cron_loader
from src.infra import event_types as ET
from src.infra import metadata_slots, models
from src.infra.models import LastRunStatus
from src.runtime import takeoff_gate, task_completion


async def make_tree(tmp_path: pathlib.Path) -> tuple[object, conftest.SessionBlocks, conftest.TaskTreeManager]:
  """The real blocks and the real task tree over one synthetic home."""
  return conftest.build_env(tmp_path)


async def manager_child(
    tree: conftest.TaskTreeManager,
    parent: str,
    request_id: str,
    *,
    profile: str = "manager",
    task: models.TaskSpec | None = None,
    name: str | None = None):
  return await conftest.create_task(tree, parent=parent, request_id=request_id, profile=profile, task=task, name=name)


def close_facts(tree: conftest.TaskTreeManager, session_id: str) -> list[dict]:
  return [e for e in tree.fact_history(session_id) if e.get("type") == ET.TASK_CLOSED]


def report_facts(tree: conftest.TaskTreeManager, session_id: str) -> list[dict]:
  return [e for e in tree.fact_history(session_id) if e.get("type") == ET.CHILD_REPORT]


def event_ids(tree: conftest.TaskTreeManager, session_id: str) -> list[str]:
  return [str(e.get("id")) for e in tree.fact_history(session_id)]


def kill(proc: subprocess.Popen) -> None:
  proc.kill()
  proc.wait()


# ---------------------------------------------------------------------------
# (a) (b) Clone keeps the position
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_clone_child_manager_copies_position_and_stays_childless(tmp_path: pathlib.Path) -> None:
  """(a) A child manager's clone sits under the same parent with the source's
  task, group and prompt-rule references, holds no children, and leaves the
  source and its subtree untouched."""
  cfg, blocks, tree = await make_tree(tmp_path)
  root = await manager_child(tree, None, "root", task=models.TaskSpec(goal="project"), name="Root")
  source = await manager_child(tree, root.id, "source", task=models.TaskSpec(goal="feature"), name="Feature")
  source_child = await manager_child(tree, source.id, "source-child", name="Feature child")
  source.subtree_prompt_ref = "sha-subtree"
  source.node_prompt_ref = "sha-node"
  source.group = "quant"
  await blocks.store.save_metadata(source)

  clone = await blocks.fork.fork_session(source.id)

  fresh = await blocks.store.get_session(clone.id)
  assert fresh is not None
  assert fresh.task_parent_id == root.id
  assert fresh.profile == "manager"
  assert fresh.task is not None and fresh.task.goal == "feature"
  assert fresh.group == "quant"
  assert fresh.subtree_prompt_ref == "sha-subtree"
  assert fresh.node_prompt_ref == "sha-node"
  assert fresh.parent_session_id == source.id
  index = await tree._get_index(force=True)
  assert tree._children_of(index, clone.id) == []
  # The source keeps its position and its subtree.
  fresh_source = await blocks.store.get_session(source.id)
  assert fresh_source is not None and fresh_source.task_parent_id == root.id
  fresh_child = await blocks.store.get_session(source_child.id)
  assert fresh_child is not None and fresh_child.task_parent_id == source.id
  assert tree.task_state(source.id) == "open"


@pytest.mark.asyncio
async def test_clone_root_manager_is_a_root(tmp_path: pathlib.Path) -> None:
  """(b) A root manager's clone is a root too."""
  cfg, blocks, tree = await make_tree(tmp_path)
  root = await manager_child(tree, None, "root", task=models.TaskSpec(goal="project"), name="Root")

  clone = await blocks.fork.fork_session(root.id)

  fresh = await blocks.store.get_session(clone.id)
  assert fresh is not None and fresh.task_parent_id is None
  index = await tree._get_index(force=True)
  assert clone.id in tree._children_of(index, None)


# ---------------------------------------------------------------------------
# (c) Elone moves the subtree and archives the source
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_elone_moves_subtree_and_archives_source(tmp_path: pathlib.Path) -> None:
  """(c) An elone of a child manager re-parents both children (a running child
  included), the running child's later report reaches the new node, the source
  closes archived with a null report_to and names its successor, and the
  parent's completion check no longer sees the source as open."""
  cfg, blocks, tree = await make_tree(tmp_path)
  root = await manager_child(tree, None, "root", task=models.TaskSpec(goal="project"), name="Root")
  source = await manager_child(tree, root.id, "source", task=models.TaskSpec(goal="feature"), name="Feature")
  still_child = await manager_child(tree, source.id, "c1", name="C1")
  running = await manager_child(tree, source.id, "c2", profile="worker", name="C2")
  proc = conftest.live_subprocess()
  try:
    pid, pid_start = conftest.identity_of(proc.pid)
    run = await tree.runs.register_run(
        models.RunRecord(
            id="run-live", session_id=running.id, pid=pid, pid_start=pid_start, started_at=models.utc_now()))

    new = await blocks.fork.elone_session(source.id, event_index=0)

    fresh_still = await blocks.store.get_session(still_child.id)
    fresh_running = await blocks.store.get_session(running.id)
    assert fresh_still is not None and fresh_still.task_parent_id == new.id
    assert fresh_running is not None and fresh_running.task_parent_id == new.id

    # The running child's later report reads its current parent: the new node.
    await tree.dispatch.finish_run(running.id, run.id, outcome="failed", exit_code=1)
    adapter = conftest.build_execution_adapter(cfg, blocks, tree)
    await adapter._report_failure_to_parent(running.id, run, "failed")
    reports = report_facts(tree, new.id)
    assert [r.get("child_session_id") for r in reports] == [running.id]

    closes = close_facts(tree, source.id)
    assert len(closes) == 1
    assert closes[0].get("outcome") == "archived"
    assert closes[0].get("report_to") is None
    fresh_source = await blocks.store.read_metadata_fresh(source.id)
    assert fresh_source is not None
    assert fresh_source.status == models.SessionStatus.ARCHIVED
    assert fresh_source.successor_session_id == new.id

    # The parent's completion check is not blocked by the source (the new node
    # is the open child that took the position).
    async with tree.control_lock:
      blockers = tree.completion.completion_blockers(
          await tree._get_index(force=True), root.id, exclude_run_ids=None, exclude_input_ids=None)
    assert f"has open descendant task {source.id}" not in blockers
  finally:
    kill(proc)


# ---------------------------------------------------------------------------
# (d) Children created after the cut point move too
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_elone_moves_children_created_after_the_cut(tmp_path: pathlib.Path) -> None:
  """(d) The move reads the current tree, not the copied log: a child the cut
  point predates still re-parents to the new node."""
  cfg, blocks, tree = await make_tree(tmp_path)
  root = await manager_child(tree, None, "root", name="Root")
  source = await manager_child(tree, root.id, "source", name="Feature")
  # The cut at index 0 copies only the creation fact; this child is created
  # after that point in the source's life.
  late_child = await manager_child(tree, source.id, "late-child", name="Late")

  new = await blocks.fork.elone_session(source.id, event_index=0)

  fresh = await blocks.store.get_session(late_child.id)
  assert fresh is not None and fresh.task_parent_id == new.id
  index = await tree._get_index(force=True)
  assert sorted(tree._children_of(index, new.id)) == [late_child.id]
  # The copied history stays cut at the creation fact: the child came from the
  # live tree, not from the copied log.
  copied = tree.fact_history(new.id)
  assert [e.get("type") for e in copied] == [ET.TASK_CREATED, ET.CLONE_START, ET.TASK_CREATED]


# ---------------------------------------------------------------------------
# (e) A bound scheduled task moves to the new node
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_elone_rebinds_scheduled_task_and_copies_bookkeeping(
    tmp_path: pathlib.Path, temp_home: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(e) The elone of a bound node rewrites the yaml's ``session_id``, copies
  the scheduler bookkeeping, and the next fire resolves to the new node."""
  cfg, blocks, tree = await make_tree(tmp_path)
  source = await manager_child(tree, None, "source", task=models.TaskSpec(goal="nightly"), name="nightly")
  write_nightly_task(temp_home)
  cron_files.write_cron_key("nightly", "session_id", source.id)
  fired_at = "2026-01-02T03:04:05+00:00"
  await tree.update_slot_fields(
      source.id,
      "cron",
      last_scheduled_run=fired_at,
      last_scheduled_cron="0 3 * * *",
      last_run_status=LastRunStatus.SUCCESS)

  new = await blocks.fork.elone_session(source.id, event_index=0)

  body = yaml.safe_load((temp_home / ".charliebot" / "config.d" / "cron.d" / "nightly.yaml").read_text())
  assert body["session_id"] == new.id
  assert cron_sequence.bound_task_name(new.id) == "nightly"
  assert cron_sequence.bound_task_name(source.id) is None
  fresh_new = await blocks.store.get_session(new.id)
  assert fresh_new is not None
  cron_fields = metadata_slots.fields_of(fresh_new, "cron")
  assert cron_fields.last_scheduled_run == fired_at
  assert cron_fields.last_scheduled_cron == "0 3 * * *"
  assert cron_fields.last_run_status == LastRunStatus.SUCCESS
  # The next fire lands on the new node.
  task_cfg = next(t for t in cron_loader.get_scheduled_tasks() if t.name == "nightly")
  assert (await cron_sequence.check_fireable_binding(task_cfg, tree)).id == new.id


# ---------------------------------------------------------------------------
# (f) (j) The bootstrap is the new node's first and only input
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_clone_bootstrap_is_the_only_pending_input(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(f) Copied unprocessed inputs stay off the new node; the clone's first
  round's only input is the route's bootstrap prompt."""
  cfg, blocks, tree = build_env(tmp_path, monkeypatch)
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  root = await manager_child(tree, None, "root", name="Root")
  source = await manager_child(tree, root.id, "source", name="Feature")
  copied_input = await tree.dispatch.admit_input(
      source.id, event_type=ET.USER, content="unhandled user instruction", actor="user")

  with make_api_client(cfg, blocks, tree) as client:
    response = client.post(f"/api/sessions/{source.id}/fork", headers=OP_HEADERS)
    assert response.status_code == 200, response.text
    clone_id = response.json()["id"]

    pending = tree.dispatch.pending_inputs(clone_id)
    assert len(pending) == 1
    bootstrap = pending[0]
    assert bootstrap.get("id") != str(copied_input.get("id"))
    assert "continues a prior conversation" in str(bootstrap.get("content"))


@pytest.mark.asyncio
async def test_bootstrap_names_the_tree_sentences(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(j) The first message of a clone and of an elone carries the two
  ``session tree`` sentences with the new node's id."""
  cfg, blocks, tree = build_env(tmp_path, monkeypatch)
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  root = await manager_child(tree, None, "root", name="Root")
  source = await manager_child(tree, root.id, "source", name="Feature")

  with make_api_client(cfg, blocks, tree) as client:
    forked = client.post(f"/api/sessions/{source.id}/fork", headers=OP_HEADERS)
    assert forked.status_code == 200, forked.text
    clone_id = forked.json()["id"]
    eloned = client.post(f"/api/sessions/{source.id}/elone", json={"event_index": 0}, headers=OP_HEADERS)
    assert eloned.status_code == 200, eloned.text
    new_id = eloned.json()["id"]

    clone_bootstrap = " ".join(str(e.get("content")) for e in tree.dispatch.pending_inputs(clone_id))
    assert f"Run `charliebot session tree --root {clone_id}` to list your current child tasks." in clone_bootstrap
    assert "Child tasks in the copied history can belong to the source session." in clone_bootstrap
    elone_bootstrap = " ".join(str(e.get("content")) for e in tree.dispatch.pending_inputs(new_id))
    assert f"Run `charliebot session tree --root {new_id}` to list your current child tasks." in elone_bootstrap
    assert "Child tasks in the copied history can belong to the source session." in elone_bootstrap


# ---------------------------------------------------------------------------
# (g) An unfinished run refuses the elone with nothing written
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_elone_with_unfinished_run_refuses_and_writes_nothing(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(g) A queued run on the source refuses the elone with HTTP 409, and no
  node, fact or status change lands."""
  cfg, blocks, tree = build_env(tmp_path, monkeypatch)
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  root = await manager_child(tree, None, "root", name="Root")
  source = await manager_child(tree, root.id, "source", name="Feature")
  await tree.runs.register_run(models.RunRecord(id="run-queued", session_id=source.id))
  sessions_before = {p.name for p in cfg.sessions_dir.iterdir()}
  events_before = event_ids(tree, source.id)

  with make_api_client(cfg, blocks, tree) as client:
    response = client.post(f"/api/sessions/{source.id}/elone", json={"event_index": 0}, headers=OP_HEADERS)
    assert response.status_code == 409
    assert "run-queued" in str(response.json()["detail"])

  assert {p.name for p in cfg.sessions_dir.iterdir()} == sessions_before
  assert event_ids(tree, source.id) == events_before
  fresh_source = await blocks.store.read_metadata_fresh(source.id)
  assert fresh_source is not None
  assert fresh_source.status == models.SessionStatus.ACTIVE
  assert fresh_source.successor_session_id is None


# ---------------------------------------------------------------------------
# (h) A worker source keeps the ordinary fork shape
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_worker_source_clone_and_elone_keep_the_ordinary_shape(tmp_path: pathlib.Path) -> None:
  """(h) Clone and elone of a worker node behave as on origin/main: the child
  is a manager root with no position, and the elone archives the source with
  its successor pointer but writes no close fact."""
  cfg, blocks, tree = await make_tree(tmp_path)
  root = await manager_child(tree, None, "root", name="Root")
  worker = await manager_child(tree, root.id, "worker", profile="worker", name="W")

  clone = await blocks.fork.fork_session(worker.id)
  fresh_clone = await blocks.store.get_session(clone.id)
  assert fresh_clone is not None
  assert fresh_clone.profile == "manager"
  assert fresh_clone.task_parent_id is None and fresh_clone.task is None
  assert fresh_clone.parent_session_id == worker.id

  new = await blocks.fork.elone_session(worker.id, event_index=0)
  fresh_new = await blocks.store.get_session(new.id)
  assert fresh_new is not None
  assert fresh_new.profile == "manager"
  assert fresh_new.task_parent_id is None and fresh_new.task is None
  fresh_worker = await blocks.store.read_metadata_fresh(worker.id)
  assert fresh_worker is not None
  assert fresh_worker.status == models.SessionStatus.ARCHIVED
  assert fresh_worker.successor_session_id == new.id
  assert close_facts(tree, worker.id) == []


# ---------------------------------------------------------------------------
# (i) The parent node's log stays untouched, no parent round starts
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_clone_and_elone_write_nothing_into_the_parent_log(tmp_path: pathlib.Path) -> None:
  """(i) Neither operation appends a line to the tree parent's log or starts a
  parent round."""
  cfg, blocks, tree = await make_tree(tmp_path)
  root = await manager_child(tree, None, "root", name="Root")
  source = await manager_child(tree, root.id, "source", name="Feature")
  parent_events_before = event_ids(tree, root.id)

  clone = await blocks.fork.fork_session(source.id)
  assert event_ids(tree, root.id) == parent_events_before
  assert tree.runs.list_run_records_sync(root.id) == []

  await blocks.fork.elone_session(source.id, event_index=0)
  assert event_ids(tree, root.id) == parent_events_before
  assert tree.runs.list_run_records_sync(root.id) == []
  assert clone.id


# ---------------------------------------------------------------------------
# (k) The report header names the fork origin
# ---------------------------------------------------------------------------


async def deliver_locked(
    tree: conftest.TaskTreeManager, child_id: str, recipient: str, outcome: str = "completed") -> dict:
  """One real child_report delivery without the wake, for a test that
  dispatches the parent's turn itself."""
  source_event = next(
      e for e in reversed(tree.fact_history(child_id)) if e.get("type") in (ET.RUN_FINISHED, ET.TASK_CREATED))
  async with tree.control_lock:
    report, _created = await tree.dispatch.deliver_child_report_locked(
        child_id,
        source_event=source_event,
        outcome=outcome,
        summary="did the work",
        result_refs=[],
        recipient=recipient,
        actor="agent")
  assert report
  return report


@pytest.mark.asyncio
async def test_report_header_shows_clone_provenance(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(k) A cloned node's report header reads ``clone of <source id>`` in the
  parent's turn prompt."""
  cfg, blocks, tree = build_env(tmp_path, monkeypatch)
  root = await manager_child(tree, None, "root", name="Root")
  source = await manager_child(tree, root.id, "source", name="Feature")
  clone = await blocks.fork.fork_session(source.id)
  await deliver_locked(tree, clone.id, root.id)

  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, blocks, tree, monkeypatch)
  builds = conftest.install_scripted_backends(
      monkeypatch, [ScriptedRelayBackend([result_event("ack")], exit_code=0)], conftest.BUILD_BACKEND_PATCH_TARGET)
  decision = await tree.dispatch.dispatch_pending(root.id)
  assert decision["launch"] is True
  await wait_for_terminal_run(tree, root.id, decision["run_id"])
  prompt = builds[0]["backend"].prompt
  assert f"[Report from task {clone.id} (clone of {source.id}) | outcome completed]" in prompt


@pytest.mark.asyncio
async def test_report_header_shows_elone_provenance(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(k) An eloned node's report header reads ``elone of <source id>``."""
  cfg, blocks, tree = build_env(tmp_path, monkeypatch)
  root = await manager_child(tree, None, "root", name="Root")
  source = await manager_child(tree, root.id, "source", name="Feature")
  new = await blocks.fork.elone_session(source.id, event_index=0)
  await deliver_locked(tree, new.id, root.id)

  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, blocks, tree, monkeypatch)
  builds = conftest.install_scripted_backends(
      monkeypatch, [ScriptedRelayBackend([result_event("ack")], exit_code=0)], conftest.BUILD_BACKEND_PATCH_TARGET)
  decision = await tree.dispatch.dispatch_pending(root.id)
  assert decision["launch"] is True
  await wait_for_terminal_run(tree, root.id, decision["run_id"])
  prompt = builds[0]["backend"].prompt
  assert f"[Report from task {new.id} (elone of {source.id}) | outcome completed]" in prompt


# ---------------------------------------------------------------------------
# (l) A closed parent refuses both with nothing written
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_closed_parent_refuses_clone_and_elone(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(l) A closed parent task returns 409 for clone and elone, and nothing is
  written."""
  cfg, blocks, tree = build_env(tmp_path, monkeypatch)
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  root = await manager_child(tree, None, "root", name="Root")
  source = await manager_child(tree, root.id, "source", name="Feature")
  event = tree.archived_close_event(root.id, "close-root", summary="archived by the user")
  await tree.events.append(root.id, event)
  assert tree.task_state(root.id) == "archived"
  sessions_before = {p.name for p in cfg.sessions_dir.iterdir()}
  events_before = event_ids(tree, source.id)

  with make_api_client(cfg, blocks, tree) as client:
    fork_response = client.post(f"/api/sessions/{source.id}/fork", headers=OP_HEADERS)
    assert fork_response.status_code == 409
    assert f"parent task {root.id} is archived" in str(fork_response.json()["detail"])
    elone_response = client.post(f"/api/sessions/{source.id}/elone", json={"event_index": 0}, headers=OP_HEADERS)
    assert elone_response.status_code == 409
    assert f"parent task {root.id} is archived" in str(elone_response.json()["detail"])

  assert {p.name for p in cfg.sessions_dir.iterdir()} == sessions_before
  assert event_ids(tree, source.id) == events_before
  fresh_source = await blocks.store.read_metadata_fresh(source.id)
  assert fresh_source is not None and fresh_source.successor_session_id is None


# ---------------------------------------------------------------------------
# (n) Copied take-off words authorize nobody on the new node
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_copied_pre_take_off_does_not_authorize_the_successor(tmp_path: pathlib.Path) -> None:
  """(n) A fresh pre take off in the source log authorizes the source itself,
  and neither its clone nor its elone child: the gate reads only the user
  messages after the newest clone_start."""
  cfg, blocks, tree = await make_tree(tmp_path)
  root = await manager_child(tree, None, "root", name="Root")
  source = await manager_child(tree, root.id, "source", name="Feature")
  await tree.dispatch.admit_input(source.id, event_type=ET.USER, content="pre take off, prepare the work", actor="user")
  assert await tree.check_task_authorization(source.id) == source.id

  clone = await blocks.fork.fork_session(source.id)
  with pytest.raises(takeoff_gate.DelegationBlockedError):
    await tree.check_task_authorization(clone.id)

  new = await blocks.fork.elone_session(source.id, event_index=0)
  with pytest.raises(takeoff_gate.DelegationBlockedError):
    await tree.check_task_authorization(new.id)


# ---------------------------------------------------------------------------
# (o) The undelivered close report lands before the source's close
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_elone_redelivers_subtree_close_report_before_the_close(tmp_path: pathlib.Path) -> None:
  """(o) A subtree close report the crash window lost is redelivered to the
  source first; the source receives no report after its own close fact, and a
  recovery pass duplicates nothing."""
  cfg, blocks, tree = await make_tree(tmp_path)
  root = await manager_child(tree, None, "root", name="Root")
  source = await manager_child(tree, root.id, "source", name="Feature")
  child = await manager_child(tree, source.id, "child", name="Child")
  # The crash window: the child's close fact is durable, its report is not.
  close = tree.archived_close_event(child.id, "close-child", summary="done")
  close["outcome"] = "completed"
  close["report_to"] = source.id
  await tree.events.append(child.id, close)
  assert report_facts(tree, source.id) == []

  new = await blocks.fork.elone_session(source.id, event_index=0)

  source_events = tree.fact_history(source.id)
  reports = [i for i, e in enumerate(source_events) if e.get("type") == ET.CHILD_REPORT]
  closes = [i for i, e in enumerate(source_events) if e.get("type") == ET.TASK_CLOSED]
  assert len(reports) == 1 and len(closes) == 1
  assert reports[0] < closes[0]
  assert source_events[reports[0]].get("child_session_id") == child.id
  assert source_events[closes[0]].get("outcome") == "archived"
  # A repeated recovery pass dedups: the stable report id already sits in the
  # source's log, so the subtree still owes nothing.
  assert await tree.dispatch.recover_pending_reports(child.id) == []
  assert new.id


# ---------------------------------------------------------------------------
# (p) A completed source's successor starts open and recovers to nothing
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_completed_source_successor_starts_open_and_recovers_to_nothing(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """(p) Clone and elone of a completed node: both successors are open, the
  bootstrap starts the elone successor's first round, and a recovery scan
  leaves the parent with no new line and no report from the new node."""
  cfg, blocks, tree = build_env(tmp_path, monkeypatch)
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  monkeypatch.setenv("CHARLIEBOT_HOME", str(cfg.charliebot_home))
  root = await manager_child(tree, None, "root", name="Root")
  source = await manager_child(tree, root.id, "source", name="Feature")
  await tree.completion.complete_task(
      source.id,
      request_id="complete-1",
      evidence=task_completion.CompletionEvidence(summary="done", result_refs=["ref-1"], run_ids=[]),
      caller=conftest.OPERATOR)
  assert tree.task_state(source.id) == "completed"
  assert len(close_facts(tree, source.id)) == 1
  root_events_before = event_ids(tree, root.id)
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, blocks, tree, monkeypatch)
  # The manager-turn build path takes one SpawningScriptedBackend per round.
  conftest.install_scripted_backends(
      monkeypatch,
      [SpawningScriptedBackend([result_event("clone round")]),
       SpawningScriptedBackend([result_event("elone round")])], conftest.BUILD_BACKEND_PATCH_TARGET)

  # The run executes on the API loop that scheduled it: the wait stays inside
  # the client context (the same rule the execution tests pin).
  with make_api_client(cfg, blocks, tree) as client:
    fork_response = client.post(f"/api/sessions/{source.id}/fork", headers=OP_HEADERS)
    assert fork_response.status_code == 200, fork_response.text
    clone_id = fork_response.json()["id"]
    clone_runs = tree.runs.list_run_records_sync(clone_id)
    assert len(clone_runs) == 1  # the bootstrap started the clone's first round
    await wait_for_terminal_run(tree, clone_id, clone_runs[0].id)

    elone_response = client.post(f"/api/sessions/{source.id}/elone", json={"event_index": 0}, headers=OP_HEADERS)
    assert elone_response.status_code == 200, elone_response.text
    new_id = elone_response.json()["id"]
    runs = tree.runs.list_run_records_sync(new_id)
    assert len(runs) == 1
    await wait_for_terminal_run(tree, new_id, runs[0].id)

  clone = await blocks.store.get_session(clone_id)
  assert clone is not None
  assert tree.task_state(clone.id) == "open"
  # The copied close belongs to the source: the fold resets it at clone_start.
  assert tree.facts_of(clone.id).close_events == []
  assert tree.task_state(new_id) == "open"
  # The completed close fact is kept, not written twice.
  assert len(tree.facts_of(source.id).close_events) == 1

  # The startup recovery scan over the new node repairs nothing and writes
  # nothing into the parent's log.
  assert await tree.dispatch.recover_pending_reports(new_id) == []
  assert event_ids(tree, root.id) == root_events_before
