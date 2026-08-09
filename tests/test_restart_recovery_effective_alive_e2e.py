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
test process then runs startup crash recovery against the truth on disk.
"""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

import pytest

from src.agents.backends.base import AgentBackend
from src.core import init as init_module
from src.core import runs
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, CreateSessionRequest, ThreadStatus, utc_now
from src.core.sessions import SessionManager
from src.core.spawner import resume_worker as _real_resume_worker
from src.core.threads import ThreadManager

REPO_ROOT = Path(__file__).resolve().parents[1]

FAKE_SHIM = """#!/bin/sh
echo '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"E2E-ASSISTANT-MARKER"}]}}'
sleep "$FAKE_RESULT_DELAY"
echo '{"type":"result","subtype":"success","is_error":false,"result":"E2E-RESULT-MARKER","usage":{"input_tokens":1,"output_tokens":1}}'
exit 0
"""

DRIVER = """import asyncio
import json
import sys
from pathlib import Path

from src.core import spawner
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, CreateSessionRequest, SpawnRequest
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager


async def main() -> None:
  home = Path(sys.argv[1])
  cfg = CharlieBotConfig(
      charliebot_home=home,
      worktree_dir=str(home / "worktrees"),
      backend_options=[BackendOption(id="fake", label="Fake", type="cc-claude", model="fake-model")],
  )
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="e2e"))
  thread = await thread_mgr.create_thread(meta, "e2e task")
  # Sync handshake for the test harness (stdout is structlog's, not ours).
  (home / "driver_ids.json").write_text(json.dumps({"session": meta.id, "thread": thread.id}))
  await spawner.spawn_worker(
      meta.id,
      "e2e task",
      thread.id,
      cfg,
      session_mgr,
      thread_mgr,
      request=SpawnRequest(resolved_backend="fake", resolved_model="fake-model", prompt_override="do the thing"))


asyncio.run(main())
"""


def _cfg(home: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=home,
      worktree_dir=str(home / "worktrees"),
      backend_options=[BackendOption(id="fake", label="Fake", type="cc-claude", model="fake-model")],
  )


def _wait_for(predicate, timeout: float, what: str) -> None:
  deadline = time.monotonic() + timeout
  while time.monotonic() < deadline:
    if predicate():
      return
    time.sleep(0.05)
  raise TimeoutError(what)


def _read_meta(home: Path, session_id: str, thread_id: str) -> dict:
  meta_path = home / "sessions" / session_id / "threads" / thread_id / "metadata.json"
  return json.loads(meta_path.read_text(encoding="utf-8"))


async def _await_recovery_tasks() -> None:
  current = asyncio.current_task()
  pending = [
      t for t in asyncio.all_tasks()
      if t is not current and not t.done()
      and t.get_name().startswith(("resume-", "respawn-", "recomplete-"))
  ]
  if pending:
    await asyncio.gather(*pending)


def _install_shim(tmp_path: Path) -> Path:
  shim_dir = tmp_path / "shim"
  shim_dir.mkdir()
  shim = shim_dir / "claude"
  shim.write_text(FAKE_SHIM, encoding="utf-8")
  shim.chmod(0o755)
  return shim_dir


def _launch_driver(tmp_path: Path, home: Path, result_delay: float) -> tuple[subprocess.Popen, dict]:
  shim_dir = _install_shim(tmp_path)
  driver = tmp_path / "driver.py"
  driver.write_text(DRIVER, encoding="utf-8")
  env = dict(os.environ)
  env["PYTHONPATH"] = str(REPO_ROOT)
  env["PATH"] = f"{shim_dir}:{env['PATH']}"
  env["FAKE_RESULT_DELAY"] = str(result_delay)
  proc = subprocess.Popen(
      [sys.executable, str(driver), str(home)],
      stdout=subprocess.DEVNULL,
      stderr=subprocess.DEVNULL,
      env=env,
  )
  ids_file = home / "driver_ids.json"
  _wait_for(ids_file.exists, timeout=20.0, what="driver did not create session/thread")
  return proc, json.loads(ids_file.read_text(encoding="utf-8"))


def _kill_driver_mid_run(proc: subprocess.Popen, home: Path, ids: dict) -> None:
  """SIGKILL the driver once the run's identity is persisted and output is flowing."""
  thread_dir = home / "sessions" / ids["session"] / "threads" / ids["thread"]
  raw = thread_dir / "data" / runs.RAW_LOG_NAME

  def run_started() -> bool:
    if not raw.exists() or "E2E-ASSISTANT-MARKER" not in raw.read_text(encoding="utf-8", errors="replace"):
      return False
    try:
      meta = _read_meta(home, ids["session"], ids["thread"])
    except json.JSONDecodeError:
      # metadata.json is a plain (non-atomic) "w"-mode write; the driver may be
      # mid-write when this polls, which is exactly "not ready yet".
      return False
    return meta.get("pid") is not None and meta.get("pid_start") is not None and meta.get("status") == "running"

  _wait_for(run_started, timeout=20.0, what="worker run did not start/persist identity")
  proc.kill()
  proc.wait(timeout=10)


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
  recovered = await init_module.run_crash_recovery(cfg, datetime.now(timezone.utc))
  await _await_recovery_tasks()
  return recovered, alive_at_reattach, master_wakes, outcomes


def _terminal_summaries(home: Path, ids: dict) -> list[dict]:
  chat_path = home / "sessions" / ids["session"] / "data" / "chat_events.jsonl"
  events = [json.loads(line) for line in chat_path.read_text(encoding="utf-8").splitlines() if line.strip()]
  return [
      e for e in events
      if e.get("type") == "worker_summary" and e.get("thread_id") == ids["thread"] and e.get("status") != "running"
  ]


def _recovery_reports(home: Path, session_id: str) -> list[dict]:
  chat_path = home / "sessions" / session_id / "data" / "chat_events.jsonl"
  if not chat_path.exists():
    return []
  events = [json.loads(line) for line in chat_path.read_text(encoding="utf-8").splitlines() if line.strip()]
  return [e for e in events if e.get("source") == "crash_recovery"]


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
  cfg = CharlieBotConfig(
      charliebot_home=home,
      worktree_dir=str(home / "worktrees"),
      backend_options=[
          BackendOption(id="fake", label="Fake", type="cc-claude", model="fake-model"),
          BackendOption(id="fake-oc", label="FakeOC", type="opencode", model="fake-model"),
      ],
  )
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
