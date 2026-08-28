"""End-to-end boot recovery for effective-alive (unverifiable-death) runs.

Plan "boot recovery 误杀判定保守化", acceptance legs (a), (e), (f-mount):

  (a) a running worker whose pid_start is missing resolves RUNNING, is mounted
      with a constant-true liveness probe, and is never failed on missing
      evidence;
  (e) when that thread's real result event later lands, the existing
      completion path finalizes it exactly once;
  (f-mount) an uncovered-backend run that is effective-alive resolves RUNNING
      uncovered-alive and is REPORTED only — no follow is attached, nothing is
      torn down.

Same two-process A/B protocol as test_restart_recovery_e2e.py: a driver
subprocess spawns a worker with a fake `claude` shim and is SIGKILLed; this
test process then runs startup crash recovery against the truth on disk. The
protocol's scaffolding (shim, driver template, launcher, killer, waits,
chat-event readers) is shared by import from that module — edit it there.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime
from pathlib import Path

import pytest
from conftest import build_recovery_cfg
from test_restart_recovery_e2e import (
  _await_recovery_tasks,
  _cfg,
  _kill_driver_mid_run,
  _launch_driver,
  _read_meta,
  _recovery_reports,
  _terminal_summaries,
)

from src.agents.backends.base import AgentBackend
from src.core import init as init_module
from src.core import runs
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, ThreadStatus, utc_now
from src.core.sessions import SessionManager
from src.core.spawner import resume_worker as _real_resume_worker
from src.core.threads import ThreadManager


async def _recover(
    monkeypatch: pytest.MonkeyPatch, home: Path, cfg: CharlieBotConfig | None = None
) -> tuple[int, list[bool], list[str], list[runs.RunOutcome]]:
  """Run startup crash recovery as process B; record reattach mode, master
  wakes, and the resolve outcome each interrupted run received."""
  alive_at_reattach: list[bool] = []
  master_wakes: list[str] = []
  outcomes: list[runs.RunOutcome] = []

  async def spy_resume(*args, **kwargs):
    alive_at_reattach.append(bool(kwargs["is_alive"]()))
    await _real_resume_worker(*args, **kwargs)

  async def fake_trigger_master(session_id: str, summary: str, cfg, session_mgr) -> None:
    master_wakes.append(summary)

  real_resolve = runs.resolve_run

  def spy_resolve(**kwargs):
    resolution = real_resolve(**kwargs)
    outcomes.append(resolution.outcome)
    return resolution

  monkeypatch.setattr("src.core.spawner.resume_worker", spy_resume)
  monkeypatch.setattr("src.core.review.trigger_master", fake_trigger_master)
  monkeypatch.setattr("src.core.runs.resolve_run", spy_resolve)

  cfg = cfg or _cfg(home)
  recovered = await init_module.run_crash_recovery(cfg, datetime.now(UTC))
  await _await_recovery_tasks()
  return recovered, alive_at_reattach, master_wakes, outcomes


@pytest.mark.asyncio
async def test_pid_start_missing_running_worker_never_false_failed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Legs (a)+(e): pid_start scrubbed mid-run -> the boot judges RUNNING and
  mounts a constant-true probe (death unprovable), the shim's real result
  event then closes the run through the existing completion path — no failed
  finalize at any point (the 2026-08-08误杀 shape)."""
  home = tmp_path / "home"
  proc, ids = _launch_driver(tmp_path, home, result_delay=3.0)
  _kill_driver_mid_run(proc, home, ids)

  # Scrub pid_start: the shim process is alive, but the recorded identity can
  # no longer prove death (the incident's exact metadata shape).
  meta_path = home / "sessions" / ids["session"] / "threads" / ids["thread"] / "metadata.json"
  meta = json.loads(meta_path.read_text(encoding="utf-8"))
  assert meta.get("pid") is not None
  meta["pid_start"] = None
  meta_path.write_text(json.dumps(meta), encoding="utf-8")

  # With a constant-true probe the follow ends on the post-result timeout;
  # keep it fast.
  monkeypatch.setattr(AgentBackend, "_POST_RESULT_TIMEOUT", 1.0)

  recovered, alive_at_reattach, master_wakes, outcomes = await _recover(monkeypatch, home)

  assert recovered == 1
  assert outcomes == [runs.RunOutcome.RUNNING]
  # (a) the mounted probe is constant-true: alive-or-unverifiable, never a
  # death judgment against missing inputs.
  assert alive_at_reattach == [True]

  # (e) the real result event landed later and closed the run exactly once.
  meta = _read_meta(home, ids["session"], ids["thread"])
  assert meta["status"] == "completed"
  assert meta["exit_code"] == 0
  summaries = _terminal_summaries(home, ids)
  assert len(summaries) == 1
  assert len(master_wakes) == 1
  # No exception-gate report and no report-only分流: this thread always had a
  # followable transport, so nothing but the normal completion was emitted.
  assert _recovery_reports(home, ids["session"]) == []


