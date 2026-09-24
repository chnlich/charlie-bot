"""Task-tree owner tests: stable creates, tree validation, derived state, mutation guards."""

from __future__ import annotations

import os
from datetime import UTC, datetime
from pathlib import Path

import pytest
from conftest import OPUS_BACKEND_ID, make_home_config

from src.core import event_types as ET
from src.core.control_events import sha256_hex, stable_task_id
from src.core.models import CreateSessionRequest, EventRef, PatchSessionTaskRequest, RunRecord, TaskSpec
from src.core.run_token import CallerIdentity
from src.core.runs import read_pid_stat
from src.core.sessions import SessionManager
from src.core.task_sessions import (
  TaskConflictError,
  TaskInvalidError,
  TaskNotFoundError,
  TaskTreeManager,
)

OPERATOR = CallerIdentity(kind="operator")


def build_env(tmp_path: Path) -> tuple[object, SessionManager, TaskTreeManager]:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  return cfg, session_mgr, TaskTreeManager(cfg, session_mgr)


async def create_task(mgr: TaskTreeManager, *, parent: str | None, profile: str = "manager",
                      request_id: str, name: str | None = None,
                      task: TaskSpec | None = None, **kwargs: object):
  return await mgr.create_task(
      request_id=request_id,
      task_parent_id=parent,
      profile=profile,
      task=task,
      name=name,
      backend=None,
      caller=OPERATOR,
      **kwargs,
  )


async def build_three_levels(mgr: TaskTreeManager) -> dict[str, str]:
  """root(m) -> mid(m) -> low(m) -> {worker1(w), worker2(w)}; returns ids by label."""
  root = await create_task(mgr, parent=None, request_id="root", name="Root")
  mid = await create_task(mgr, parent=root.id, request_id="mid", name="Mid")
  low = await create_task(mgr, parent=mid.id, request_id="low", name="Low")
  worker1 = await create_task(mgr, parent=low.id, request_id="w1", profile="worker", name="W1")
  worker2 = await create_task(mgr, parent=low.id, request_id="w2", profile="worker", name="W2")
  return {"root": root.id, "mid": mid.id, "low": low.id, "worker1": worker1.id, "worker2": worker2.id}


async def close_task(mgr: TaskTreeManager, session_id: str, outcome: str = "completed") -> None:
  """Append one task_closed fact through the control-event sink (the durable way a task ends)."""
  await mgr.events.append(session_id, {
      "id": f"close-{session_id}",
      "type": ET.TASK_CLOSED,
      "timestamp": datetime.now(UTC).isoformat(),
      "actor": "user",
      "source_session_id": session_id,
      "outcome": outcome,
      "summary": "s",
      "result_refs": [],
      "run_ids": [],
      "report_to": None,
  })
  mgr._index = None


# ---------------------------------------------------------------------------
# Tree shape
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_three_manager_depths_share_one_profile_and_workers_are_leaves(tmp_path: Path) -> None:
  _, session_mgr, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)

  for label in ("root", "mid", "low"):
    meta = await session_mgr.get_session(ids[label])
    assert meta is not None and meta.profile == "manager"  # one profile at every manager depth
    assert meta.schema_version == 2

  # A worker is a leaf: no task may be created under it.
  with pytest.raises(TaskInvalidError, match="not a manager"):
    await create_task(mgr, parent=ids["worker1"], profile="worker", request_id="under-worker")


@pytest.mark.asyncio
async def test_flat_paths_survive_reparenting(tmp_path: Path) -> None:
  cfg, session_mgr, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  attachment = cfg.sessions_dir / ids["worker1"] / "data" / "chat_events.jsonl"
  assert attachment.is_file()

  # Move worker1 from low to mid: the on-disk directory (and every log/attachment path) stays flat.
  await mgr.patch_task(ids["worker1"], PatchSessionTaskRequest(task_parent_id=ids["mid"]), caller=OPERATOR)
  assert attachment.is_file() and attachment == cfg.sessions_dir / ids["worker1"] / "data" / "chat_events.jsonl"
  meta = await session_mgr.get_session(ids["worker1"])
  assert meta is not None and meta.task_parent_id == ids["mid"]

  index = await mgr._get_index()
  assert ids["worker1"] in mgr._children_of(index, ids["mid"])


