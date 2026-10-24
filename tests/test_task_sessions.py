"""Task-tree owner tests: stable creates, tree validation, derived state, mutation guards."""

from __future__ import annotations

import asyncio
import datetime
import json
import os
import pathlib
import threading

import conftest
import pytest

from src.features.slack import metadata
from src.infra import event_types as ET
from src.infra import metadata_slots, models, ndjson
from src.runtime import control_events, run_token, runs, task_errors, task_sessions


async def build_three_levels(mgr: task_sessions.TaskTreeManager) -> dict[str, str]:
  """root(m) -> mid(m) -> low(m) -> {worker1(w), worker2(w)}; returns ids by label."""
  root = await conftest.create_task(mgr, parent=None, request_id="root", name="Root")
  mid = await conftest.create_task(mgr, parent=root.id, request_id="mid", name="Mid")
  low = await conftest.create_task(mgr, parent=mid.id, request_id="low", name="Low")
  worker1 = await conftest.create_task(mgr, parent=low.id, request_id="w1", profile="worker", name="W1")
  worker2 = await conftest.create_task(mgr, parent=low.id, request_id="w2", profile="worker", name="W2")
  return {"root": root.id, "mid": mid.id, "low": low.id, "worker1": worker1.id, "worker2": worker2.id}


# ---------------------------------------------------------------------------
# Tree shape
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_three_manager_depths_share_one_profile_and_workers_are_leaves(tmp_path: pathlib.Path) -> None:
  _, session_blocks, mgr = conftest.build_env(tmp_path)
  ids = await build_three_levels(mgr)

  for label in ("root", "mid", "low"):
    meta = await session_blocks.store.get_session(ids[label])
    assert meta is not None and meta.profile == "manager"  # one profile at every manager depth
    assert meta.schema_version == 2

  # A worker is a leaf: no task may be created under it.
  with pytest.raises(task_errors.TaskInvalidError, match="not a manager"):
    await conftest.create_task(mgr, parent=ids["worker1"], profile="worker", request_id="under-worker")


@pytest.mark.asyncio
async def test_history_references_do_not_become_task_parent_edges(tmp_path: pathlib.Path) -> None:
  cfg, session_blocks, mgr = conftest.build_env(tmp_path)
  parent = await conftest.create_root_session(
      session_blocks, models.CreateSessionRequest(name="Parent history copy"), backend=conftest.OPUS_BACKEND_ID)
  parent.parent_session_id = "some-old-session"
  parent.origin_ref = models.EventRef(session_id="some-old-session", event_id=None)
  await session_blocks.store.save_metadata(parent)

  index = await mgr._get_index()
  assert parent.id in index.children.get(None, [])
  # The child create writes nothing to its parent (metadata.json stays byte-
  # identical), and the task-parent relation is the only tree edge.
  meta_path = cfg.sessions_dir / parent.id / "metadata.json"
  before = meta_path.read_bytes()
  child = await conftest.create_task(mgr, parent=parent.id, request_id="child-of-parent")
  assert meta_path.read_bytes() == before
  fresh = await session_blocks.store.get_session(parent.id)
  assert fresh is not None and fresh.task_parent_id is None
  assert fresh.profile == "manager" and fresh.schema_version == 2
  assert child.task_parent_id == parent.id
  index = await mgr._get_index()
  assert parent.id in index.children.get(None, [])
  assert child.id in mgr._children_of(index, parent.id)


# ---------------------------------------------------------------------------
# Derived state, pagination, deletion
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_permanent_delete_blockers(tmp_path: pathlib.Path) -> None:
  cfg, _, mgr = conftest.build_env(tmp_path)
  ids = await build_three_levels(mgr)

  with pytest.raises(task_errors.TaskConflictError, match="child task"):
    await mgr.delete_permanently(ids["low"], caller=conftest.OPERATOR)

  await mgr.runs.register_run_locked(models.RunRecord(id="run-q", session_id=ids["worker1"]))
  with pytest.raises(task_errors.TaskConflictError, match="run record"):
    await mgr.delete_permanently(ids["worker1"], caller=conftest.OPERATOR)

  triggers_dir = cfg.sessions_dir / ids["worker2"] / "triggers"
  triggers_dir.mkdir(parents=True)
  (triggers_dir / "t.json").write_text("{}", encoding="utf-8")
  with pytest.raises(task_errors.TaskConflictError, match="trigger"):
    await mgr.delete_permanently(ids["worker2"], caller=conftest.OPERATOR)

  leaf = await conftest.create_task(mgr, parent=ids["root"], request_id="leafy", profile="worker", name="Leafy")
  assert await mgr.delete_permanently(leaf.id, caller=conftest.OPERATOR) is True


