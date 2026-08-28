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

A second driver protocol simulates a GRACEFUL shutdown: the driver cancels
the spawn task (exactly what the closing event loop does) and exits cleanly.
The shutdown must write no terminal state — covered runs survive and
re-attach on the next boot; everything else is finalized there with
resolve_run's explicit reason.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import os
import signal
import subprocess
import sys
import time
from collections import Counter
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import RECOVERY_TASK_PREFIXES, await_recovery_tasks, build_recovery_cfg

from src.agents.worker import QuotaExhaustedException, Worker
from src.core import event_types as ET
from src.core import init as init_module
from src.core import runs
from src.core import spawner as spawner_module
from src.core.config import CharlieBotConfig
from src.core.git import git_create_worktree, git_worktree_dir_name
from src.core.models import (
    BackendOption,
    CreateSessionRequest,
    SpawnRequest,
    TaskType,
    ThreadMetadata,
    ThreadStatus,
    utc_now,
)
from src.core.process import kill_process_group
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

# A fake reviewer: no LLM, just a real commit-of-its-own plus a `git push` of the
# worker's already-committed change to the shared worktree's base branch, standing
# in for the reviewer prompt's rebase+commit+push instructions (build_review_prompt,
# src/core/review.py). The commit is keyed on the shim's own pid ($$) so that two
# reviewer invocations *within the same test run* produce distinct commits — a
# same-commit re-push is a git no-op ("Everything up-to-date") and would leave a
# duplicated reviewer run undetectable from the origin repo's commit count alone.
REVIEWER_SHIM = """#!/bin/sh
echo '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"REVIEWER-ASSISTANT-MARKER"}]}}'
echo "reviewer touch $$" > "reviewer_shim_$$.txt"
git add "reviewer_shim_$$.txt"
git commit -m "reviewer shim commit $$" 1>&2
git push origin HEAD:main 1>&2
echo '{"type":"result","subtype":"success","is_error":false,"result":"REVIEWER-RESULT-MARKER","usage":{"input_tokens":1,"output_tokens":1}}'
exit 0
"""

# Attempt 1 of a VERIFY quota-retry pair: emits a rate_limit_event the way Claude
# Code does on a rejected quota check, then dies -- the exact shape Worker._process_event
# (src/agents/worker.py) turns into QuotaExhaustedException.
QUOTA_SHIM = """#!/bin/sh
cat >/dev/null
echo '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"ATTEMPT-1-MARKER"}]}}'
echo '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"tokens","resetsAt":"later"}}'
exit 1
"""