@pytest.mark.asyncio
async def test_history_copying_never_becomes_a_task_parent(tmp_path: Path) -> None:
  _, session_mgr, mgr = build_env(tmp_path)
  legacy = await session_mgr.create_session(CreateSessionRequest(name="legacy"), backend=OPUS_BACKEND_ID)
  legacy.parent_session_id = "some-old-session"
  legacy.origin_ref = EventRef(session_id="some-old-session", event_id=None)
  await session_mgr.save_metadata(legacy)

  index = await mgr._get_index()
  assert legacy.id not in index.children.get(None, [])  # not a task-tree node yet
  # The first child create adopts the legacy session as a manager node; the
  # history-copy fields never turn into tree parentage.
  child = await create_task(mgr, parent=legacy.id, request_id="child-of-legacy")
  fresh = await session_mgr.get_session(legacy.id)
  assert fresh is not None and fresh.task_parent_id is None
  assert fresh.profile == "manager" and fresh.schema_version == 2
  assert child.task_parent_id == legacy.id
  index = await mgr._get_index()
  assert legacy.id in index.children.get(None, [])
  assert child.id in mgr._children_of(index, legacy.id)


@pytest.mark.asyncio
async def test_adopt_legacy_session_writes_profile_and_schema_version_once(tmp_path: Path) -> None:
  _, session_mgr, mgr = build_env(tmp_path)
  legacy = await session_mgr.create_session(CreateSessionRequest(name="legacy"), backend=OPUS_BACKEND_ID)
  before = legacy.model_dump(exclude={"updated_at"})

  adopted = await mgr.adopt_legacy_session(legacy.id)

  assert adopted.profile == "manager" and adopted.schema_version == 2
  after = adopted.model_dump(exclude={"updated_at"})
  assert {k for k in before if before[k] != after[k]} == {"profile", "schema_version"}
  stored = await session_mgr.get_session(legacy.id)
  assert stored is not None and stored.profile == "manager" and stored.schema_version == 2
  # A second call is a read: nothing is rewritten.
  again = await mgr.adopt_legacy_session(legacy.id)
  assert again.updated_at == stored.updated_at
  # A node that already carries a profile keeps it.
  worker = await create_task(mgr, parent=adopted.id, request_id="w", profile="worker", name="W")
  assert (await mgr.adopt_legacy_session(worker.id)).profile == "worker"
  with pytest.raises(TaskNotFoundError):
    await mgr.adopt_legacy_session("missing")


@pytest.mark.asyncio
async def test_unnamed_create_without_a_goal_takes_the_session_counter_name(tmp_path: Path) -> None:
  _, _session_mgr, mgr = build_env(tmp_path)
  first = await create_task(mgr, parent=None, request_id="r1", task=TaskSpec(goal=""))
  second = await create_task(mgr, parent=None, request_id="r2", task=None)
  with_goal = await create_task(mgr, parent=first.id, request_id="w", profile="worker",
                                task=TaskSpec(goal="Fix the login\nsecond line"))
  named = await create_task(mgr, parent=None, request_id="r3", name="Given", task=None)

  assert first.name.startswith("Session ") and second.name.startswith("Session ")
  assert first.name != second.name
  assert with_goal.name == "Fix the login"
  assert named.name == "Given"


# ---------------------------------------------------------------------------
# Stable create / retry
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_create_is_stable_across_duplicate_request_and_fresh_reload(tmp_path: Path) -> None:
  _, session_mgr, mgr = build_env(tmp_path)
  root = await create_task(mgr, parent=None, request_id="req-1", name="Root")

  replay = await create_task(mgr, parent=None, request_id="req-1", name="Root")
  assert replay.id == root.id

  fresh_mgr = TaskTreeManager(make_home_config(tmp_path), session_mgr)
  replay_fresh = await create_task(fresh_mgr, parent=None, request_id="req-1", name="Root")
  assert replay_fresh.id == root.id

  # One node, one creation fact.
  assert (session_mgr.get_chat_events_path(root.id)).read_text().count(ET.TASK_CREATED) == 1
  meta = await session_mgr.get_session(root.id)
  assert meta is not None and meta.created_by_event is not None
  assert meta.created_by_event.session_id == root.id
  assert meta.created_by_event.event_id


