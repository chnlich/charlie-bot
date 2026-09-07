"""The resume_worker finalize liveness gate.

Before resume_worker records a FAILED outcome on ANY exception or
cancellation path (asyncio.CancelledError included — a BaseException that
lands in the same finally), it consults the same is_alive probe it was
mounted with:

  - probe true (alive or constant-true unverifiable) -> the failure is ours,
    not the run's: emit one recovery event (resume-exception-alive), keep the
    thread running, skip the FAILED finalize;
  - probe false (death proven) -> the current FAILED finalize runs unchanged.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
from collections.abc import Awaitable, Callable
from pathlib import Path

import pytest
from conftest import REVIEW_TRIGGER_MASTER_PATCH_TARGET
from test_restart_recovery_e2e import _cfg, _recovery_reports

from src.agents.worker import Worker
from src.core import spawner
from src.core.models import CreateSessionRequest, ThreadStatus
from src.core.sessions import SessionManager
from src.core.spawner_lifecycle import RESUME_EXCEPTION_ALIVE_REASON
from src.core.threads import ThreadManager


async def _make_running_thread(home: Path):
  cfg = _cfg(home)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session_meta = await session_mgr.create_session(CreateSessionRequest(name="gate"))
  thread = await thread_mgr.create_thread(session_meta, "gate task")
  thread.status = ThreadStatus.RUNNING
  await thread_mgr.save_metadata(thread)
  return cfg, session_mgr, thread_mgr, session_meta, thread


async def _boom_resume(self, *, is_alive, on_silence=None) -> int:
  raise RuntimeError("resume exploded")


def _hang_resume(entered: asyncio.Event) -> Callable[..., Awaitable[int]]:
  """A Worker.resume stand-in that signals entry through ``entered``, then hangs."""

  async def hang(self, *, is_alive, on_silence=None) -> int:
    entered.set()
    await asyncio.Event().wait()  # never returns; only cancellation gets out
    return -1

  return hang


def _thread_status(home: Path, session_id: str, thread_id: str) -> str:
  meta_path = home / "sessions" / session_id / "threads" / thread_id / "metadata.json"
  return json.loads(meta_path.read_text(encoding="utf-8"))["status"]


@pytest.fixture
def _no_master_wake(monkeypatch: pytest.MonkeyPatch) -> None:

  async def fake_trigger_master(session_id: str, summary: str, cfg, session_mgr) -> None:
    pass

  monkeypatch.setattr(REVIEW_TRIGGER_MASTER_PATCH_TARGET, fake_trigger_master)


# Rows are the two probe outcomes the gate consults on every exception or
# cancellation path. A live probe means the failure is ours, not the run's:
# one recovery event, thread left running. A dead probe is proven death: the
# FAILED finalize runs unchanged and nothing is reported.
_GATE_ROWS = [
    pytest.param(True, "running", True, id="live-probe"),
    pytest.param(False, "failed", False, id="dead-probe"),
]


@pytest.mark.asyncio
@pytest.mark.usefixtures("_no_master_wake")
@pytest.mark.parametrize("is_alive, expected_status, expect_alive_report", _GATE_ROWS)
async def test_generic_exception_gate_by_probe(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    is_alive: bool,
    expected_status: str,
    expect_alive_report: bool,
) -> None:
  """A generic exception in resume consults the same is_alive probe it was
  mounted with: live -> recovery event, thread left running; dead -> FAILED
  finalize."""
  home = tmp_path / "home"
  cfg, session_mgr, thread_mgr, session_meta, thread = await _make_running_thread(home)

  monkeypatch.setattr(Worker, "resume", _boom_resume)

  await spawner.resume_worker(
      session_meta.id,
      "gate task",
      thread.id,
      cfg,
      session_mgr,
      thread_mgr,
      is_alive=lambda: is_alive,
      interrupt_reason="",
      on_silence=None)

  assert _thread_status(home, session_meta.id, thread.id) == expected_status
  reports = _recovery_reports(home, session_meta.id)
  if expect_alive_report:
    assert len(reports) == 1
    assert RESUME_EXCEPTION_ALIVE_REASON in reports[0]["content"]
  else:
    assert reports == []


@pytest.mark.asyncio
@pytest.mark.usefixtures("_no_master_wake")
@pytest.mark.parametrize("is_alive, expected_status, expect_alive_report", _GATE_ROWS)
async def test_cancellation_gate_by_probe(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    is_alive: bool,
    expected_status: str,
    expect_alive_report: bool,
) -> None:
  """asyncio.CancelledError bypasses ``except Exception`` and still hits the
  same gate: live -> recovery event, thread left running; dead -> FAILED
  finalize while the cancellation still propagates."""
  home = tmp_path / "home"
  cfg, session_mgr, thread_mgr, session_meta, thread = await _make_running_thread(home)
  resume_entered = asyncio.Event()

  monkeypatch.setattr(Worker, "resume", _hang_resume(resume_entered))

  task = asyncio.create_task(
      spawner.resume_worker(
          session_meta.id,
          "gate task",
          thread.id,
          cfg,
          session_mgr,
          thread_mgr,
          is_alive=lambda: is_alive,
          interrupt_reason="",
          on_silence=None))
  await asyncio.wait_for(resume_entered.wait(), timeout=10.0)
  task.cancel()
  with contextlib.suppress(asyncio.CancelledError):
    await task
  assert task.cancelled()

  assert _thread_status(home, session_meta.id, thread.id) == expected_status
  reports = _recovery_reports(home, session_meta.id)
  if expect_alive_report:
    assert len(reports) == 1
    assert RESUME_EXCEPTION_ALIVE_REASON in reports[0]["content"]
  else:
    assert reports == []