# Attempt 2: a clean retry with no quota event, completing normally.
CLEAN_RETRY_SHIM = """#!/bin/sh
cat >/dev/null
echo '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"ATTEMPT-2-MARKER"}]}}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"ATTEMPT-2-RESULT","usage":{"input_tokens":1,"output_tokens":1}}'
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
  await await_recovery_tasks(RECOVERY_TASK_PREFIXES)


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
  recovered = await init_module.run_crash_recovery(cfg, datetime.now(UTC))
  await _await_recovery_tasks()
  return recovered, alive_at_reattach, master_wakes, outcomes


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

  # Projection equality (timestamps stripped): nothing is ever lost, and the
  # whole STREAM carries at most one duplicated event in total (the one
  # straddling the persisted cursor when the kill landed between the raw
  # write and the cursor advance) — a budget for the stream, not per event.
  persisted = [{k: v for k, v in e.items() if k != "timestamp"} for e in _read_events(home, ids["session"], ids["thread"])]
  projected = runs.project_raw_events(runs.parse_raw_lines(raw.read_bytes()), lambda e: [e])
  persisted_counts = Counter(json.dumps(e, sort_keys=True) for e in persisted)
  projected_counts = Counter(json.dumps(e, sort_keys=True) for e in projected)
  for key, count in projected_counts.items():
    assert persisted_counts[key] >= count, f"lost event: {key}"
  assert set(persisted_counts) <= set(projected_counts), "persisted an event the raw projection never produced"
  duplicate_total = sum(persisted_counts.values()) - sum(projected_counts.values())
  assert duplicate_total in (0, 1), f"stream-wide duplicate budget exceeded: {duplicate_total} extra event(s)"

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

  recovered, alive_at_reattach, master_wakes, _outcomes = await _recover(monkeypatch, home)

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
  recovered, alive_at_reattach, master_wakes, _outcomes = await _recover(monkeypatch, home)

  assert recovered == 1
  assert alive_at_reattach == [True]
  _assert_run_converged(home, ids)
  _assert_finalize_effects_once(home, ids, master_wakes)


@pytest.mark.asyncio
async def test_projection_exact_equality_at_deterministic_line_boundary(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Kill lands exactly when the persisted cursor has caught up to the raw log's
  current end (a completed-line boundary). Unlike the timing-dependent scenarios
  above, this is constructed so the crash-boundary duplicate tolerance can never
  apply — projection equality must come out EXACT, not just within budget.
  """
  home = tmp_path / "home"
  # A long delay keeps the result line far away, so the deterministic snapshot
  # below can never race a second raw-log write landing before the kill.
  proc, ids = _launch_driver(tmp_path, home, result_delay=20.0)
  thread_dir = home / "sessions" / ids["session"] / "threads" / ids["thread"]
  raw = thread_dir / "data" / runs.RAW_LOG_NAME
  cursor = thread_dir / "data" / runs.CURSOR_NAME

  def cursor_caught_up_at_boundary() -> bool:
    if not raw.exists() or not cursor.exists():
      return False
    if "E2E-ASSISTANT-MARKER" not in raw.read_text(encoding="utf-8", errors="replace"):
      return False
    return runs.read_raw_cursor(cursor) == raw.stat().st_size

  _wait_for(cursor_caught_up_at_boundary, timeout=20.0, what="cursor never caught up to a line boundary")
  boundary_offset = raw.stat().st_size

  meta = _read_meta(home, ids["session"], ids["thread"])
  shim_pid = meta["pid"]
  assert shim_pid is not None
  proc.kill()
  proc.wait(timeout=10)
  # The shim (still sleeping toward the far-off result) outlives A independently
  # (its own process group) — kill it too so recovery observes a DEAD run.
  kill_process_group(shim_pid, signal.SIGKILL)

  # Confirm nothing raced in after the snapshot: no growth past the boundary.
  assert raw.stat().st_size == boundary_offset

  recovered, alive_at_reattach, _master_wakes, _outcomes = await _recover(monkeypatch, home)

  assert recovered == 1
  assert alive_at_reattach == [False]
  meta = _read_meta(home, ids["session"], ids["thread"])
  assert meta["status"] == "failed"  # DIED without a result event; no growth to drain

  persisted = [{k: v for k, v in e.items() if k != "timestamp"} for e in _read_events(home, ids["session"], ids["thread"])]
  projected = runs.project_raw_events(runs.parse_raw_lines(raw.read_bytes()), lambda e: [e])
  assert persisted == projected


# ---------------------------------------------------------------------------
# Finalize idempotency across repeated restarts (contract line 38)
# ---------------------------------------------------------------------------


def _run_git(cwd: Path, *args: str) -> None:
  subprocess.run(["git", *args], cwd=str(cwd), check=True, capture_output=True)


def _origin_commit_count(origin: Path, branch: str = "main") -> int:
  result = subprocess.run(
      ["git", "log", "--oneline", branch], cwd=str(origin), check=True, capture_output=True, text=True)
  return len(result.stdout.splitlines())


def _thread_metas(home: Path, session_id: str) -> list[dict]:
  threads_dir = home / "sessions" / session_id / "threads"
  if not threads_dir.is_dir():
    return []
  return [
      json.loads((p / "metadata.json").read_text(encoding="utf-8"))
      for p in threads_dir.iterdir()
      if (p / "metadata.json").exists()
  ]


def _session_chat_events(home: Path, session_id: str) -> list[dict]:
  path = home / "sessions" / session_id / "data" / "chat_events.jsonl"
  if not path.exists():
    return []
  return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def _recovery_reports(home: Path, session_id: str) -> list[dict]:
  return [e for e in _session_chat_events(home, session_id) if e.get("source") == "crash_recovery"]