@pytest.mark.asyncio
async def test_only_the_operator_and_the_server_may_name_a_new_task_id(tmp_path: pathlib.Path) -> None:
  _, session_blocks, mgr = conftest.build_env(tmp_path)
  root = await conftest.create_task(mgr, parent=None, request_id="root", name="Root")
  agent = run_token.CallerIdentity(
      kind="agent", claims=run_token.RunTokenClaims(session_id=root.id, run_id="run-1", agent="manager"))
  create = {"task_parent_id": root.id, "profile": "manager", "task": None, "name": "child", "backend": None}

  # The agent's own-child create stays legal; naming its id is the scope refusal.
  with pytest.raises(task_errors.TaskForbiddenError, match=task_sessions.AGENT_CREATE_SCOPE_REFUSAL):
    await mgr.create_task(request_id="by-agent", session_id="agent-picked-id", caller=agent, **create)
  assert await session_blocks.store.get_session("agent-picked-id") is None
  assert (await mgr.create_task(request_id="by-agent", caller=agent, **create)).id != "agent-picked-id"

  origin = metadata.SlackOrigin(team_id="T", channel_id="C", thread_ts="1.1")
  by_server = await mgr.create_task(
      request_id="by-server",
      session_id="server-picked-id",
      slot_values={"slack_origin": origin},
      caller="system",
      **{
          **create, "task_parent_id": None
      })
  by_operator = await mgr.create_task(
      request_id="by-operator", session_id="operator-picked-id", caller=conftest.OPERATOR, **create)
  assert (by_server.id, by_operator.id) == ("server-picked-id", "operator-picked-id")
  # The origin rides the same publish as the metadata: no read sees the node without it.
  assert metadata_slots.fields_of(
      await session_blocks.store.get_session("server-picked-id"), "slack").slack_origin == origin
  # A replay under the explicit id returns the original node instead of a second one.
  replay = await mgr.create_task(
      request_id="by-server", session_id="server-picked-id", caller="system", **{
          **create, "task_parent_id": None
      })
  assert replay.created_by_event == by_server.created_by_event


@pytest.mark.asyncio
async def test_fork_child_of_an_archived_v2_parent_starts_open_with_no_inherited_inputs(tmp_path: pathlib.Path) -> None:
  """The fork copies the parent's control facts into the child's log; the clone_start marker keeps
  them the parent's, so the child's own task_created starts its lifecycle."""
  _, session_blocks, mgr = conftest.build_env(tmp_path)
  parent = await conftest.create_task(mgr, parent=None, request_id="parent", name="Parent")
  await mgr.dispatch.admit_input(parent.id, event_type="user", content="old input", actor="user")
  await mgr.completion.archive_subtree(
      parent.id,
      request_id="archive-test",
      summary="archived by the user",
      result_refs=[],
      run_ids=[],
      caller=conftest.OPERATOR)
  assert mgr.task_state(parent.id) == "archived"

  child = await session_blocks.fork.fork_session(parent.id)

  facts = mgr.facts_of(child.id)
  assert (mgr.task_state(child.id), facts.input_candidates, facts.close_events) == ("open", [], [])
  assert child.task_parent_id is None and child.parent_session_id == parent.id


