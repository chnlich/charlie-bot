"""Run-owner tests: records, terminal facts, stop/finish races, identity, recovery."""

from __future__ import annotations

import os
import subprocess
import time
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import make_home_config

from src.core import event_types as ET
from src.core.models import RunRecord
from src.core.runs import (
  RunIdentityConflictError,
  RunStore,
  read_pid_stat,
)
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager


def build_env(tmp_path: Path) -> tuple[object, SessionManager, TaskTreeManager, RunStore]:
  cfg = make_home_config(tmp_path)
  session_mgr = SessionManager(cfg)
  mgr = TaskTreeManager(cfg, session_mgr)
  return cfg, session_mgr, mgr, mgr.runs


async def make_task(store_run_env: tuple, request_id: str) -> str:
  _, _, mgr, _ = store_run_env
  task = await mgr.create_task(
      request_id=request_id, task_parent_id=None, profile="worker", task=None, name=None,
      backend=None, caller="operator")
  return task.id


def live_subprocess() -> subprocess.Popen:
  """An owned, isolated sleeper: the only process identity any test here signals."""
  return subprocess.Popen(["/bin/sleep", "30"])


def identity_of(pid: int) -> tuple[int, str]:
  pair = read_pid_stat(pid)
  assert pair is not None
  return pid, pair[0]


async def register_live_run(store: RunStore, session_id: str, proc: subprocess.Popen) -> RunRecord:
  pid, pid_start = identity_of(proc.pid)
  return await store.register_run(
      RunRecord(id="run-live", session_id=session_id, pid=pid, pid_start=pid_start,
                started_at=datetime.now(UTC)))


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
  assert store.run_is_queued(run, store.load_events_sync(session_id))


