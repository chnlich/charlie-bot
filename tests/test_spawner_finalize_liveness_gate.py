"""The resume_worker finalize liveness gate (plan 4.1 finalize 存活总闸),
acceptance leg (j).

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

from src.agents.worker import Worker
from src.core import spawner
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, CreateSessionRequest, ThreadStatus
from src.core.sessions import SessionManager
from src.core.spawner import RESUME_EXCEPTION_ALIVE_REASON
from src.core.threads import ThreadManager


def _cfg(home: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=home,
      worktree_dir=str(home / "worktrees"),
      backend_options=[BackendOption(id="fake", label="Fake", type="cc-claude", model="fake-model")],
  )


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


def _recovery_reports(home: Path, session_id: str) -> list[dict]:
  chat_path = home / "sessions" / session_id / "data" / "chat_events.jsonl"
  if not chat_path.exists():
    return []
  events = [json.loads(line) for line in chat_path.read_text(encoding="utf-8").splitlines() if line.strip()]
  return [e for e in events if e.get("source") == "crash_recovery"]


@pytest.fixture()
def _no_master_wake(monkeypatch: pytest.MonkeyPatch) -> None:
  async def fake_trigger_master(session_id: str, summary: str, cfg, session_mgr) -> None:
    pass

  monkeypatch.setattr("src.core.review.trigger_master", fake_trigger_master)


@pytest.mark.asyncio
@pytest.mark.usefixtures("_no_master_wake")
async def test_generic_exception_with_live_probe_reports_and_skips_finalize(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Generic exception + probe true -> recovery event, thread left running."""
  home = tmp_path / "home"
  cfg, session_mgr, thread_mgr, session_meta, thread = await _make_running_thread(home)

  monkeypatch.setattr(Worker, "resume", _boom_resume)

  await spawner.resume_worker(
      session_meta.id, "gate task", thread.id, cfg, session_mgr, thread_mgr, is_alive=lambda: True)

  assert _thread_status(home, session_meta.id, thread.id) == "running"
  reports = _recovery_reports(home, session_meta.id)
  assert len(reports) == 1
  assert RESUME_EXCEPTION_ALIVE_REASON in reports[0]["content"]


@pytest.mark.asyncio
@pytest.mark.usefixtures("_no_master_wake")
async def test_generic_exception_with_dead_probe_finalizes_failed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Generic exception + probe false (death proven) -> current FAILED finalize."""
  home = tmp_path / "home"
  cfg, session_mgr, thread_mgr, session_meta, thread = await _make_running_thread(home)

  monkeypatch.setattr(Worker, "resume", _boom_resume)

  await spawner.resume_worker(
      session_meta.id, "gate task", thread.id, cfg, session_mgr, thread_mgr, is_alive=lambda: False)

  assert _thread_status(home, session_meta.id, thread.id) == "failed"
  assert _recovery_reports(home, session_meta.id) == []


@pytest.mark.asyncio
@pytest.mark.usefixtures("_no_master_wake")
async def test_cancellation_with_live_probe_reports_and_skips_finalize(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """asyncio.CancelledError bypasses ``except Exception`` and still hits the
  same gate: probe true -> recovery event, thread left running."""
  home = tmp_path / "home"
  cfg, session_mgr, thread_mgr, session_meta, thread = await _make_running_thread(home)
  resume_entered = asyncio.Event()

  monkeypatch.setattr(Worker, "resume", _hang_resume(resume_entered))

  task = asyncio.create_task(
      spawner.resume_worker(
          session_meta.id, "gate task", thread.id, cfg, session_mgr, thread_mgr, is_alive=lambda: True))
  await asyncio.wait_for(resume_entered.wait(), timeout=10.0)
  task.cancel()
  with contextlib.suppress(asyncio.CancelledError):
    await task
  assert task.cancelled()

  assert _thread_status(home, session_meta.id, thread.id) == "running"
  reports = _recovery_reports(home, session_meta.id)
  assert len(reports) == 1
  assert RESUME_EXCEPTION_ALIVE_REASON in reports[0]["content"]


@pytest.mark.asyncio
@pytest.mark.usefixtures("_no_master_wake")
async def test_cancellation_with_dead_probe_finalizes_failed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """CancelledError + probe false -> current FAILED finalize (and the
  cancellation still propagates)."""
  home = tmp_path / "home"
  cfg, session_mgr, thread_mgr, session_meta, thread = await _make_running_thread(home)
  resume_entered = asyncio.Event()

  monkeypatch.setattr(Worker, "resume", _hang_resume(resume_entered))

  task = asyncio.create_task(
      spawner.resume_worker(
          session_meta.id, "gate task", thread.id, cfg, session_mgr, thread_mgr, is_alive=lambda: False))
  await asyncio.wait_for(resume_entered.wait(), timeout=10.0)
  task.cancel()
  with contextlib.suppress(asyncio.CancelledError):
    await task
  assert task.cancelled()

  assert _thread_status(home, session_meta.id, thread.id) == "failed"
  assert _recovery_reports(home, session_meta.id) == []
