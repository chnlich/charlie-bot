"""Run-owner tests: records, terminal facts, stop/finish races, identity, recovery."""

from __future__ import annotations

import json
import os
import subprocess
from datetime import UTC, datetime
from pathlib import Path

import pytest
from conftest import (
    RUNS_STOP_EXIT_WAIT_SECONDS_PATCH_TARGET,
    identity_of,
    live_subprocess,
)
from conftest import build_env as build_task_tree_env

from src.core import event_types as ET
from src.core import runs
from src.core.chat_events import chat_events_path
from src.core.models import RunRecord, utc_now_iso
from src.core.runs import (
    RUN_IDENTITY_UNKNOWN_DETAIL,
    RunIdentityConflictError,
    RunStore,
    run_identity_refusal,
)
from src.core.session_aliases import SessionAliasStore
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager


def build_env(tmp_path: Path) -> tuple[object, SessionManager, TaskTreeManager, RunStore]:
  cfg, session_mgr, mgr = build_task_tree_env(tmp_path)
  return cfg, session_mgr, mgr, mgr.runs


async def make_task(store_run_env: tuple, request_id: str) -> str:
  _, _, mgr, _ = store_run_env
  task = await mgr.create_task(
      request_id=request_id,
      task_parent_id=None,
      profile="worker",
      task=None,
      name=None,
      backend=None,
      caller="operator")
  return task.id


async def register_live_run(store: RunStore, session_id: str, proc: subprocess.Popen) -> RunRecord:
  pid, pid_start = identity_of(proc.pid)
  return await store.register_run(
      RunRecord(id="run-live", session_id=session_id, pid=pid, pid_start=pid_start, started_at=datetime.now(UTC)))


# ---------------------------------------------------------------------------
# Records and retries
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_register_is_idempotent_and_registers_alias(tmp_path: Path) -> None:
  env = build_env(tmp_path)
  _, session_mgr, mgr, store = env
  session_id = await make_task(env, "t1")

  run = await store.register_run(RunRecord(id="r1", session_id=session_id, kind="work"))
  again = await store.register_run(RunRecord(id="r1", session_id=session_id, kind="review"))
  assert again.id == run.id and again.kind == "work"  # the original product wins

  assert (await store.get_run(session_id, "r1")) is not None
  assert mgr.aliases.resolve_thread(session_id, "r1") == {"session_id": session_id, "run_id": "r1"}
  # No second ThreadMetadata exists for the alias.
  assert not (session_mgr._cfg.sessions_dir / session_id / "threads" / "r1").exists()

  # A queued run is distinguishable from a live process and keeps its inputs for dispatch.
  assert store.run_blocker(
      run, store.load_events_sync(session_id),
      runs.read_host_boot_time()) == f"run {run.id} is queued (pending dispatch)"

  # A durable stop request settles the never-launched run: no blocker, still
  # no terminal fact.
  stop = await store.request_stop(session_id, "r1", "stop-1")
  assert stop.stop_requested is True and stop.outcome is None
  assert store.run_blocker(run, store.load_events_sync(session_id), runs.read_host_boot_time()) is None
  assert store.terminal_outcome(store.load_events_sync(session_id), "r1") is None


@pytest.mark.asyncio
async def test_retry_binding_is_stable_across_requests_and_reload(tmp_path: Path) -> None:
  env = build_env(tmp_path)
  cfg, session_mgr, mgr, store = env
  session_id = await make_task(env, "t1")
  original = await store.register_run(RunRecord(id="r-orig", session_id=session_id, kind="work", backend="opus"))

  # The locked variant is the production entry (the retry route holds the tree
  # owner's control lock); the test holds that same lock to meet its contract.
  async with mgr.control_lock:
    first = await store.create_retry_run_locked(
        session_id, "retry-req", "r-orig", task_spec_text='{"goal":"x"}', backend=original.backend)
    second = await store.create_retry_run_locked(
        session_id, "retry-req", "r-orig", task_spec_text='{"goal":"x"}', backend=original.backend)
  assert first.id == second.id and first.retry_of_run_id == "r-orig"

  fresh_mgr = TaskTreeManager(cfg, session_mgr)
  fresh_store = fresh_mgr.runs
  async with fresh_mgr.control_lock:
    replay = await fresh_store.create_retry_run_locked(
        session_id, "retry-req", "r-orig", task_spec_text='{"goal":"x"}', backend=original.backend)
  assert replay.id == first.id

  # The pinned spec body and its hash survive on the record.
  record = await store.get_run(session_id, first.id)
  assert record is not None and record.task_spec_hash is not None
  assert Path(record.task_spec_ref).read_text(encoding="utf-8") == '{"goal":"x"}'
  import hashlib
  assert record.task_spec_hash == hashlib.sha256(b'{"goal":"x"}').hexdigest()

  # The original run's evidence is untouched.
  assert (await store.get_run(session_id, "r-orig")) is not None