# ---------------------------------------------------------------------------
# Scheduler bookkeeping
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_scheduled_fire_bookkeeping_keeps_the_sidebar_sort_key(tmp_path: pathlib.Path) -> None:
  """Every registered-slot write shape the scheduler uses writes its
  scheduling fields and leaves updated_at as it was: a frequent cron's node
  keeps its sidebar place, while a listing read shows the new Last status."""
  _, session_blocks, tree = conftest.build_env(tmp_path)
  node = await conftest.create_scheduled_node(tree, name="nightly", backend=conftest.OPUS_BACKEND_ID)
  fired_at = datetime.datetime(2026, 1, 2, 3, 4, 5, tzinfo=datetime.UTC)
  await session_blocks.anchors.update_thinking_state(node.id, fired_at)

  # The five call shapes src/features/cron/scheduler.py fires with, and the metadata
  # fields each must land (the scheduler's cron argument writes
  # last_scheduled_cron): cron change, overlap skip, normal fire, loop noop,
  # handler outcome.
  stamp = fired_at.isoformat()
  cron_call = {"last_scheduled_run": stamp, "last_scheduled_cron": "0 3 * * *"}
  cron_land = {"last_scheduled_run": stamp, "last_scheduled_cron": "0 3 * * *"}
  skip_call = {"last_scheduled_run": stamp, "last_run_status": models.LastRunStatus.SKIPPED}
  noop = {"last_run_status": models.LastRunStatus.SUCCESS}
  handler_failed = {"last_run_status": models.LastRunStatus.FAILED}
  shapes = [
      (cron_call, cron_land),
      (skip_call, skip_call),
      (cron_call, cron_land),
      (noop, noop),
      (handler_failed, handler_failed),
  ]
  for call, landed in shapes:
    meta = await tree.update_slot_fields(node.id, "cron", **call)
    assert meta.updated_at == fired_at
    fresh = await session_blocks.store.get_session(node.id)
    assert fresh is not None and fresh.updated_at == fired_at
    row = next(
        r for r in await session_blocks.listing.list_sessions(status=models.SessionStatus.ACTIVE) if r.id == node.id)
    for name, value in landed.items():
      assert getattr(row, name) == value


# ---------------------------------------------------------------------------
# History-tail task state
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "facts,archived", [
        (["clone_start"], 0),
        (["clone_start", "task_closed"], 0),
        (["clone_start", "task_closed", "task_reopened"], 0),
        (["clone_start", "task_reopened", "task_closed"], 0),
        (["clone_start", "task_imported", "task_closed"], 0),
        (["clone_start", "task_closed"], 1),
        (["clone_start", "task_reopened"], 1),
    ])
async def test_task_state_fast_matches_the_full_fold(tmp_path: pathlib.Path, facts: list[str], archived: int) -> None:
  """Every lifecycle combination gives the tail read the full fold's verdict."""
  cfg, _blocks, tree = conftest.build_env(tmp_path)
  node = await conftest.create_task(tree, parent=None, request_id="root", name="Root")

  events = []
  for i, fact in enumerate(facts):
    kind = {
        "clone_start": ET.CLONE_START,
        "task_closed": ET.TASK_CLOSED,
        "task_reopened": ET.TASK_REOPENED,
        "task_imported": ET.TASK_IMPORTED,
    }[fact]
    extra = {"outcome": "completed"} if fact == "task_closed" else {}
    events.append(
        control_events.build_control_event(
            kind, actor="operator", source_session_id=node.id, request_id=f"r{i}", **extra))
  # The first `archived` facts move into an archived segment file.
  live, older = events[archived:], events[:archived]
  archives = cfg.sessions_dir / node.id / "data" / "archives"
  archives.mkdir(parents=True, exist_ok=True)
  with (archives / "chat_events.2026-W01.jsonl").open("wb") as f:
    for event in older:
      f.write(json.dumps(event).encode() + b"\n")
  for event in live:
    await tree.events.append(node.id, event)
  if older:
    meta = await tree.load_meta(node.id)
    meta.archive_offset = len(older)
    await tree.session_store.save_metadata(meta)
    tree = conftest.build_task_tree(cfg, _blocks)  # a fresh block: cold caches read the archive

  assert tree.task_state_fast(node.id) == tree.task_state(node.id)


@pytest.mark.asyncio
async def test_task_state_tail_without_lifecycle_fact_falls_back_to_full_fold(tmp_path: pathlib.Path) -> None:
  """A tail of non-lifecycle events reads past the tail into the full fold."""
  cfg, _blocks, tree = conftest.build_env(tmp_path)
  node = await conftest.create_task(tree, parent=None, request_id="root", name="Root")
  lines = [
      control_events.build_control_event(
          ET.TASK_CLOSED, actor="operator", source_session_id=node.id, request_id="c", outcome="completed")
  ]
  lines += [
      control_events.build_control_event(ET.AGENT_MESSAGE, actor="operator", source_session_id=node.id, text="x")
      for _ in range(1001)
  ]
  events_path = cfg.sessions_dir / node.id / "data" / "chat_events.jsonl"
  events_path.parent.mkdir(parents=True, exist_ok=True)
  with events_path.open("ab") as f:
    for line in lines:
      f.write(json.dumps(line).encode() + b"\n")

  assert tree.task_state(node.id) == "completed"
  assert tree.task_state_fast(node.id) == "completed"


