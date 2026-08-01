"""End-to-end restart recovery: a crashed server's worker run survives and finalizes.

Two-process A/B protocol:
  A is a short-lived driver subprocess that spawns a worker (a fake `claude`
  shim on PATH emitting claude-shaped NDJSON) and is then SIGKILLed mid-run —
  no cleanup, no finalize, exactly like a crashed server.
  B is this test process: it points a fresh CharlieBotConfig at the same
  CHARLIEBOT_HOME and runs startup crash recovery against the truth on disk.

Scenario "completed" kills A after the agent finished: the reconcile sees a
trailing result event and drain-finalizes from it. Scenario "running" kills A
while the agent still runs: the reconcile re-attaches to the live run and
follows it to completion. Both end in the same terminal state, proving the
transport (raw log + cursor + pid/pid_start) makes the server process
dispensable.
"""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import sys
import time
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path

import pytest

from src.core import init as init_module
from src.core import runs
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption
from src.core.spawner import resume_worker as _real_resume_worker

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


def _read_events(home: Path, session_id: str, thread_id: str) -> list[dict]:
  events_path = home / "sessions" / session_id / "threads" / thread_id / "data" / "events.jsonl"
  if not events_path.exists():
    return []
  return [json.loads(line) for line in events_path.read_text(encoding="utf-8").splitlines() if line.strip()]


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
    meta = _read_meta(home, ids["session"], ids["thread"])
    return meta.get("pid") is not None and meta.get("pid_start") is not None and meta.get("status") == "running"

  _wait_for(run_started, timeout=20.0, what="worker run did not start/persist identity")
  proc.kill()
  proc.wait(timeout=10)


async def _recover(monkeypatch: pytest.MonkeyPatch, home: Path) -> tuple[int, list[bool], list[str]]:
  """Run startup crash recovery as process B; record reattach mode + master wakes."""
  alive_at_reattach: list[bool] = []
  master_wakes: list[str] = []

  async def spy_resume(*args, **kwargs):
    alive_at_reattach.append(bool(kwargs["is_alive"]()))
    await _real_resume_worker(*args, **kwargs)

  async def fake_trigger_master(session_id: str, summary: str, cfg, session_mgr) -> None:
    master_wakes.append(summary)

  monkeypatch.setattr("src.core.spawner.resume_worker", spy_resume)
  monkeypatch.setattr("src.core.review.trigger_master", fake_trigger_master)

  cfg = _cfg(home)
  recovered = await init_module.run_crash_recovery(cfg, datetime.now(timezone.utc))
  await _await_recovery_tasks()
  return recovered, alive_at_reattach, master_wakes


def _assert_run_converged(home: Path, ids: dict) -> dict:
  """Terminal state both scenarios must reach, straight from the contract."""
  meta = _read_meta(home, ids["session"], ids["thread"])
  assert meta["status"] == "completed"
  assert meta["exit_code"] == 0

  thread_dir = home / "sessions" / ids["session"] / "threads" / ids["thread"]
  raw = thread_dir / "data" / runs.RAW_LOG_NAME
  # The run's completion time is the raw log's final write, not finalize time.
  completion = runs.raw_completion_time(raw)
  assert completion is not None
  recorded = datetime.fromisoformat(meta["completed_at"])
  assert abs((recorded - completion).total_seconds()) < 0.005

  # Projection equality (timestamps stripped), with duplicate-over-loss
  # tolerance: every raw-derived event persisted, none lost, none beyond a
  # single replay-at-crash-boundary duplicate.
  persisted = [{k: v for k, v in e.items() if k != "timestamp"} for e in _read_events(home, ids["session"], ids["thread"])]
  projected = runs.project_raw_events(runs.parse_raw_lines(raw.read_bytes()), lambda e: [e])
  counted = Counter(json.dumps(e, sort_keys=True) for e in persisted)
  for event in projected:
    key = json.dumps(event, sort_keys=True)
    assert counted[key] >= 1, f"lost event: {event}"
    assert counted[key] <= 2, f"duplicated beyond crash-boundary tolerance: {event}"
  assert counted.keys() == {json.dumps(e, sort_keys=True) for e in projected}

  # Every persisted event timestamp is clamped to the raw log's completion time.
  for event in _read_events(home, ids["session"], ids["thread"]):
    ts = event.get("timestamp")
    if ts is not None:
      assert datetime.fromisoformat(ts) <= completion

  # The cursor drained exactly to the end of the raw log.
  cursor = thread_dir / "data" / runs.CURSOR_NAME
  assert runs.read_raw_cursor(cursor) == raw.stat().st_size
  # Companion transport files exist.
  assert (thread_dir / "data" / runs.STDERR_LOG_NAME).exists()
  return meta


def _assert_finalize_effects_once(home: Path, ids: dict, master_wakes: list[str]) -> None:
  chat_path = home / "sessions" / ids["session"] / "data" / "chat_events.jsonl"
  chat_events = [json.loads(line) for line in chat_path.read_text(encoding="utf-8").splitlines() if line.strip()]
  terminal_summaries = [
      e for e in chat_events
      if e.get("type") == "worker_summary" and e.get("thread_id") == ids["thread"] and e.get("status") != "running"
  ]
  assert len(terminal_summaries) == 1
  assert len(master_wakes) == 1


@pytest.mark.asyncio
async def test_restart_recovers_completed_run(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Server crashed after the agent finished: drain-finalize from the result event."""
  home = tmp_path / "home"
  proc, ids = _launch_driver(tmp_path, home, result_delay=0.6)
  _kill_driver_mid_run(proc, home, ids)

  # Wait until the agent's result line landed and the process is long gone.
  raw = home / "sessions" / ids["session"] / "threads" / ids["thread"] / "data" / runs.RAW_LOG_NAME
  _wait_for(
      lambda: "E2E-RESULT-MARKER" in raw.read_text(encoding="utf-8", errors="replace"),
      timeout=20.0,
      what="agent result never arrived",
  )
  time.sleep(0.5)

  recovered, alive_at_reattach, master_wakes = await _recover(monkeypatch, home)

  assert recovered == 1
  # The run finished during downtime: drained (dead at reattach), not followed.
  assert alive_at_reattach == [False]
  _assert_run_converged(home, ids)
  _assert_finalize_effects_once(home, ids, master_wakes)


@pytest.mark.asyncio
async def test_restart_reattaches_running_run(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Server crashed while the agent kept running: re-attach and follow it to the end."""
  home = tmp_path / "home"
  proc, ids = _launch_driver(tmp_path, home, result_delay=3.0)
  _kill_driver_mid_run(proc, home, ids)

  # The agent is still alive for ~3s; recovery must judge the run ALIVE and
  # re-attach (is_alive consulted inside resume), then stream its remainder.
  recovered, alive_at_reattach, master_wakes = await _recover(monkeypatch, home)

  assert recovered == 1
  assert alive_at_reattach == [True]
  _assert_run_converged(home, ids)
  _assert_finalize_effects_once(home, ids, master_wakes)