@pytest.mark.asyncio
async def test_retry_binding_is_stable_across_requests_and_reload(tmp_path: Path) -> None:
  env = build_env(tmp_path)
  cfg, session_mgr, _mgr, store = env
  session_id = await make_task(env, "t1")
  original = await store.register_run(
      RunRecord(id="r-orig", session_id=session_id, kind="work", backend="opus"))

  first = await store.create_retry_run(
      session_id, "retry-req", "r-orig", task_spec_text='{"goal":"x"}', backend=original.backend)
  second = await store.create_retry_run(
      session_id, "retry-req", "r-orig", task_spec_text='{"goal":"x"}', backend=original.backend)
  assert first.id == second.id and first.retry_of_run_id == "r-orig"

  fresh_store = TaskTreeManager(cfg, session_mgr).runs
  replay = await fresh_store.create_retry_run(
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


@pytest.mark.asyncio
async def test_run_pagination_is_a_keyset_over_started_order(tmp_path: Path) -> None:
  env = build_env(tmp_path)
  _, _, _, store = env
  session_id = await make_task(env, "t1")
  base = datetime(2026, 1, 1, tzinfo=UTC)
  for i, started in enumerate([base, base + timedelta(minutes=1), None]):
    await store.register_run(
        RunRecord(id=f"r{i}", session_id=session_id, started_at=started))

  page1 = store.list_runs_page_sync(session_id, limit=2, cursor=None)
  assert [r.id for r in page1.items] == ["r2", "r0"]  # queued (never launched) first
  assert page1.next_cursor is not None
  page2 = store.list_runs_page_sync(session_id, limit=2, cursor=page1.next_cursor)
  assert [r.id for r in page2.items] == ["r1"]
  assert page2.next_cursor is None


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
  await store._events.append(session_id, {
      "id": "e1", "type": ET.USER, "timestamp": datetime.now(UTC).isoformat(),
      "content": "real input event"})
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
async def test_stop_request_can_return_null_outcome_and_recovery_closes_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr("src.core.runs.STOP_EXIT_WAIT_SECONDS", 0.2)
  monkeypatch.setattr("src.core.runs.STOP_EXIT_POLL_SECONDS", 0.02)
  env = build_env(tmp_path)
  cfg, session_mgr, _mgr, store = env
  session_id = await make_task(env, "t1")
  # A process that ignores SIGTERM: the request stays durable, outcome stays null.
  ready = tmp_path / "sigterm_ready"
  proc = subprocess.Popen(
      ["python3", "-c",
       ("import signal, time, sys; "
        "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
        f"open({str(ready)!r}, 'w').close(); "
        "time.sleep(30)")])
  deadline = time.monotonic() + 10
  while not ready.exists():
    assert time.monotonic() < deadline, "helper process never armed its SIGTERM handler"
    time.sleep(0.01)
  try:
    await register_live_run(store, session_id, proc)
    result = await store.request_stop(session_id, "run-live", "stop-1")
    assert result.stop_requested is True and result.outcome is None
    assert proc.poll() is None  # still alive; the request is not a completion assertion

    # A fresh reader (post-restart) recognizes the same durable stop request...
    fresh_store = TaskTreeManager(cfg, session_mgr).runs
    events = fresh_store.load_events_sync(session_id)
    assert fresh_store.stop_requested(events, "run-live")

    # ...and once the actual exit is observed, the interrupted fact lands once.
    proc.kill()
    proc.wait(timeout=10)
    reconciled = await fresh_store.reconcile_stop_request(session_id, "run-live")
    assert reconciled.outcome == "interrupted"
    events = fresh_store.load_events_sync(session_id)
    assert len([e for e in events if e["type"] == ET.RUN_FINISHED]) == 1
  finally:
    if proc.poll() is None:
      proc.kill()


@pytest.mark.asyncio
async def test_identity_mismatch_returns_conflict_and_keeps_evidence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr("src.core.runs.STOP_EXIT_WAIT_SECONDS", 0.2)
  env = build_env(tmp_path)
  _, _, _, store = env
  session_id = await make_task(env, "t1")
  # A live pid with a forged pid_start (pid reuse cannot fake field 22).
  await store.register_run(
      RunRecord(id="run-forged", session_id=session_id, pid=os.getpid(), pid_start="1",
                started_at=datetime.now(UTC)))
  with pytest.raises(RunIdentityConflictError, match="identity mismatch"):
    await store.request_stop(session_id, "run-forged", "stop-1")

  # The durable stop request stays as pending evidence; nothing signalled this process.
  events = store.load_events_sync(session_id)
  assert store.stop_requested(events, "run-forged")
  assert not [e for e in events if e["type"] == ET.RUN_FINISHED]


@pytest.mark.asyncio
async def test_exit_before_stop_observes_interrupted(tmp_path: Path) -> None:
  env = build_env(tmp_path)
  _, _, _, store = env
  session_id = await make_task(env, "t1")
  proc = subprocess.Popen(["/bin/sleep", "0.05"])
  pid, pid_start = identity_of(proc.pid)
  proc.wait(timeout=10)
  await store.register_run(
      RunRecord(id="run-exited", session_id=session_id, pid=pid, pid_start=pid_start,
                started_at=datetime.now(UTC)))
  result = await store.request_stop(session_id, "run-exited", "stop-1")
  assert result.outcome == "interrupted"


@pytest.mark.asyncio
async def test_exit_between_identity_read_and_signal_still_records_interrupted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The stat->kill race: the pid vanishes after the identity read; the observed exit is the fact."""
  env = build_env(tmp_path)
  _, _, _, store = env
  session_id = await make_task(env, "t1")
  proc = live_subprocess()
  try:
    await register_live_run(store, session_id, proc)

    def vanished(pid: int, sig: int) -> None:
      raise ProcessLookupError

    monkeypatch.setattr("src.core.runs.os.kill", vanished)
    result = await store.request_stop(session_id, "run-live", "stop-race")
    assert result.stop_requested is True and result.outcome == "interrupted"
    events = store.load_events_sync(session_id)
    assert len([e for e in events if e["type"] == ET.RUN_FINISHED]) == 1
    assert proc.poll() is None  # the signal never actually left
  finally:
    proc.kill()