async def _append_raw_lines(cfg: models.Config, node: models.SessionNode, lines: list[str]) -> None:
  events_path = cfg.sessions_dir / node.id / "data" / "chat_events.jsonl"
  events_path.parent.mkdir(parents=True, exist_ok=True)
  with events_path.open("ab") as f:
    for line in lines:
      f.write(line.encode() + b"\n")


def _closed_fact(node_id: str) -> str:
  event = control_events.build_control_event(
      ET.TASK_CLOSED, actor="operator", source_session_id=node_id, request_id="c", outcome="completed")
  return json.dumps(event)


@pytest.mark.asyncio
async def test_task_state_fast_skips_tail_line_without_type_key(tmp_path: pathlib.Path) -> None:
  """A newest line with no `type` key does not break or decide the tail read."""
  cfg, _blocks, tree = conftest.build_env(tmp_path)
  node = await conftest.create_task(tree, parent=None, request_id="root", name="Root")
  await _append_raw_lines(cfg, node, [_closed_fact(node.id), json.dumps({"is_error": False})])

  assert tree.task_state_fast(node.id) == tree.task_state(node.id) == "completed"


@pytest.mark.asyncio
async def test_task_state_fast_skips_tail_line_with_foreign_leading_key(tmp_path: pathlib.Path) -> None:
  """A newest line that leads with a foreign key and carries a non-lifecycle type is skipped."""
  cfg, _blocks, tree = conftest.build_env(tmp_path)
  node = await conftest.create_task(tree, parent=None, request_id="root", name="Root")
  foreign = json.dumps({"session_id": node.id, "type": ET.AGENT_MESSAGE})
  await _append_raw_lines(cfg, node, [_closed_fact(node.id), foreign])

  assert tree.task_state_fast(node.id) == tree.task_state(node.id) == "completed"


@pytest.mark.asyncio
async def test_task_state_fast_without_lifecycle_fact_and_headless_lines_matches_full_fold(
    tmp_path: pathlib.Path) -> None:
  """No lifecycle fact and a `type`-less line: the tail read matches the full fold."""
  cfg, _blocks, tree = conftest.build_env(tmp_path)
  node = await conftest.create_task(tree, parent=None, request_id="root", name="Root")
  await _append_raw_lines(cfg, node, [json.dumps({"is_error": False})])

  assert tree.task_state_fast(node.id) == tree.task_state(node.id)


def _forget_cached_history(tree: task_sessions.TaskTreeManager, session_id: str) -> None:
  """Drop the events cache entry a setup step may have left, so a read's cache effect shows."""
  tree.session_events.chat_events.events_cache.pop(session_id, None)


@pytest.mark.asyncio
async def test_tail_without_lifecycle_fact_reads_the_deep_fact_without_caching_history(tmp_path: pathlib.Path) -> None:
  """A closing fact 1001+ lines above the end is found by the walk, and the history is never cached."""
  cfg, _blocks, tree = conftest.build_env(tmp_path)
  node = await conftest.create_task(tree, parent=None, request_id="root", name="Root")
  closed = json.dumps(
      control_events.build_control_event(
          ET.TASK_CLOSED, actor="operator", source_session_id=node.id, request_id="c", outcome="completed"))
  filler = json.dumps(
      control_events.build_control_event(ET.AGENT_MESSAGE, actor="operator", source_session_id=node.id, text="x"))
  await _append_raw_lines(cfg, node, [closed, *[filler] * 1001])
  _forget_cached_history(tree, node.id)

  assert tree.task_state_fast(node.id) == "completed"
  assert node.id not in tree.session_events.chat_events.events_cache
  assert tree.task_state(node.id) == "completed"  # the full fold agrees