@pytest.mark.asyncio
async def test_create_is_atomic_at_the_publish_seam(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _, _session_mgr, mgr = build_env(tmp_path)

  def explode(src: object, dst: object) -> None:
    raise OSError("crash before publication")

  monkeypatch.setattr("src.core.task_sessions.os.replace", explode)
  with pytest.raises(OSError, match="crash before publication"):
    await create_task(mgr, parent=None, request_id="crashy", name="X")
  monkeypatch.undo()

  # No node, no temp leftovers; the same request then creates cleanly.
  cfg = make_home_config(tmp_path)
  assert not (cfg.sessions_dir / stable_task_id(None, "crashy")).exists()
  assert not list(cfg.sessions_dir.glob(".task-*"))
  created = await create_task(mgr, parent=None, request_id="crashy", name="X")
  assert created.id == stable_task_id(None, "crashy")


@pytest.mark.asyncio
async def test_invalid_missing_and_closed_parents(tmp_path: Path) -> None:
  _, _, mgr = build_env(tmp_path)
  with pytest.raises(TaskNotFoundError):
    await create_task(mgr, parent="no-such-task", request_id="x")
  with pytest.raises(TaskInvalidError, match="profile must be"):
    await create_task(mgr, parent=None, profile="lead", request_id="x")

  ids = await build_three_levels(mgr)
  # A closed ancestor (beyond the direct parent) blocks creation.
  await close_task(mgr, ids["mid"])
  with pytest.raises(TaskConflictError, match="closed ancestor"):
    await create_task(mgr, parent=ids["low"], profile="worker", request_id="under-closed-ancestor")
  # A closed direct parent blocks with its own blocker.
  await close_task(mgr, ids["low"])
  with pytest.raises(TaskConflictError, match="is completed"):
    await create_task(mgr, parent=ids["low"], profile="worker", request_id="under-closed")


@pytest.mark.asyncio
async def test_reparent_rejects_cycles_and_closed_targets(tmp_path: Path) -> None:
  _, session_mgr, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)

  with pytest.raises(TaskConflictError, match="own subtree"):
    await mgr.patch_task(ids["root"], PatchSessionTaskRequest(task_parent_id=ids["low"]), caller=OPERATOR)
  with pytest.raises(TaskConflictError, match="own subtree"):
    await mgr.patch_task(ids["low"], PatchSessionTaskRequest(task_parent_id=ids["low"]), caller=OPERATOR)

  await close_task(mgr, ids["mid"])
  with pytest.raises(TaskConflictError, match="is completed"):
    await mgr.patch_task(ids["worker1"], PatchSessionTaskRequest(task_parent_id=ids["mid"]), caller=OPERATOR)
  # ...but moving to the open root works and history stays fixed.
  await mgr.patch_task(ids["worker2"], PatchSessionTaskRequest(task_parent_id=ids["root"]), caller=OPERATOR)
  meta = await session_mgr.get_session(ids["worker2"])
  assert meta is not None and meta.task_parent_id == ids["root"]