# ---------------------------------------------------------------------------
# Terminal facts
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_record_finish_is_the_one_terminal_writer(tmp_path: Path) -> None:
  env = build_env(tmp_path)
  _, _, _, store = env
  session_id = await make_task(env, "t1")
  await store.register_run(RunRecord(id="r1", session_id=session_id))
  # An unclaimed run's finisher names real input events of this session only:
  # the acknowledgement payload is identity-bound, never arbitrary strings.
  await store._events.append(
      session_id, {
          "id": "e1",
          "type": ET.USER,
          "timestamp": utc_now_iso(),
          "content": "real input event"
      })
  await store.record_finish(session_id, "r1", "success", input_event_ids=["e1"], exit_code=0)
  events = store.load_events_sync(session_id)
  finished = [e for e in events if e["type"] == ET.RUN_FINISHED]
  assert len(finished) == 1 and finished[0]["outcome"] == "success"
  assert finished[0]["input_event_ids"] == ["e1"]

  record = await store.get_run(session_id, "r1")
  assert record is not None and record.ended_at is not None and record.exit_code == 0

  # A second finish never overwrites the first fact.
  await store.record_finish(session_id, "r1", "failed", input_event_ids=["e2"], exit_code=2)
  events = store.load_events_sync(session_id)
  assert len([e for e in events if e["type"] == ET.RUN_FINISHED]) == 1
  still = await store.get_run(session_id, "r1")
  assert still is not None and still.exit_code == 0 and still.input_event_ids == ["e1"]


# ---------------------------------------------------------------------------
# Stop requests
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_stop_signals_owned_process_and_records_interrupted_after_actual_exit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  env = build_env(tmp_path)
  _, _, _, store = env
  session_id = await make_task(env, "t1")
  proc = live_subprocess()
  try:
    await register_live_run(store, session_id, proc)
    result = await store.request_stop(session_id, "run-live", "stop-1")
    assert result.stop_requested is True
    assert result.outcome == "interrupted"
    assert proc.poll() is not None  # actual exit observed before the fact landed

    events = store.load_events_sync(session_id)
    stops = [e for e in events if e["type"] == ET.RUN_STOP_REQUESTED]
    assert len(stops) == 1 and stops[0]["request_id"] == "stop-1"
    finished = [e for e in events if e["type"] == ET.RUN_FINISHED]
    assert len(finished) == 1 and finished[0]["outcome"] == "interrupted"

    # A duplicate stop returns the same result without a second request fact.
    dup = await store.request_stop(session_id, "run-live", "stop-2")
    assert dup.outcome == result.outcome and dup.stop_requested is False
    events = store.load_events_sync(session_id)
    assert len([e for e in events if e["type"] == ET.RUN_STOP_REQUESTED]) == 1
  finally:
    proc.kill()


@pytest.mark.asyncio
async def test_naturally_completed_run_retains_outcome_against_a_late_stop(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  env = build_env(tmp_path)
  _, _, _, store = env
  session_id = await make_task(env, "t1")
  proc = live_subprocess()
  try:
    await register_live_run(store, session_id, proc)
    # Natural finish lands first.
    await store.record_finish(session_id, "run-live", "success", exit_code=0)

    result = await store.request_stop(session_id, "run-live", "late-stop")
    assert result.stop_requested is False and result.outcome == "success"
    assert proc.poll() is None  # no signal was ever sent
    events = store.load_events_sync(session_id)
    assert not [e for e in events if e["type"] == ET.RUN_STOP_REQUESTED]
  finally:
    proc.kill()


@pytest.mark.asyncio
async def test_identity_mismatch_returns_conflict_and_keeps_evidence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr(RUNS_STOP_EXIT_WAIT_SECONDS_PATCH_TARGET, 0.2)
  env = build_env(tmp_path)
  _, _, _, store = env
  session_id = await make_task(env, "t1")
  # A live pid with a forged pid_start (pid reuse cannot fake field 22).
  await store.register_run(
      RunRecord(id="run-forged", session_id=session_id, pid=os.getpid(), pid_start="1", started_at=datetime.now(UTC)))
  with pytest.raises(RunIdentityConflictError, match="identity mismatch"):
    await store.request_stop(session_id, "run-forged", "stop-1")

  # The durable stop request stays as pending evidence; nothing signalled this process.
  events = store.load_events_sync(session_id)
  assert store.stop_requested(events, "run-forged")
  assert not [e for e in events if e["type"] == ET.RUN_FINISHED]


# ---------------------------------------------------------------------------
# Read-only lockless store (the CLI's run-token resolution)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_readonly_store_reads_without_a_control_lock(tmp_path: Path) -> None:
  sessions_dir = tmp_path / "sessions"
  sessions_dir.mkdir()
  store = RunStore(sessions_dir, None, None, SessionAliasStore(sessions_dir))
  session_id, run_id = "sess-ro", "run-ro"
  record = RunRecord(id=run_id, session_id=session_id)
  path = store.metadata_path(session_id, run_id)
  path.parent.mkdir(parents=True)
  path.write_text(record.model_dump_json(), encoding="utf-8")

  assert store.read_run_sync(session_id, run_id).id == run_id

  # The identity predicate scans the live chat log the read-only store parses.
  events_path = chat_events_path(sessions_dir / session_id)
  events_path.parent.mkdir(parents=True, exist_ok=True)
  events_path.write_text(
      json.dumps({
          "type": ET.RUN_FINISHED,
          "run_id": run_id,
          "outcome": "success",
      }) + "\n", encoding="utf-8")
  refusal = run_identity_refusal(store.read_run_sync(session_id, run_id), store.load_events_sync(session_id))
  assert refusal == RUN_IDENTITY_UNKNOWN_DETAIL

  # The lockless store is read-only by contract: a write path fails loud.
  with pytest.raises(TypeError):
    await store.register_run(RunRecord(id="r2", session_id=session_id))