@pytest.mark.asyncio
async def test_tail_without_lifecycle_fact_reads_an_archived_fact_without_caching_history(
    tmp_path: pathlib.Path) -> None:
  """A closing fact that rotated into an archive segment is found by the walk, and the history is never cached."""
  cfg, blocks, tree = conftest.build_env(tmp_path)
  node = await conftest.create_task(tree, parent=None, request_id="root", name="Root")
  archives = cfg.sessions_dir / node.id / "data" / "archives"
  archives.mkdir(parents=True, exist_ok=True)
  closed = control_events.build_control_event(
      ET.TASK_CLOSED, actor="operator", source_session_id=node.id, request_id="c", outcome="completed")
  (archives / "chat_events.2026-W01.jsonl").write_text(json.dumps(closed) + "\n", encoding="utf-8")
  live_filler = json.dumps(
      control_events.build_control_event(ET.AGENT_MESSAGE, actor="operator", source_session_id=node.id, text="x"))
  await _append_raw_lines(cfg, node, [live_filler] * 3)
  meta = await tree.load_meta(node.id)
  meta.archive_offset = 1
  await tree.session_store.save_metadata(meta)
  tree = conftest.build_task_tree(cfg, blocks)  # a fresh block: nothing cached, the archive is the only source
  _forget_cached_history(tree, node.id)

  assert tree.task_state_fast(node.id) == "completed"
  assert node.id not in tree.session_events.chat_events.events_cache
  assert tree.task_state(node.id) == "completed"


async def _queued_worker(tree: task_sessions.TaskTreeManager, tmp_path: pathlib.Path) -> str:
  root = await conftest.create_task(tree, parent=None, request_id="root", name="Root")
  worker = await conftest.create_task(tree, parent=root.id, request_id="w1", profile="worker", name="W1")
  return worker.id


@pytest.mark.asyncio
async def test_queued_run_with_a_stop_request_is_idle_and_caches_no_history(tmp_path: pathlib.Path) -> None:
  _cfg, _blocks, tree = conftest.build_env(tmp_path)
  worker = await _queued_worker(tree, tmp_path)
  await tree.runs.register_run_locked(models.RunRecord(id="r-stop", session_id=worker))
  assert tree.activity_of(worker).work_state == "waiting"

  await tree.runs.request_stop(worker, "r-stop", "stop-1")
  _forget_cached_history(tree, worker)
  assert tree.activity_of(worker).work_state == "idle"
  assert worker not in tree.session_events.chat_events.events_cache


@pytest.mark.asyncio
async def test_launched_run_with_a_live_pid_and_no_terminal_fact_is_running_and_caches_no_history(
    tmp_path: pathlib.Path) -> None:
  _cfg, _blocks, tree = conftest.build_env(tmp_path)
  worker = await _queued_worker(tree, tmp_path)
  await tree.runs.register_run_locked(models.RunRecord(id="r-live", session_id=worker))
  pid_start, _state = runs.read_pid_stat(os.getpid())
  await tree.runs.record_launch(worker, "r-live", pid=os.getpid(), pid_start=pid_start)
  _forget_cached_history(tree, worker)

  activity = tree.activity_of(worker)
  assert (activity.has_running_tasks, activity.work_state) == (True, "running")
  assert worker not in tree.session_events.chat_events.events_cache