# ---------------------------------------------------------------------------
# Derived state, pagination, deletion
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_tree_pagination_counts_and_attention_ancestor_path(tmp_path: Path) -> None:
  _, _session_mgr, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)

  # One attention descendant: a launched run nobody observed exiting.
  run = await mgr.runs.register_run(
      RunRecord(id="run-attn", session_id=ids["worker1"], pid=os.getpid(), pid_start="1",
                started_at=datetime.now(UTC)))
  assert run.id == "run-attn"

  page = await mgr.tree_page(parent_id=None, include_archived=False, limit=100, cursor=None)
  root_row = next(r for r in page["items"] if r["id"] == ids["root"])
  assert root_row["task_state"] == "open"
  assert root_row["child_count"] == 1
  assert root_row["open_descendant_count"] == 4  # mid, low, worker1, worker2
  assert root_row["attention_descendant_count"] == 1

  detail = await mgr.session_detail(ids["worker1"])
  assert [(a["id"], a["name"]) for a in detail["ancestors"]] == [
      (ids["low"], "Low"), (ids["mid"], "Mid"), (ids["root"], "Root")]
  assert detail["work_state"] == "attention"
  assert detail["task_state"] == "open"

  # Stale pagination: the revision moves when the tree changes, and the old cursor 409s.
  await create_task(mgr, parent=None, request_id="root-2", name="Root2")  # two roots -> a real page 2
  first = await mgr.tree_page(parent_id=None, include_archived=False, limit=1, cursor=None)
  assert first["next_cursor"] is not None
  await create_task(mgr, parent=ids["root"], request_id="new-branch", name="New")
  with pytest.raises(TaskConflictError, match="changed during pagination"):
    await mgr.tree_page(parent_id=None, include_archived=False, limit=1, cursor=first["next_cursor"])


@pytest.mark.asyncio
async def test_task_state_rebuilds_from_close_and_reopen_facts(tmp_path: Path) -> None:
  _, _, mgr = build_env(tmp_path)
  root = await create_task(mgr, parent=None, request_id="root", name="Root")
  await close_task(mgr, root.id, outcome="cancelled")
  index = await mgr._get_index()
  assert mgr.task_state_of(index, root.id) == "cancelled"
  page = await mgr.tree_page(parent_id=None, include_archived=True, limit=10, cursor=None)
  row = next(r for r in page["items"] if r["id"] == root.id)
  assert row["task_state"] == "cancelled"
  assert row["open_descendant_count"] == 0


@pytest.mark.asyncio
async def test_permanent_delete_blockers(tmp_path: Path) -> None:
  cfg, _, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)

  blockers = await mgr.deletion_blockers(ids["low"])
  assert any("child task" in b for b in blockers)

  await mgr.runs.register_run(RunRecord(id="run-q", session_id=ids["worker1"]))
  blockers = await mgr.deletion_blockers(ids["worker1"])
  assert any("run record" in b for b in blockers)

  triggers_dir = cfg.sessions_dir / ids["worker2"] / "triggers"
  triggers_dir.mkdir(parents=True)
  (triggers_dir / "t.json").write_text("{}", encoding="utf-8")
  blockers = await mgr.deletion_blockers(ids["worker2"])
  assert any("trigger" in b for b in blockers)

  # A saved alias mapping referencing the session blocks deletion too.
  leaf = await create_task(mgr, parent=ids["root"], request_id="leafy", profile="worker", name="Leafy")
  mgr.aliases.put_old_session("old-leaf", leaf.id)
  blockers = await mgr.deletion_blockers(leaf.id)
  assert any("alias" in b for b in blockers)
  mgr.aliases.path.unlink()
  assert await mgr.deletion_blockers(leaf.id) == []


# ---------------------------------------------------------------------------
# Mutation guards
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_running_and_pending_runs_block_structural_edits(tmp_path: Path) -> None:
  _, session_mgr, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)

  # Queued (registered, never launched): a pending execution request.
  await mgr.runs.register_run(RunRecord(id="run-queued", session_id=ids["worker1"]))
  with pytest.raises(TaskConflictError, match="queued"):
    await mgr.patch_task(
        ids["worker1"], PatchSessionTaskRequest(task=TaskSpec(goal="edited")), caller=OPERATOR)

  # Live process: an active run.
  pid_start, _ = read_pid_stat(os.getpid())
  await mgr.runs.register_run(
      RunRecord(id="run-live", session_id=ids["worker2"], pid=os.getpid(), pid_start=pid_start,
                started_at=datetime.now(UTC)))
  with pytest.raises(TaskConflictError, match="active"):
    await mgr.patch_task(
        ids["worker2"], PatchSessionTaskRequest(task=TaskSpec(goal="edited")), caller=OPERATOR)

  # Terminal runs block nothing.
  await mgr.runs.record_finish(ids["worker1"], "run-queued", "failed")
  await mgr.patch_task(
      ids["worker1"], PatchSessionTaskRequest(task=TaskSpec(goal="edited")), caller=OPERATOR)
  meta = await session_mgr.get_session(ids["worker1"])
  assert meta is not None and meta.task is not None and meta.task.goal == "edited"