@pytest.mark.asyncio
async def test_uncovered_effective_alive_run_reported_not_attached(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Leg (f-mount): uncovered backend + death unverifiable -> RUNNING
  uncovered-alive; recovery emits exactly one report and mounts nothing —
  the thread is left running and untouched."""
  home = tmp_path / "home"
  cfg = build_recovery_cfg(home)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session_meta = await session_mgr.create_session(CreateSessionRequest(name="uncovered-alive"))
  thread = await thread_mgr.create_thread(session_meta, "uncovered task")
  thread.status = ThreadStatus.RUNNING
  thread.backend = "fake-oc"
  thread.model = "fake-model"
  thread.pid = 4242
  thread.pid_start = None  # the scrubbed-input shape: death unverifiable
  thread.started_at = utc_now()
  await thread_mgr.save_metadata(thread)
  ids = {"session": session_meta.id, "thread": thread.id}

  # The run's raw log exists and holds no result event yet.
  data_dir = home / "sessions" / session_meta.id / "threads" / thread.id / "data"
  data_dir.mkdir(parents=True, exist_ok=True)
  (data_dir / runs.RAW_LOG_NAME).write_text(
      '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}\n',
      encoding="utf-8")

  recovered, alive_at_reattach, _master_wakes, outcomes = await _recover(monkeypatch, home, cfg=cfg)

  assert recovered == 1
  assert outcomes == [runs.RunOutcome.RUNNING]
  # Report-only分流: no follow was ever mounted for this thread.
  assert alive_at_reattach == []
  meta = _read_meta(home, ids["session"], ids["thread"])
  assert meta["status"] == "running"
  reports = _recovery_reports(home, ids["session"])
  assert len(reports) == 1
  assert runs.UNCOVERED_ALIVE_REASON in reports[0]["content"]


@pytest.mark.asyncio
async def test_uncovered_dead_pinned_worker_finalized_failed_with_reason(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The provably-dead counterpart of the effective-alive row: with pid_start
  pinned, reconcile proves the uncovered-backend process dead, resolves DIED
  with the transport reason, drains the run's pending output, and finalizes
  the thread failed with that reason carried into the worker summary."""
  home = tmp_path / "home"
  cfg = build_recovery_cfg(home)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session_meta = await session_mgr.create_session(CreateSessionRequest(name="uncovered-dead"))
  thread = await thread_mgr.create_thread(session_meta, "uncovered dead task")
  thread.status = ThreadStatus.RUNNING
  thread.backend = "fake-oc"
  thread.model = "fake-model"
  thread.pid = 999999  # dead: no /proc/999999 entry
  thread.pid_start = "1"  # pinned at spawn: the death above is provable
  thread.started_at = utc_now()
  await thread_mgr.save_metadata(thread)
  ids = {"session": session_meta.id, "thread": thread.id}

  # Pending work the dead server never drained: output without a result event.
  data_dir = home / "sessions" / session_meta.id / "threads" / thread.id / "data"
  data_dir.mkdir(parents=True, exist_ok=True)
  (data_dir / runs.RAW_LOG_NAME).write_text(
      '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}\n',
      encoding="utf-8")

  recovered, alive_at_reattach, _master_wakes, outcomes = await _recover(monkeypatch, home, cfg=cfg)

  assert recovered == 1
  assert outcomes == [runs.RunOutcome.DIED]
  assert alive_at_reattach == [False]
  meta = _read_meta(home, ids["session"], ids["thread"])
  assert meta["status"] == "failed"
  assert meta["exit_code"] == -1
  summaries = _terminal_summaries(home, ids)
  assert len(summaries) == 1
  assert runs.TRANSPORT_NOT_COVERED_REASON in summaries[0]["full_content"]