@pytest.mark.asyncio
async def test_run_with_ended_at_is_settled_without_reading_its_history(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _cfg, _blocks, tree = conftest.build_env(tmp_path)
  worker = await _queued_worker(tree, tmp_path)
  await tree.runs.register_run_locked(models.RunRecord(id="r-done", session_id=worker))
  await tree.runs.record_finish(worker, "r-done", "completed")
  _forget_cached_history(tree, worker)

  def no_history_read(*_args: object, **_kwargs: object) -> None:
    raise AssertionError("a run whose record has ended_at reads no history")

  monkeypatch.setattr(tree.session_events.chat_events, "iter_events_from_end", no_history_read)
  assert tree.activity_of(worker).work_state == "idle"


@pytest.mark.asyncio
@pytest.mark.parametrize("pause_after_stat", [False, True])
async def test_tree_rows_computed_across_an_append_equal_the_rows_before_or_after_it(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, pause_after_stat: bool) -> None:
  """The page's rows come from the worker thread; an append that lands mid-page is seen wholly or not at all.

  The worker's history identity is taken on the thread, and the loop appends a
  closing fact to that worker while the thread is held either just before or
  just after that stat. The page's rows follow the identity the thread took, so
  every row shows the worker wholly before the append (stat first) or wholly
  after it (append first). The parent's row counts the worker's open descendant,
  so a read that mixed the two would show neither page.
  """
  _cfg, _blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root", name="Root")
  mid = await conftest.create_task(tree, parent=root.id, request_id="mid", name="Mid")
  worker = await conftest.create_task(tree, parent=mid.id, request_id="w1", profile="worker", name="W1")
  await tree.runs.register_run_locked(models.RunRecord(id="r-queued", session_id=worker.id))
  for session_id in (root.id, mid.id, worker.id):
    _forget_cached_history(tree, session_id)
  # The index is built before the pause: a rebuild would read the worker on the wrong thread.
  monkeypatch.setattr(task_sessions, "_TREE_INDEX_TTL_SECONDS", 3600.0)
  await tree.tree_page(parent_id=None, include_archived=True, limit=100, cursor=None)
  before = (await tree.tree_page(parent_id=root.id, include_archived=True, limit=100, cursor=None))["items"]
  assert before[0]["open_descendant_count"] == 1

  paused = threading.Event()
  resume = threading.Event()
  original_identity = tree._history_identity

  def paused_identity(session_id: str) -> tuple[int, int, int] | None:
    if session_id != worker.id or paused.is_set():
      return original_identity(session_id)
    if pause_after_stat:
      identity = original_identity(session_id)
      paused.set()
      resume.wait(timeout=10)
      return identity
    paused.set()
    resume.wait(timeout=10)
    return original_identity(session_id)

  monkeypatch.setattr(tree, "_history_identity", paused_identity)
  request = asyncio.ensure_future(tree.tree_page(parent_id=root.id, include_archived=True, limit=100, cursor=None))
  assert await asyncio.to_thread(paused.wait, 10)
  closed = control_events.build_control_event(
      ET.TASK_CLOSED, actor="operator", source_session_id=worker.id, request_id="c", outcome="completed")
  await tree.events.append(worker.id, closed)
  resume.set()
  during = (await request)["items"]
  monkeypatch.setattr(tree, "_history_identity", original_identity)

  after = (await tree.tree_page(parent_id=root.id, include_archived=True, limit=100, cursor=None))["items"]
  assert after[0]["open_descendant_count"] == 0
  assert during == (before if pause_after_stat else after)
  # A later serial read agrees with the memos the page left behind.
  assert tree.task_state_fast(worker.id) == "completed"
  assert tree.activity_of(worker.id).work_state == "idle"
  for session_id in (root.id, mid.id, worker.id):
    assert session_id not in tree.session_events.chat_events.events_cache


@pytest.mark.asyncio
@pytest.mark.parametrize("parse_reads_append", [True, False])
async def test_cold_events_fill_on_a_thread_keeps_an_append_exactly_once(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, parse_reads_append: bool) -> None:
  """A cold fill on a worker thread and one append race; the events cache then equals the file's events.

  With parse_reads_append the parse reads the appended line, and the fill installs before the append checks
  the cache. Without it the parse reads before the write, and the fill installs after that check. Either way
  the appended event is in the cache once, and the cache equals what the file holds.
  """
  _cfg, _blocks, tree = conftest.build_env(tmp_path)
  root = await conftest.create_task(tree, parent=None, request_id="root", name="Root")
  store = tree.session_events.chat_events
  store.clear_cache(root.id)
  path = store.get_chat_events_path(root.id)
  real_parse = ndjson.parse_ndjson_file
  real_append = ndjson.append_ndjson
  appended = threading.Event()
  parsed = threading.Event()
  installed = threading.Event()
  checked = threading.Event()

  def parse(parse_path: pathlib.Path) -> list[dict]:
    if parse_path != path:
      return real_parse(parse_path)
    if parse_reads_append:
      assert appended.wait(10)
      return real_parse(parse_path)
    events = real_parse(parse_path)
    parsed.set()
    assert checked.wait(10)
    return events

  async def append(append_path: pathlib.Path, data: dict) -> None:
    await real_append(append_path, data)
    if append_path != path:
      return
    appended.set()
    if parse_reads_append:
      assert await asyncio.to_thread(installed.wait, 10)

  monkeypatch.setattr(ndjson, "parse_ndjson_file", parse)
  monkeypatch.setattr(ndjson, "append_ndjson", append)
  event = {"type": "pressure_probe", "content": "appended during the fill"}
  fill = asyncio.ensure_future(asyncio.to_thread(store.load_chat_events_sync, root.id))
  if parse_reads_append:
    save = asyncio.ensure_future(store.save_chat_event(root.id, event))
    loaded = await fill
    installed.set()
    await save
  else:
    assert await asyncio.to_thread(parsed.wait, 10)
    await store.save_chat_event(root.id, event)
    checked.set()
    loaded = await fill

  assert loaded is store.events_cache[root.id]
  assert loaded == real_parse(path)
  assert [e["id"] for e in loaded].count(event["id"]) == 1