@pytest.mark.asyncio
async def test_worker_promotion_and_demotion_constraints(tmp_path: Path) -> None:
  _, _, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)

  # Demotion with children is blocked.
  with pytest.raises(TaskConflictError, match="no child tasks"):
    await mgr.patch_task(ids["low"], PatchSessionTaskRequest(profile="worker"), caller=OPERATOR)

  # Promotion of an idle childless worker is allowed and keeps the run history.
  await mgr.patch_task(ids["worker2"], PatchSessionTaskRequest(profile="manager"), caller=OPERATOR)
  child = await create_task(mgr, parent=ids["worker2"], profile="worker", request_id="grandchild")
  assert child.task_parent_id == ids["worker2"]

  # Promotion with a pending execution request is blocked.
  await mgr.runs.register_run(RunRecord(id="run-pending", session_id=ids["worker1"]))
  with pytest.raises(TaskConflictError, match="queued"):
    await mgr.patch_task(ids["worker1"], PatchSessionTaskRequest(profile="manager"), caller=OPERATOR)


@pytest.mark.asyncio
async def test_pending_input_seam_blocks_structural_edits(tmp_path: Path) -> None:
  """The input-delivery stage extends the guard through the seam; this pins its shape."""
  _, _, mgr = build_env(tmp_path)
  ids = await build_three_levels(mgr)
  mgr.pending_input_blockers = lambda session_id: [f"input pending in {session_id}"]
  with pytest.raises(TaskConflictError, match="input pending"):
    await mgr.patch_task(
        ids["worker1"], PatchSessionTaskRequest(task=TaskSpec(goal="edited")), caller=OPERATOR)
  mgr.pending_input_blockers = None
  await mgr.patch_task(
      ids["worker1"], PatchSessionTaskRequest(task=TaskSpec(goal="edited")), caller=OPERATOR)


# ---------------------------------------------------------------------------
# Prompt bodies
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_prompt_bodies_are_immutable_and_record_prompt_changed(tmp_path: Path) -> None:
  cfg, session_mgr, mgr = build_env(tmp_path)
  root = await create_task(mgr, parent=None, request_id="root", name="Root")

  await mgr.patch_task(root.id, PatchSessionTaskRequest(subtree_prompt="deliver with evidence"), caller=OPERATOR)
  ref = sha256_hex("deliver with evidence")
  body_path = cfg.charliebot_home / "prompt_bodies" / f"{ref}.md"
  assert body_path.read_text(encoding="utf-8") == "deliver with evidence"
  meta = await session_mgr.get_session(root.id)
  assert meta is not None and meta.subtree_prompt_ref == ref

  events = mgr.events.load_events(root.id)
  changed = [e for e in events if e["type"] == ET.PROMPT_CHANGED]
  assert changed[-1]["scope"] == "subtree" and changed[-1]["previous_ref"] is None and changed[-1]["new_ref"] == ref

  # Same body re-saved: same fingerprint, no second change event.
  await mgr.patch_task(root.id, PatchSessionTaskRequest(subtree_prompt="deliver with evidence"), caller=OPERATOR)
  events = mgr.events.load_events(root.id)
  assert len([e for e in events if e["type"] == ET.PROMPT_CHANGED]) == 1

  # Clearing records the replacement back to null.
  await mgr.patch_task(root.id, PatchSessionTaskRequest(subtree_prompt=None), caller=OPERATOR)
  meta = await session_mgr.get_session(root.id)
  assert meta is not None and meta.subtree_prompt_ref is None

  # A corrupted body store fails loud instead of silently referencing other text.
  body_path.write_text("tampered", encoding="utf-8")
  with pytest.raises(RuntimeError, match="fingerprint content mismatch"):
    await mgr.patch_task(root.id, PatchSessionTaskRequest(subtree_prompt="deliver with evidence"), caller=OPERATOR)