async def _settle_finalize_window(home: Path, session_id: str, original_id: str) -> None:
  """Drain the named recovery tasks, then wait for the (idempotently, at most
  once) spawned reviewer thread's own completion. The reviewer's own spawn_worker
  task is unnamed (dispatched from spawn_review_worker), so _await_recovery_tasks()
  alone cannot see it — only the disk state can.
  """
  await _await_recovery_tasks()
  deadline = time.monotonic() + 20.0
  while time.monotonic() < deadline:
    reviewers = [m for m in _thread_metas(home, session_id) if m.get("review_of") == original_id]
    if reviewers and all(m.get("status") in ("completed", "failed", "cancelled") for m in reviewers):
      return
    await asyncio.sleep(0.05)
  raise TimeoutError("reviewer thread never settled")


@pytest.mark.asyncio
async def test_finalize_idempotent_across_repeated_restarts(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Contract line 38: drive the reconcile N (>=3) times over the same terminal,
  review-needing thread and assert the finalize effects — terminal worker_summary,
  master wake, reviewer thread, and the reviewer's commits reaching origin — each
  land exactly once (per ONE reviewer run), not once per reconcile. The server
  tracks no merge state of its own (src/core/review.py::spawn_review_worker), so the commit
  count must come from the origin repo itself, not a server-side proxy.
  """
  home = tmp_path / "home"

  # --- a real bare origin + a worktree already carrying the worker's own
  # (unpushed) commit, exactly as a completed-but-not-yet-reviewed worker would
  # leave it. ---
  origin = tmp_path / "origin.git"
  _run_git(tmp_path, "init", "--bare", "-b", "main", str(origin))
  seed = tmp_path / "seed"
  _run_git(tmp_path, "clone", str(origin), str(seed))
  _run_git(seed, "config", "user.email", "t@example.com")
  _run_git(seed, "config", "user.name", "T")
  (seed / "README.md").write_text("seed\n", encoding="utf-8")
  _run_git(seed, "add", "README.md")
  _run_git(seed, "commit", "-m", "seed")
  _run_git(seed, "push", "origin", "main")

  main_checkout = tmp_path / "main_checkout"
  _run_git(tmp_path, "clone", str(origin), str(main_checkout))
  _run_git(main_checkout, "config", "user.email", "t@example.com")
  _run_git(main_checkout, "config", "user.name", "T")

  branch_name = "charliebot/task-finalize-idem"
  worktree_dir = home / "worktrees"
  worktree_dir.mkdir(parents=True)
  wt_path = worktree_dir / git_worktree_dir_name(branch_name)
  await git_create_worktree(main_checkout, "main", branch_name, wt_path)
  (wt_path / "change.txt").write_text("worker change\n", encoding="utf-8")
  _run_git(wt_path, "add", "change.txt")
  _run_git(wt_path, "commit", "-m", "worker change")

  origin_commits_before = _origin_commit_count(origin)
  assert origin_commits_before == 1  # only the seed commit; the worker's commit is still local-only

  # --- the fake reviewer, on PATH: a real subprocess doing a real `git push`. ---
  shim_dir = tmp_path / "reviewer_shim"
  shim_dir.mkdir()
  shim = shim_dir / "claude"
  shim.write_text(REVIEWER_SHIM, encoding="utf-8")
  shim.chmod(0o755)
  monkeypatch.setenv("PATH", f"{shim_dir}:{os.environ['PATH']}")

  # --- fake trigger_master: records wakes AND persists a plausible master-output
  # event, so the idempotency judgment (master_woke_after_summary) sees "woke"
  # exactly as a real master turn would — otherwise every reconcile would judge
  # the wake still missing and re-fire it. ---
  master_wakes: list[str] = []

  async def fake_trigger_master(session_id: str, summary: str, cfg, session_mgr) -> None:
    master_wakes.append(summary)
    await session_mgr.persist_and_broadcast(
        session_id,
        {"type": ET.ASSISTANT, "message": {"role": "assistant", "content": [{"type": "text", "text": "ack"}]}})

  monkeypatch.setattr("src.core.review.trigger_master", fake_trigger_master)

  # --- keep the shared worktree alive across every reconcile round: the real
  # finalize_review_chain removes it once a review lands, which would make rounds
  # 2 and 3 exit at validate_review_prerequisites' worktree-exists check before ever
  # reaching the reviewer_thread_exists judgment (src/core/review.py::spawn_review_worker,
  # src/core/init_worker_recovery.py::_effects_maybe_missing) — masking whether that judgment actually
  # works. Neutering
  # only the worktree-removal step (a test-side no-op) lets every round walk the
  # judgment for real. ---
  async def fake_finalize_review_chain(*args, **kwargs) -> None:
    return None

  monkeypatch.setattr("src.core.review.finalize_review_chain", fake_finalize_review_chain)

  # --- the original worker thread: already terminal (completed), needing review. ---
  cfg = _cfg(home)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session_meta = await session_mgr.create_session(CreateSessionRequest(name="finalize-idempotency"))

  original = await thread_mgr.create_thread(session_meta, "Implement the finalize idempotency fixture.")
  original.status = ThreadStatus.COMPLETED
  original.exit_code = 0
  original.repo_path = str(main_checkout)
  original.branch_name = branch_name
  original.worktree_path = str(wt_path)
  original.base_branch = "main"
  original.backend = "fake"
  original.model = "fake-model"
  original.completed_at = utc_now()
  await thread_mgr.save_metadata(original)

  boot_time = utc_now() + timedelta(hours=1)  # treats every thread here as pre-boot on every pass

  for _ in range(3):
    await init_module.run_crash_recovery(cfg, boot_time)
    await _settle_finalize_window(home, session_meta.id, original.id)

  # Effect 1: terminal worker_summary for the original thread, exactly once.
  chat_events = _session_chat_events(home, session_meta.id)
  terminal_summaries = [
      e for e in chat_events
      if e.get("type") == "worker_summary" and e.get("thread_id") == original.id and e.get("status") != "running"
  ]
  assert len(terminal_summaries) == 1

  # Effect 2: master woke exactly once.
  assert len(master_wakes) == 1

  # Effect 3: exactly one reviewer thread derived from the original, and it ran
  # to completion. A broken reviewer_thread_exists judgment lets the reconcile
  # loop derive a second (or third) reviewer on the later rounds — the worktree
  # is kept alive above precisely so this is reachable.
  reviewer_threads = [m for m in _thread_metas(home, session_meta.id) if m.get("review_of") == original.id]
  assert len(reviewer_threads) == 1
  assert reviewer_threads[0]["status"] == "completed"

  # Effect 4: exactly the commits of ONE reviewer run reached the origin — the
  # worker's own commit plus the reviewer shim's own pid-keyed commit — counted
  # from the origin repo itself, since the server keeps no merge state of its
  # own. A duplicated reviewer run pushes its own additional pid-keyed commit,
  # which (unlike a same-commit re-push) is not a git no-op, so it strictly
  # raises this count.
  assert _origin_commit_count(origin) == origin_commits_before + 2


# ---------------------------------------------------------------------------
# Fresh-spawn transport rotation (VERIFY quota-retry fallback, bug 1)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_fresh_spawn_rotates_stale_raw_log_so_verify_retry_quota_not_replayed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The VERIFY quota-retry fallback (src/core/spawner_lifecycle.py::spawn_worker) respawns a
  fresh Worker into the SAME thread data dir. Without rotation, the fresh
  spawn's tail-follow (always start_offset=0) would replay attempt 1's entire
  raw stream first -- ending in the very quota event that killed attempt 1 --
  falsely concluding the retry backend exhausted too. A real subprocess (a
  `claude` shim on PATH), same two-attempt shape as the DRIVER/FAKE_SHIM e2e
  tests above.
  """
  shim_dir = tmp_path / "shim"
  shim_dir.mkdir()
  shim = shim_dir / "claude"
  shim.write_text(QUOTA_SHIM, encoding="utf-8")
  shim.chmod(0o755)
  monkeypatch.setenv("PATH", f"{shim_dir}:{os.environ['PATH']}")

  work_dir = tmp_path / "work"
  work_dir.mkdir()
  data_dir = tmp_path / "data"
  events_log = data_dir / "events.jsonl"
  cfg = _cfg(tmp_path / "home")
  thread = ThreadMetadata(session_id="sess-1", description="test")

  worker1 = Worker(
      thread_metadata=thread,
      working_dir=work_dir,
      events_log_path=events_log,
      task_description="do the thing",
      cfg=cfg,
  )
  with pytest.raises(QuotaExhaustedException):
    await worker1.run()

  raw_path = data_dir / runs.RAW_LOG_NAME
  assert raw_path.exists()
  assert "ATTEMPT-1-MARKER" in raw_path.read_text(encoding="utf-8")
  attempt1_line_count = len([line for line in events_log.read_text(encoding="utf-8").splitlines() if line.strip()])
  assert attempt1_line_count > 0

  # Attempt 2: same thread data dir (the retry fallback's own respawn shape),
  # a clean shim this time.
  shim.write_text(CLEAN_RETRY_SHIM, encoding="utf-8")
  shim.chmod(0o755)

  worker2 = Worker(
      thread_metadata=thread,
      working_dir=work_dir,
      events_log_path=events_log,
      task_description="do the thing",
      cfg=cfg,
  )
  exit_code = await worker2.run()
  assert exit_code == 0

  # The first attempt's bytes are preserved, just moved aside under a name
  # distinct from RAW_LOG_NAME.
  rotated = list(data_dir.glob(f"{runs.RAW_LOG_NAME}.*"))
  assert len(rotated) == 1
  assert "ATTEMPT-1-MARKER" in rotated[0].read_text(encoding="utf-8")

  # The current raw log holds only attempt 2's bytes.
  assert "ATTEMPT-1-MARKER" not in raw_path.read_text(encoding="utf-8")

  # events.jsonl accumulates across both attempts (same thread, same file) --
  # exactly like a real retry. What must NOT happen is attempt 2's own
  # contribution replaying attempt 1's assistant marker or its quota event.
  all_lines = [line for line in events_log.read_text(encoding="utf-8").splitlines() if line.strip()]
  attempt2_events = [json.loads(line) for line in all_lines[attempt1_line_count:]]
  assert attempt2_events
  assert not any("ATTEMPT-1-MARKER" in json.dumps(e) for e in attempt2_events)
  assert not any(e.get("type") == "rate_limit_event" for e in attempt2_events)


# ---------------------------------------------------------------------------
# Graceful shutdown: cancellation writes no terminal state (plan "关机不再抢先下结论")
# ---------------------------------------------------------------------------

GRACEFUL_DRIVER = """import asyncio
import contextlib
import json
import sys
from pathlib import Path

from src.core import runs, spawner
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, CreateSessionRequest, SpawnRequest
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager


async def main() -> None:
  home = Path(sys.argv[1])
  description = sys.argv[2]
  cfg = CharlieBotConfig(
      charliebot_home=home,
      worktree_dir=str(home / "worktrees"),
      backend_options=[BackendOption(id="fake", label="Fake", type="cc-claude", model="fake-model")],
  )
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="e2e"))
  thread = await thread_mgr.create_thread(meta, description)
  (home / "driver_ids.json").write_text(json.dumps({"session": meta.id, "thread": thread.id}))
  task = asyncio.create_task(
      spawner.spawn_worker(
          meta.id,
          description,
          thread.id,
          cfg,
          session_mgr,
          thread_mgr,
          request=SpawnRequest(resolved_backend="fake", resolved_model="fake-model", prompt_override="do the thing")))

  thread_dir = home / "sessions" / meta.id / "threads" / thread.id
  raw = thread_dir / "data" / runs.RAW_LOG_NAME

  def run_started() -> bool:
    if not raw.exists() or "E2E-ASSISTANT-MARKER" not in raw.read_text(encoding="utf-8", errors="replace"):
      return False
    try:
      m = json.loads((thread_dir / "metadata.json").read_text(encoding="utf-8"))
    except json.JSONDecodeError:
      return False
    return m.get("pid") is not None and m.get("pid_start") is not None and m.get("status") == "running"

  while not run_started():
    await asyncio.sleep(0.05)

  # Graceful shutdown: exactly what the closing event loop does to the task.
  task.cancel()
  with contextlib.suppress(asyncio.CancelledError):
    await task
  (home / "driver_done.json").write_text("{}")


asyncio.run(main())
"""


def _launch_graceful_driver(
    tmp_path: Path, home: Path, result_delay: float, description: str = "e2e task"
) -> tuple[subprocess.Popen, dict]:
  """Run the graceful driver to completion: spawn, wait for the run, cancel, exit."""
  shim_dir = _install_shim(tmp_path)
  driver = tmp_path / "graceful_driver.py"
  driver.write_text(GRACEFUL_DRIVER, encoding="utf-8")
  env = dict(os.environ)
  env["PYTHONPATH"] = str(REPO_ROOT)
  env["PATH"] = f"{shim_dir}:{env['PATH']}"
  env["FAKE_RESULT_DELAY"] = str(result_delay)
  proc = subprocess.Popen(
      [sys.executable, str(driver), str(home), description],
      stdout=subprocess.DEVNULL,
      stderr=subprocess.DEVNULL,
      env=env,
  )
  done = home / "driver_done.json"
  _wait_for(done.exists, timeout=30.0, what="graceful driver never finished cancelling")
  proc.wait(timeout=10)
  ids = json.loads((home / "driver_ids.json").read_text(encoding="utf-8"))
  return proc, ids


def _terminal_summaries(home: Path, ids: dict) -> list[dict]:
  return [
      e for e in _session_chat_events(home, ids["session"])
      if e.get("type") == "worker_summary" and e.get("thread_id") == ids["thread"] and e.get("status") != "running"
  ]


def _pid_alive(pid: int) -> bool:
  try:
    os.kill(pid, 0)
    return True
  except OSError:
    return False


@pytest.mark.asyncio
async def test_graceful_shutdown_lets_covered_run_survive_and_reattach(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Event-loop shutdown mid-run: a covered worker is neither signalled nor
  finalized; the next boot re-attaches and finishes with the real result."""
  home = tmp_path / "home"
  proc, ids = _launch_graceful_driver(tmp_path, home, result_delay=3.0)
  assert proc.returncode == 0

  meta = _read_meta(home, ids["session"], ids["thread"])
  assert meta["status"] == "running"  # shutdown wrote no terminal state
  assert meta.get("exit_code") is None
  assert _pid_alive(meta["pid"])  # the agent process outlived the server
  assert not _terminal_summaries(home, ids)  # finalize was skipped entirely

  recovered, alive_at_reattach, master_wakes, _outcomes = await _recover(monkeypatch, home)

  assert recovered == 1
  assert alive_at_reattach == [True]
  _assert_run_converged(home, ids)
  _assert_finalize_effects_once(home, ids, master_wakes)


@pytest.mark.asyncio
async def test_graceful_shutdown_in_setup_phase_reaches_never_started_row(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Cancellation before the agent process exists writes no terminal state; the
  next boot judges NEVER_STARTED and walks the existing respawn machinery
  (which, without a persisted delegation invocation, drain-finalizes)."""
  home = tmp_path / "home"
  cfg = _cfg(home)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session_meta = await session_mgr.create_session(CreateSessionRequest(name="graceful-setup"))
  thread = await thread_mgr.create_thread(session_meta, "e2e setup-phase task")
  ids = {"session": session_meta.id, "thread": thread.id}

  setup_entered = asyncio.Event()

  async def hang_in_setup(*args, **kwargs):
    setup_entered.set()
    await asyncio.Event().wait()

  monkeypatch.setattr("src.core.spawner_launch._create_repoless_process", hang_in_setup)
  task = asyncio.create_task(
      spawner_module.spawn_worker(
          session_meta.id, "e2e setup-phase task", thread.id, cfg, session_mgr, thread_mgr,
          request=SpawnRequest(resolved_backend="fake", resolved_model="fake-model", prompt_override="x")))
  await asyncio.wait_for(setup_entered.wait(), timeout=10.0)
  task.cancel()
  with contextlib.suppress(asyncio.CancelledError):
    await task

  meta = _read_meta(home, ids["session"], ids["thread"])
  assert meta["status"] == "idle"  # untouched: no failed/-1 fabricated at shutdown
  assert meta.get("exit_code") is None
  assert not _terminal_summaries(home, ids)

  recovered, alive_at_reattach, _master_wakes, outcomes = await _recover(monkeypatch, home)

  assert recovered == 1
  assert outcomes == [runs.RunOutcome.NEVER_STARTED]
  assert alive_at_reattach == [False]
  meta = _read_meta(home, ids["session"], ids["thread"])
  assert meta["status"] == "failed"  # existing fall-back: no invocation to respawn from


@pytest.mark.asyncio
async def test_restart_finalizes_uncovered_transport_with_explicit_reason(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The incident shape: an opencode-backed verify thread left running by a
  graceful shutdown. The next boot cannot re-attach (transport not covered),
  so the thread fails with resolve_run's reason — the module constant, not a
  retyped literal — in the worker_summary the master reads."""
  home = tmp_path / "home"
  cfg = build_recovery_cfg(home)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session_meta = await session_mgr.create_session(CreateSessionRequest(name="uncovered"))
  thread = await thread_mgr.create_thread(session_meta, "Verify plan fixture", task_type=TaskType.VERIFY)
  thread.status = ThreadStatus.RUNNING
  thread.backend = "fake-oc"
  thread.model = "fake-model"
  thread.pid = 4194304  # dead: beyond this host's live pids, /proc entry absent
  thread.pid_start = "1"
  thread.started_at = utc_now()
  thread.require_review = False
  await thread_mgr.save_metadata(thread)
  ids = {"session": session_meta.id, "thread": thread.id}

  recovered, alive_at_reattach, master_wakes, _outcomes = await _recover(monkeypatch, home, cfg=cfg)

  assert recovered == 1
  assert alive_at_reattach == [False]
  meta = _read_meta(home, ids["session"], ids["thread"])
  assert meta["status"] == "failed"
  assert meta["exit_code"] == -1
  summaries = _terminal_summaries(home, ids)
  assert len(summaries) == 1
  assert runs.TRANSPORT_NOT_COVERED_REASON in summaries[0]["full_content"]
  assert len(master_wakes) == 1


@pytest.mark.asyncio
async def test_restart_recovery_summary_invariant_to_backend_binary_presence(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Install-state invariance: the same interrupted opencode-backed VERIFY
  thread finalizes to the same worker_summary whether or not the host can
  resolve the `opencode` binary.

  Translate-only drain construction must not require the binary. The two arms
  are compared by summary equality — never against a hardcoded reason string —
  so the test pins the mechanism itself: the recorded outcome must not depend
  on host install state. Absence is simulated in opencode's own module
  namespace, where `resolve_binary` is bound (`from base import resolve_binary`).
  """

  async def run_once(binary: str | None) -> str:
    if binary is None:

      def _missing_binary(name: str, fallback: str) -> str:
        raise FileNotFoundError(f"{name} binary not found on PATH or at {fallback}")

      monkeypatch.setattr("src.agents.backends.opencode.resolve_binary", _missing_binary)
      home = tmp_path / "home-absent"
    else:
      monkeypatch.setattr(
          "src.agents.backends.opencode.resolve_binary", lambda name, fallback: binary)
      home = tmp_path / "home-present"

    cfg = build_recovery_cfg(home)
    session_mgr = SessionManager(cfg)
    thread_mgr = ThreadManager(cfg)
    session_meta = await session_mgr.create_session(CreateSessionRequest(name="invariance"))
    # The id is pinned across arms: full_content embeds it, and the comparison
    # below is by equality, so the two runs must narrate the SAME thread.
    thread = ThreadMetadata(
        id="invariance-opencode-verify",
        session_id=session_meta.id,
        description="Verify plan fixture",
        task_type=TaskType.VERIFY,
    )
    thread.status = ThreadStatus.RUNNING
    thread.backend = "fake-oc"
    thread.model = "fake-model"
    thread.pid = 4194304  # dead: beyond this host's live pids, /proc entry absent
    thread.pid_start = "1"
    thread.started_at = utc_now()
    thread.require_review = False
    thread_dir = home / "sessions" / session_meta.id / "threads" / thread.id
    (thread_dir / "data").mkdir(parents=True, exist_ok=True)
    await thread_mgr.save_metadata(thread)

    recovered, alive_at_reattach, _master_wakes, _outcomes = await _recover(monkeypatch, home, cfg=cfg)

    assert recovered == 1
    assert alive_at_reattach == [False]  # judged dead: drained, never re-attached
    meta = _read_meta(home, session_meta.id, thread.id)
    assert meta["status"] == "failed"
    summaries = _terminal_summaries(home, {"session": session_meta.id, "thread": thread.id})
    assert len(summaries) == 1
    return summaries[0]["full_content"]

  resolvable_summary = await run_once("/usr/bin/opencode")
  unresolvable_summary = await run_once(None)
  assert resolvable_summary == unresolvable_summary


@pytest.mark.asyncio
async def test_graceful_shutdown_winds_down_improve_iteration_with_reason(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An improve iteration dies with its loop at shutdown (terminated, not let
  go), but its terminal state still comes from the next boot's judgment chain:
  DIED with an explicit reason, no re-attach, no respawn."""
  home = tmp_path / "home"
  proc, ids = _launch_graceful_driver(
      tmp_path, home, result_delay=20.0, description=f"{runs.IMPROVE_ITERATION_PREFIX} 1/2")
  assert proc.returncode == 0

  meta = _read_meta(home, ids["session"], ids["thread"])
  assert meta["status"] == "running"  # no terminal state written at shutdown
  # ...but the iteration's process was terminated along with its loop.
  _wait_for(lambda: not _pid_alive(meta["pid"]), timeout=10.0, what="improve iteration outlived shutdown")

  recovered, alive_at_reattach, _master_wakes, _outcomes = await _recover(monkeypatch, home)

  assert recovered == 1
  assert alive_at_reattach == [False]  # judged dead: drained, never re-attached
  meta = _read_meta(home, ids["session"], ids["thread"])
  assert meta["status"] == "failed"
  assert meta["exit_code"] == -1
  summaries = _terminal_summaries(home, ids)
  assert len(summaries) == 1
  assert runs.DIED_WITHOUT_RESULT_REASON in summaries[0]["full_content"]
  # Improve iterations are never respawned: the session still has exactly one thread.
  assert len(_thread_metas(home, ids["session"])) == 1


@pytest.mark.asyncio
async def test_ui_cancel_endpoint_still_finalizes_cancelled(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Regression: the cancel endpoint (SIGTERM + CANCELLED status, no task
  cancellation) is untouched by the shutdown change — the spawn task finishes
  normally and finalize keeps the cancelled status."""
  from src.api.threads import cancel_thread

  home = tmp_path / "home"
  shim_dir = _install_shim(tmp_path)
  monkeypatch.setenv("PATH", f"{shim_dir}:{os.environ['PATH']}")
  monkeypatch.setenv("FAKE_RESULT_DELAY", "20")

  master_wakes: list[str] = []

  async def fake_trigger_master(session_id: str, summary: str, cfg, session_mgr) -> None:
    master_wakes.append(summary)

  monkeypatch.setattr("src.core.review.trigger_master", fake_trigger_master)

  cfg = _cfg(home)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session_meta = await session_mgr.create_session(CreateSessionRequest(name="ui-cancel"))
  thread = await thread_mgr.create_thread(session_meta, "e2e cancel task")
  ids = {"session": session_meta.id, "thread": thread.id}
  task = asyncio.create_task(
      spawner_module.spawn_worker(
          session_meta.id, "e2e cancel task", thread.id, cfg, session_mgr, thread_mgr,
          request=SpawnRequest(resolved_backend="fake", resolved_model="fake-model", prompt_override="do the thing")))

  def started() -> bool:
    try:
      m = _read_meta(home, ids["session"], ids["thread"])
    except (FileNotFoundError, json.JSONDecodeError):
      return False
    return m.get("pid") is not None and m.get("status") == "running"

  deadline = time.monotonic() + 20.0
  while not started():
    assert time.monotonic() < deadline, "worker never started"
    await asyncio.sleep(0.05)

  await cancel_thread(ids["session"], ids["thread"], thread_mgr)
  await task  # completes normally: this path never cancels the spawn task

  meta = _read_meta(home, ids["session"], ids["thread"])
  assert meta["status"] == "cancelled"
  summaries = _terminal_summaries(home, ids)
  assert len(summaries) == 1
  assert summaries[0]["status"] == "cancelled"
