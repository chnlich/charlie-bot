"""Run a deterministic task-dispatch timeline on the Python baseline."""

from __future__ import annotations

import asyncio
import json
import pathlib
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tests"))

import conftest
from src.infra import models
from src.runtime import control_events, task_execution


class ScriptedExecutor:

  def __init__(self, tree: object, outcomes: list[str]) -> None:
    self.tree = tree
    self.outcomes = outcomes
    self.run_index = 0
    self.outcome_index = 0

  async def __call__(self, session_id: str, pending: list[dict], launch_run_id: str | None = None) -> str:
    tree = self.tree
    if launch_run_id is None:
      self.run_index += 1
      run_id = f"run-{self.run_index}"
      await tree.runs.register_run_locked(models.RunRecord(id=run_id, session_id=session_id, kind="work"))
    else:
      run_id = launch_run_id
    batch_ids = [str(event["id"]) for event in pending]
    async with tree.control_lock:
      claimed = await tree.dispatch.claim_input_batch_locked(session_id, run_id, input_ids=batch_ids)
    assert claimed == batch_ids
    outcome = self.outcomes[self.outcome_index]
    self.outcome_index += 1
    await tree.dispatch.finish_run(session_id, run_id, outcome=outcome)
    return run_id


async def run() -> dict:
  with tempfile.TemporaryDirectory(prefix="charliebot-python-dispatch-") as temporary:
    cfg, blocks, tree = conftest.build_env(pathlib.Path(temporary))
    task_execution._task_manager = tree
    completion = tree.completion

    async def scripted_after_run_finished(_session_id: str, _run_id: str) -> None:
      return None

    completion.after_run_finished = scripted_after_run_finished
    executor = ScriptedExecutor(tree, ["success", "success", "failed", "success", "success", "success", "success"])
    tree.dispatch.executor = executor

    root = await conftest.create_task(tree, parent=None, request_id="root", profile="manager", name="Root")
    worker = await conftest.create_task(tree, parent=root.id, request_id="worker", profile="worker", name="Worker")

    await tree.dispatch.admit_input(
        root.id, event_type="user", content="root message", actor="user", input_id="root-input")
    await tree.dispatch.admit_input(
        root.id, event_type="user", content="changed replay", actor="user", input_id="root-input")
    await tree.dispatch.admit_input(root.id, event_type="scheduled_trigger", content="cron fire", actor="system")
    await tree.dispatch.admit_input(
        worker.id,
        event_type="agent_message",
        content="parent instruction",
        actor="agent",
        from_session=root.id,
        from_session_name="Root",
    )
    try:
      await tree.dispatch.admit_input(
          worker.id,
          event_type="agent_message",
          content="outsider instruction",
          actor="agent",
          from_session="outsider",
      )
    except Exception as error:
      from src.runtime import task_errors
      assert isinstance(error, task_errors.TaskForbiddenError)
    else:
      raise AssertionError("an agent message from another sender was admitted")

    close = control_events.build_control_event(
        "task_closed",
        actor="agent",
        source_session_id=worker.id,
        event_id=control_events.stable_close_event_id(worker.id, "worker-close"),
        request_id="worker-close",
        outcome="completed",
        summary="synthetic close",
        result_refs=[],
        run_ids=[],
        report_to=root.id,
    )
    await tree.events.append(worker.id, close)
    await tree.dispatch.admit_input(
        worker.id, event_type="user", content="restore worker", actor="user", input_id="worker-input")

    await tree.dispatch.dispatch_pending(root.id)
    await tree.dispatch.dispatch_pending(worker.id)

    saved_executor = tree.dispatch.executor
    tree.dispatch.executor = None
    await tree.dispatch.deliver_child_report(
        worker.id,
        source_event=close,
        outcome="completed",
        summary="synthetic close",
        result_refs=[],
        recipient=root.id,
    )
    tree.dispatch.executor = saved_executor
    lost = await conftest.create_task(tree, parent=root.id, request_id="lost", profile="worker", name="Lost")
    lost_close = control_events.build_control_event(
        "task_closed",
        actor="agent",
        source_session_id=lost.id,
        event_id=control_events.stable_close_event_id(lost.id, "lost-close"),
        request_id="lost-close",
        outcome="completed",
        summary="synthetic close",
        result_refs=[],
        run_ids=[],
        report_to=root.id,
    )
    await tree.events.append(lost.id, lost_close)

    await tree.dispatch.admit_input(
        root.id, event_type="user", content="failed round", actor="user", input_id="failed-input")
    await tree.dispatch.dispatch_pending(root.id)

    await tree.dispatch.admit_input(
        root.id, event_type="user", content="foreign batch", actor="user", input_id="foreign-input")
    await tree.runs.register_run_locked(models.RunRecord(id="run-owner", session_id=root.id, kind="work"))
    async with tree.control_lock:
      await tree.dispatch.claim_input_batch_locked(root.id, "run-owner")
    await tree.runs.register_run_locked(models.RunRecord(id="run-foreign", session_id=root.id, kind="work"))
    try:
      await tree.dispatch.finish_run(root.id, "run-foreign", outcome="success", input_event_ids=["foreign-input"])
    except Exception as error:
      from src.runtime import task_errors
      assert isinstance(error, task_errors.TaskConflictError)
    else:
      raise AssertionError("a batchless run acknowledged another run's input")
    await tree.dispatch.finish_run(root.id, "run-owner", outcome="success")

    await tree.dispatch.admit_input(
        root.id, event_type="user", content="stopped queue", actor="user", input_id="stopped-input")
    await tree.runs.register_run_locked(models.RunRecord(id="run-stopped", session_id=root.id, kind="work"))
    async with tree.control_lock:
      await tree.dispatch.claim_input_batch_locked(root.id, "run-stopped")
    await tree.runs.request_stop(root.id, "run-stopped", "stop-stopped")
    await tree.dispatch.dispatch_pending(root.id)

    await tree.dispatch.admit_input(
        root.id, event_type="user", content="report followup", actor="user", input_id="report-followup")
    assert tree.dispatch.undelivered_report_blockers(lost.id)
    assert len(await tree.dispatch.recover_pending_reports(lost.id)) == 1
    assert await tree.dispatch.recover_pending_reports(lost.id) == []

    successor = await conftest.create_task(
        tree, parent=None, request_id="successor", profile="manager", name="Successor")
    assert await blocks.store.get_session(successor.id) is not None
    assert await tree.load_task_meta(successor.id) is not None
    root_meta = await blocks.store.get_session(root.id)
    assert root_meta is not None
    root_meta.successor_session_id = successor.id
    await blocks.store.save_metadata(root_meta)
    assert await blocks.store.get_session(successor.id) is not None
    assert await tree.load_task_meta(successor.id) is not None
    from src.runtime import master_trigger
    await master_trigger.trigger_master(
        root.id,
        "wake successor",
        blocks.store,
        blocks.successor,
        blocks.lifecycle,
        input_id="successor-input",
    )

    archived = await conftest.create_task(tree, parent=None, request_id="archived", profile="manager", name="Archived")
    await blocks.lifecycle.archive_session(archived.id)
    await master_trigger.trigger_master(
        archived.id,
        "pull back",
        blocks.store,
        blocks.successor,
        blocks.lifecycle,
        input_id="archived-input",
    )

    outputs = []
    for label, session_id in (
        ("root", root.id),
        ("worker", worker.id),
        ("lost", lost.id),
        ("successor", successor.id),
        ("archived", archived.id),
    ):
      events_path = cfg.sessions_dir / session_id / "data" / "chat_events.jsonl"
      events = events_path.read_text(encoding="utf-8").splitlines()
      runs = [
          json.loads(tree.runs.metadata_path(session_id, run.id).read_text(encoding="utf-8"))
          for run in tree.runs.list_run_records_sync(session_id)
      ]
      outputs.append({"node": label, "events": events, "runs": runs})
    return {"nodes": outputs}


print(json.dumps(asyncio.run(run()), separators=(",", ":")))
