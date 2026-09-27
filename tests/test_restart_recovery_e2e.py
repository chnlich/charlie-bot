"""End-to-end and unit coverage of restart recovery across the two protocols.

Two-process A/B protocol (worker side and master side):
  A is a short-lived driver subprocess (a fake `claude` shim on PATH emitting
  claude-shaped NDJSON; on the master legs it runs a real master turn through
  ``master_cc.run_message``) that is then SIGKILLed mid-run - no cleanup, no
  finalize, exactly like a crashed server. B is this test process: it points a
  fresh CharlieBotConfig at the same CHARLIEBOT_HOME and runs startup crash
  recovery against the truth on disk.

Worker-side legs pin that a crashed server's worker run survives and
finalizes: re-attach to a live run, drain a completed turn exactly once,
rotate stale raw logs so verify retry quota is not replayed, finalize
uncovered transports with resolve_run's explicit reason, and the graceful
shutdown variant where covered runs survive and re-attach.

Master-side legs pin re-attach, drain, replay, queue drain and read-back for
master turns, and the transport unit invariants underneath: unanswered-event
scan, persist/clear of the master-run record, and the cancel path's let-go
rule (a covered transport whose record hit disk is detached, never
terminated; unprovable records get no signal).

Effective-alive legs pin boot recovery for unverifiable-death runs: a running
worker whose pid_start is missing is never failed on missing evidence, its
late result finalizes exactly once, and an effective-alive uncovered run is
reported only - no follow attached, nothing torn down.

The crash-recovery waits, readers, killer and recovery entry the four files
shared were single-homed in tests/conftest.py and moved here when the files
merged; the shim/driver/launcher trio for the worker protocol stays in the
worker-side section below.
"""

from __future__ import annotations

from __future__ import annotations
from collections import Counter
from collections.abc import AsyncIterator
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock, MagicMock
import asyncio
import json
import os
import signal
import subprocess
import sys
import time

import pytest
from structlog.testing import capture_logs

from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    LITELLM_503_ERROR_MESSAGE,
    REVIEW_TRIGGER_MASTER_PATCH_TARGET,
    ROOT,
    _async_wait_for,
    _cfg,
    _recovery_reports,
    _wait_for,
    backend_option,
    make_work_item,
    mocked_callback_fields,
    patch_instructions_content,
    read_chat_events,
    user_event,
)

from src.agents.worker import QuotaExhaustedError, Worker
from src.core import event_types as ET
from src.core import finalize_effects, runs
from src.core import init as init_module
from src.core import spawner as spawner_module
from src.core.config import CharlieBotConfig
from src.core.git import git_create_worktree, git_worktree_dir_name
from src.core.models import (
    CreateSessionRequest,
    SpawnRequest,
    TaskType,
    ThreadMetadata,
    ThreadStatus,
    utc_now,
)
from src.core.process import kill_process_group
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.agents import master_cc, master_cc_queue
from src.core.message_aggregator import MessageAggregator
from src.core.models import (
    CcClaudeBackend,
    MasterRunRecord,
    OpencodeBackend,
    SessionCallbacks,
    SessionMetadata,
)
from src.core.sessions import HISTORY_LOCATION_NOTE
from src.agents import master_cc_state
from src.core import process as core_process
from src.core.models import (
    BackendOption,
)
from src.agents.backends.base import AgentBackend
from src.core.spawner import resume_worker as _real_resume_worker

# Crash-recovery protocol helpers, single-homed here for the restart tests
# (moved from tests/conftest.py when the four restart files merged). The A/B
# protocol's driver side (fake `claude` shim, driver template, launcher) stays
# in the worker-side section below; these are the waits, readers, and the
# startup-crash-recovery entry the legs share.

SPAWNER_RESUME_WORKER_PATCH_TARGET = "src.core.spawner.resume_worker"


def build_recovery_cfg(home: Path) -> CharlieBotConfig:
  """CharlieBotConfig for restart-recovery tests: the home dir is caller-chosen (the install-invariance test
  runs its two arms under different homes), the worktrees dir lives under it, and the backend list registers
  the cc-claude fake plus the opencode fake-oc whose uncovered transport the recovery legs exercise."""
  return CharlieBotConfig(
      charliebot_home=home,
      paths={"worktree_dir": str(home / "worktrees")},
      backends={
          "options":
              [
                  backend_option(id="fake", label="Fake", type="cc-claude", model="fake-model"),
                  backend_option(id="fake-oc", label="FakeOC", type="opencode", model="fake-model"),
              ]
      },
  )


# Crash-recovery follow-ups are dispatched through create_logged_task under fixed name
# prefixes: resume-drain/resume-follow/respawn-worker/recomplete-finalize in
# src/core/init_worker_recovery.py, master-resume/master-replay in
# src/core/init_master_recovery.py, master-consumer in src/agents/master_cc_queue.py.
# A recovery test must drain those tasks before asserting on rewritten metadata.
RECOVERY_TASK_PREFIXES = ("resume-", "respawn-", "recomplete-")


MASTER_RECOVERY_TASK_PREFIXES = (*RECOVERY_TASK_PREFIXES, "master-resume-", "master-replay-", "master-consumer-")


async def await_recovery_tasks(prefixes: tuple[str, ...]) -> None:
  """Gather every unfinished named recovery task, repeating until none is left.

  A drained task may itself dispatch another named recovery task, so one gather
  pass can still leave work pending.
  """
  current = asyncio.current_task()
  while True:
    pending = [
        t for t in asyncio.all_tasks() if t is not current and not t.done() and t.get_name().startswith(prefixes)
    ]
    if not pending:
      return
    await asyncio.gather(*pending)


def _pid_alive(pid: int) -> bool:
  """True while *pid* is signalable, False when the kernel reports it gone.

  Catches only ProcessLookupError — the one failure os.kill(pid, 0) gives on a
  process the test spawned itself; anything else (e.g. PermissionError) means
  the probe cannot answer and propagates.
  """
  try:
    os.kill(pid, 0)
  except ProcessLookupError:
    return False
  return True


def _read_meta(home: Path, session_id: str, thread_id: str) -> dict:
  meta_path = home / "sessions" / session_id / "threads" / thread_id / "metadata.json"
  return json.loads(meta_path.read_text(encoding="utf-8"))


async def _await_recovery_tasks() -> None:
  await await_recovery_tasks(RECOVERY_TASK_PREFIXES)


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


async def _recover(monkeypatch: pytest.MonkeyPatch,
                   home: Path,
                   cfg: CharlieBotConfig | None = None) -> tuple[int, list[bool], list[str], list[runs.RunOutcome]]:
  """Run startup crash recovery as process B; record reattach mode, master
  wakes, and the resolve outcome each interrupted run received."""
  alive_at_reattach: list[bool] = []
  master_wakes: list[str] = []
  outcomes: list[runs.RunOutcome] = []

  async def spy_resume(*args: object, **kwargs: object) -> None:
    alive_at_reattach.append(bool(kwargs["is_alive"]()))
    await _real_resume_worker(*args, **kwargs)

  async def fake_trigger_master(
      session_id: str, summary: str, cfg: CharlieBotConfig, session_mgr: SessionManager) -> None:
    master_wakes.append(summary)

  real_resolve = runs.resolve_run

  def spy_resolve(**kwargs: object) -> runs.RunResolution:
    resolution = real_resolve(**kwargs)
    outcomes.append(resolution.outcome)
    return resolution

  monkeypatch.setattr(SPAWNER_RESUME_WORKER_PATCH_TARGET, spy_resume)
  monkeypatch.setattr(REVIEW_TRIGGER_MASTER_PATCH_TARGET, fake_trigger_master)
  monkeypatch.setattr("src.core.runs.resolve_run", spy_resolve)

  cfg = cfg or _cfg(home)
  recovered = await init_module.run_crash_recovery(cfg, datetime.now(UTC))
  await _await_recovery_tasks()
  return recovered, alive_at_reattach, master_wakes, outcomes


def _terminal_summaries(home: Path, ids: dict) -> list[dict]:
  return [
      e for e in read_chat_events(home, ids["session"])
      if e.get("type") == "worker_summary" and e.get("thread_id") == ids["thread"] and e.get("status") != "running"
  ]


def _assert_failed_with_transport_reason(home: Path, ids: dict) -> None:
  """Shared tail of the uncovered-backend recovery tests: the thread finalizes failed with exit
  code -1 and resolve_run's transport reason lands in exactly one terminal worker_summary."""
  meta = _read_meta(home, ids["session"], ids["thread"])
  assert meta["status"] == "failed"
  assert meta["exit_code"] == -1
  summaries = _terminal_summaries(home, ids)
  assert len(summaries) == 1
  assert runs.TRANSPORT_NOT_COVERED_REASON in summaries[0]["full_content"]


# A fake reviewer: no LLM, just a real commit-of-its-own plus a `git push` of the
# worker's already-committed change to the shared worktree's base branch, standing
# in for the reviewer prompt's rebase+commit+push instructions (build_review_prompt,
# src/core/review.py). The commit is keyed on the shim's own pid ($$) so that two
# reviewer invocations *within the same test run* produce distinct commits — a
# same-commit re-push is a git no-op ("Everything up-to-date") and would leave a
# duplicated reviewer run undetectable from the origin repo's commit count alone.
REVIEWER_SHIM = """#!/bin/sh
echo '{"type":"assistant","message":{"role":"assistant","content":'\
'[{"type":"text","text":"REVIEWER-ASSISTANT-MARKER"}]}}'
echo "reviewer touch $$" > "reviewer_shim_$$.txt"
git add "reviewer_shim_$$.txt"
git commit -m "reviewer shim commit $$" 1>&2
git push origin HEAD:main 1>&2
echo '{"type":"result","subtype":"success","is_error":false,"result":"REVIEWER-RESULT-MARKER",'\
'"usage":{"input_tokens":1,"output_tokens":1}}'
exit 0
"""


# Attempt 1 of a VERIFY quota-retry pair: emits a rate_limit_event the way Claude
# Code does on a rejected quota check, then dies -- the exact shape Worker._process_event
# (src/agents/worker.py) turns into QuotaExhaustedError.
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
echo '{"type":"result","subtype":"success","is_error":false,"result":"ATTEMPT-2-RESULT",'\
'"usage":{"input_tokens":1,"output_tokens":1}}'
exit 0
"""


FAKE_SHIM = """#!/bin/sh
echo '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"E2E-ASSISTANT-MARKER"}]}}'
sleep "$FAKE_RESULT_DELAY"
echo '{"type":"result","subtype":"success","is_error":false,"result":"E2E-RESULT-MARKER",'\
'"usage":{"input_tokens":1,"output_tokens":1}}'
exit 0
"""


# The two drivers' shared session rig, spliced into each driver source after
# `home` binds. Each driver's imports must cover every name the fragment uses.
_DRIVER_SESSION_SETUP = """\
  cfg = CharlieBotConfig(
      charliebot_home=home,
      paths={"worktree_dir": str(home / "worktrees")},
      backends={"options": [CcClaudeBackend(id="fake", label="Fake", model="fake-model")]},
  )
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="e2e"))
"""


DRIVER = """import asyncio
import json
import sys
from pathlib import Path

from src.core import spawner
from src.core.config import CharlieBotConfig
from src.core.models import CcClaudeBackend, CreateSessionRequest, SpawnRequest
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager


async def main() -> None:
  home = Path(sys.argv[1])
""" + _DRIVER_SESSION_SETUP + """  thread = await thread_mgr.create_thread(meta, "e2e task")
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


def _read_events(home: Path, session_id: str, thread_id: str) -> list[dict]:
  events_path = home / "sessions" / session_id / "threads" / thread_id / "data" / "events.jsonl"
  if not events_path.exists():
    return []
  return [json.loads(line) for line in events_path.read_text(encoding="utf-8").splitlines() if line.strip()]


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
  env["PYTHONPATH"] = str(ROOT)
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


def _assert_run_converged(home: Path, ids: dict) -> dict:
  """Terminal state both scenarios must reach."""
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
  persisted = [
      {
          k: v for k, v in e.items() if k != "timestamp"
      } for e in _read_events(home, ids["session"], ids["thread"])
  ]
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


def _run_git(cwd: Path, *args: str) -> None:
  subprocess.run(["git", *args], cwd=str(cwd), check=True, capture_output=True)


def _origin_commit_count(origin: Path) -> int:
  result = subprocess.run(
      ["git", "log", "--oneline", "main"], cwd=str(origin), check=True, capture_output=True, text=True)
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


async def _settle_finalize_window(home: Path, session_id: str, original_id: str) -> None:
  """Drain the named recovery tasks, then wait for the (idempotently, at most
  once) spawned reviewer thread's own completion AND the master wake its
  finalize fires. The reviewer's own spawn_worker task is unnamed (dispatched
  from spawn_review_worker), so _await_recovery_tasks() alone cannot see it —
  only the disk state can. The wake rides that same unnamed task, after the
  terminal status write the loop above waits on, so the settle must also cover
  the wake window: the next round's reconcile judges the wake by
  finalize_effects.master_woke_after_summary over the chat events, and a round
  starting before the ack lands would judge it missing and re-fire it.
  """
  await _await_recovery_tasks()

  def settled() -> bool:
    reviewers = [m for m in _thread_metas(home, session_id) if m.get("review_of") == original_id]
    reviewers_settled = bool(reviewers) and all(
        m.get("status") in ("completed", "failed", "cancelled") for m in reviewers)
    woke = finalize_effects.master_woke_after_summary(read_chat_events(home, session_id), original_id)
    return reviewers_settled and woke

  await _async_wait_for(settled, 20.0, "reviewer thread or its master wake never settled")


GRACEFUL_DRIVER = """import asyncio
import contextlib
import json
import sys
from pathlib import Path

from src.core import runs, spawner
from src.core.config import CharlieBotConfig
from src.core.models import CcClaudeBackend, CreateSessionRequest, SpawnRequest
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager


async def main() -> None:
  home = Path(sys.argv[1])
  description = sys.argv[2]
""" + _DRIVER_SESSION_SETUP + """  thread = await thread_mgr.create_thread(meta, description)
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


def _launch_graceful_driver(tmp_path: Path,
                            home: Path,
                            result_delay: float,
                            description: str = "e2e task") -> tuple[subprocess.Popen, dict]:
  """Run the graceful driver to completion: spawn, wait for the run, cancel, exit."""
  shim_dir = _install_shim(tmp_path)
  driver = tmp_path / "graceful_driver.py"
  driver.write_text(GRACEFUL_DRIVER, encoding="utf-8")
  env = dict(os.environ)
  env["PYTHONPATH"] = str(ROOT)
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


MASTER_FAKE_SHIM = r"""#!/bin/sh
# Fake `claude`: records argv + stdin prompt, emits claude-shaped NDJSON under
# SHIM_MODE control. Invocation counters live under $SHIM_STATE/inv-<n>.*.
mode="$SHIM_MODE"
state="$SHIM_STATE"
mkdir -p "$state"
n=1
while [ -e "$state/inv-$n.argv" ]; do
  n=$((n + 1))
done
printf '%s\n' "$@" > "$state/inv-$n.argv"
cat > "$state/inv-$n.prompt"
if [ "$mode" = "error_hang" ]; then
  echo "{\"type\":\"error\",\"message\":\"__LITELLM_503_ERROR_MESSAGE__\"}"
  printf '\033[1;31mGive Feedback / Get Help: https://github.com/BerriAI/litellm/issues/new\033[0m\n' >&2
  printf "LiteLLM.Info: If you need to debug this error, use \`litellm._turn_on_debug()'.\n" >&2
  echo "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":"\
"[{\"type\":\"text\",\"text\":\"ASSISTANT-INV-$n\"}]}}"
  while :; do sleep 60; done
fi
echo "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":"\
"[{\"type\":\"text\",\"text\":\"ASSISTANT-INV-$n\"}]}}"
case "$mode" in
  hang)
    while :; do sleep 60; done
    ;;
  sleep_first)
    sleep "$SHIM_SLEEP"
    ;;
esac
if [ "$mode" = "delegate" ]; then
  python -m src.cli.delegate --session "$SHIM_DELEGATE_SESSION" --repo "$SHIM_DELEGATE_REPO" \
      --task-spec-file "$SHIM_DELEGATE_SPEC" --base-branch main --keep-worktree 0 \
      > "$state/delegate.stdout" 2> "$state/delegate.stderr"
  echo "$?" > "$state/delegate.rc"
fi
echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"RESULT-INV-$n\","\
"\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}"
exit 0
"""

MASTER_FAKE_SHIM = MASTER_FAKE_SHIM.replace("__LITELLM_503_ERROR_MESSAGE__", LITELLM_503_ERROR_MESSAGE)


# The two drivers' shared turn rig, spliced into each driver source after
# `home` and `shim` bind. Each driver's imports must cover every name the
# fragment uses.
_DRIVER_TURN_SETUP = """\
  cfg = CharlieBotConfig(
      charliebot_home=home,
      paths={"worktree_dir": str(home / "worktrees")},
      backends={"options": [
          CcClaudeBackend(id="fake", label="Fake", model="fake-model", cli_binary=shim, prompt_overlay="none")]},
  )
  # Prompt assembly is orthogonal to this protocol; keep the turn minimal.
  master_cc_run._build_instructions_content = lambda session_meta, cfg, prompt_overlay: "instructions"
"""


MASTER_DRIVER = """import asyncio
import json
import sys
from pathlib import Path

from src.agents import master_cc, master_cc_run
from src.core.config import CharlieBotConfig
from src.core.models import CcClaudeBackend, CreateSessionRequest, TaskType, ThreadStatus
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager


async def main() -> None:
  home = Path(sys.argv[1])
  shim = sys.argv[2]
  kind = sys.argv[3]
""" + _DRIVER_TURN_SETUP + """  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="master-e2e"))

  if kind == "delegate":
    # Reconstruct "turn 1 delegated and the effect landed, then the response
    # was lost": the worker thread for this exact spec already exists on disk,
    # terminally, with its finalize effects present.
    spec = Path(sys.argv[4]).read_text(encoding="utf-8")
    thread = await thread_mgr.create_thread(meta, spec, task_type=TaskType.IMPLEMENT)
    await thread_mgr.update_status(meta.id, thread.id, ThreadStatus.FAILED)
    await session_mgr.save_chat_event(
        meta.id,
        {"type": "worker_summary", "thread_id": thread.id, "status": "failed", "content": "worker failed"})
    await session_mgr.save_chat_event(
        meta.id,
        {"type": "assistant",
         "message": {"role": "assistant", "content": [{"type": "text", "text": "prior master output"}]}})

  callbacks = session_mgr.callbacks()
  # kind == "wake": a turn with no user event in the chat log (delegate /
  # cron / improve wake): the record's user_event_id stays None, so the
  # replay pass has nothing to redeliver for it.
  skip_user_event = kind == "wake"
  task_a = asyncio.create_task(
      master_cc.run_message(cfg, meta, "message A", callbacks, skip_user_event=skip_user_event))
  if not skip_user_event:
    while not any(e.get("content") == "message A" for e in session_mgr.load_chat_events_sync(meta.id)):
      await asyncio.sleep(0.01)
  extra_tasks = []
  if kind == "queued":
    # Strict A-before-B enqueue ordering: A's user event is already on disk.
    task_b = asyncio.create_task(master_cc.run_message(cfg, meta, "message B", callbacks))
    extra_tasks.append(task_b)
    while not any(e.get("content") == "message B" for e in session_mgr.load_chat_events_sync(meta.id)):
      await asyncio.sleep(0.01)

  # Handshake for the harness: user event(s) (and any seed) are durable.
  (home / "driver_ids.json").write_text(json.dumps({"session": meta.id}))
  await asyncio.gather(task_a, *extra_tasks)  # never returns in killed scenarios


asyncio.run(main())
"""


def _master_cfg(home: Path, shim: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=home,
      paths={"worktree_dir": str(home / "worktrees")},
      backends={
          "options":
              [
                  CcClaudeBackend(
                      id="fake", label="Fake", model="fake-model", cli_binary=str(shim), prompt_overlay="none")
              ]
      },
  )


def _uncovered_transport_cfg(home: Path, shim: Path) -> CharlieBotConfig:
  """The uncovered-transport pair's config: the covered fake shim plus the oc
  backend the pinned session rides."""
  return CharlieBotConfig(
      charliebot_home=home,
      paths={"worktree_dir": str(home / "worktrees")},
      backends={
          "options":
              [
                  CcClaudeBackend(id="fake", label="Fake", model="fake-model", cli_binary=str(shim)),
                  OpencodeBackend(id="oc", label="OC", model="oc-model", prompt_overlay="none"),
              ]
      },
  )


def _session_meta(home: Path, session_id: str) -> dict:
  return json.loads((home / "sessions" / session_id / "metadata.json").read_text(encoding="utf-8"))


def _raw_logs(home: Path, session_id: str) -> list[Path]:
  runs_dir = home / "sessions" / session_id / "data" / "master_runs"
  if not runs_dir.is_dir():
    return []
  return sorted(runs_dir.glob(f"*/{runs.RAW_LOG_NAME}"))


def _shim_prompt(state: Path, n: int) -> str:
  return (state / f"inv-{n}.prompt").read_text(encoding="utf-8")


def _wait_turn_started(home: Path, session_id: str, what: str) -> None:
  """Wait for the recorded turn's identity and its first assistant output to be durable."""
  _wait_for(
      lambda: _session_meta(home, session_id)["master_run"] is not None and any(
          "ASSISTANT-INV-1" in r.read_text(encoding="utf-8", errors="replace") for r in _raw_logs(home, session_id)),
      timeout=20.0,
      what=what)


async def _master_await_recovery_tasks() -> None:
  await await_recovery_tasks(MASTER_RECOVERY_TASK_PREFIXES)


def _master_install_shim(tmp_path: Path) -> tuple[Path, Path]:
  shim_dir = tmp_path / "shim"
  shim_dir.mkdir()
  shim = shim_dir / "claude"
  shim.write_text(MASTER_FAKE_SHIM, encoding="utf-8")
  shim.chmod(0o755)
  state = tmp_path / "shim_state"
  state.mkdir()
  return shim, state


def _master_launch_driver(
    tmp_path: Path,
    home: Path,
    shim: Path,
    kind: str,
    shim_mode: str,
    extra_args: list[str] | None = None) -> tuple[subprocess.Popen, str]:
  shim_dir = tmp_path / "shim"
  driver = shim_dir / "driver.py"
  driver.write_text(MASTER_DRIVER, encoding="utf-8")
  home.mkdir(exist_ok=True)
  env = dict(os.environ)
  env["PYTHONPATH"] = str(ROOT)
  env["SHIM_MODE"] = shim_mode
  env["SHIM_STATE"] = str(tmp_path / "shim_state")
  env["SHIM_SLEEP"] = "3"
  proc = subprocess.Popen(
      [sys.executable, str(driver), str(home),
       str(shim), kind, *(extra_args or [])],
      stdout=subprocess.DEVNULL,
      stderr=subprocess.DEVNULL,
      env=env,
  )
  ids_file = home / "driver_ids.json"
  _wait_for(ids_file.exists, timeout=20.0, what="driver did not create session / persist user events")
  return proc, json.loads(ids_file.read_text(encoding="utf-8"))["session"]


async def _master_recover(
    monkeypatch: pytest.MonkeyPatch,
    home: Path,
    shim: Path,
    state: Path,
    cfg: CharlieBotConfig | None = None,
    shim_mode: str = "immediate",
    extra_env: dict[str, str] | None = None) -> CharlieBotConfig:
  """Run startup crash recovery as process B: point the shim at `state`, run
  `run_crash_recovery`, and wait out the recovery tasks. Returns the config.
  The default `shim_mode` "immediate" makes any hypothetical respawn exit fast,
  so a bug that respawns fails the inv-2 assertions instead of hanging."""
  patch_instructions_content(monkeypatch)
  monkeypatch.setenv("SHIM_MODE", shim_mode)
  monkeypatch.setenv("SHIM_STATE", str(state))
  for key, value in (extra_env or {}).items():
    monkeypatch.setenv(key, value)
  cfg = cfg if cfg is not None else _master_cfg(home, shim)
  await init_module.run_crash_recovery(cfg, datetime.now(UTC))
  await _master_await_recovery_tasks()
  return cfg


def _capture_replays(monkeypatch: pytest.MonkeyPatch) -> list[dict]:
  """Record the replay pass instead of spawning the uncovered backend.

  The marker application lives in ``replay_user_message``, which stays real.
  Returns the list each replay lands in.
  """
  replays: list[dict] = []

  async def _capture_run_message(
      cfg: CharlieBotConfig, session_meta: SessionMetadata, user_content: str, callbacks: SessionCallbacks,
      **kwargs: object) -> None:
    replays.append({"content": user_content, "user_event_id": kwargs.get("user_event_id")})

  monkeypatch.setattr(master_cc_queue, "run_message", _capture_run_message)
  patch_instructions_content(monkeypatch)
  return replays


def _master_pid(home: Path, session_id: str) -> int:
  """The recorded master agent pid, or fail the wait if no record exists yet."""
  record = _session_meta(home, session_id)["master_run"]
  assert record is not None and record["pid"] is not None
  return record["pid"]


def _kill_agent_only(home: Path, session_id: str) -> None:
  """SIGKILL the recorded master agent's process group (it runs its own session)."""
  pid = _master_pid(home, session_id)
  kill_process_group(pid, signal.SIGKILL)


def _turn_finished_on_disk(home: Path, session_id: str, marker: str) -> bool:
  """Producer exited with its trailing bytes durable and the record still set.

  The recovery-relevant state for a COMPLETED row: the raw log carries
  ``marker``, and the recorded (pid, pid_start, started_at) identity is dead.
  """
  raws = _raw_logs(home, session_id)
  if not raws or marker not in raws[0].read_text(encoding="utf-8", errors="replace"):
    return False
  record = _session_meta(home, session_id)["master_run"]
  if record is None:
    return False
  started_at = datetime.fromisoformat(record["started_at"])
  return not runs.is_run_alive(record["pid"], record["pid_start"], started_at, runs.read_host_boot_time())


def _round_transported_events(events: list[dict], *, skip_user_event: bool) -> list[dict]:
  """The round's raw-log-transported events, stripped of server-injected keys.

  The boundaries of "the round" are its user event (when one exists) and its
  MASTER_DONE — the transported events are everything the agent's raw stream
  contributed between them.
  """
  done_idx = next(i for i, e in enumerate(events) if e.get("type") == "master_done")
  start = 0
  if skip_user_event:
    start = next(i for i, e in enumerate(events) if e.get("type") == "user") + 1
  return [{k: v for k, v in e.items() if k not in ("id", "timestamp", "event_index")} for e in events[start:done_idx]]


def _assert_drain_matches_projection(transported: list[dict], projected: list[dict]) -> None:
  """The drained record carries the raw log's events in stream order — nothing
  lost, nothing invented.

  Exact equality is the norm. One deviation is the crash ordering
  ``tail_follow_events`` accepts (src/agents/backends/base.py): the cursor
  advances only after the consumer persisted the line's events, so a kill in
  that gap makes recovery replay the straddling line — the record then
  carries one duplicated event. The shim emits one flat event per line, so
  the replayed suffix is exactly the one straddling event; a loss, an
  invented event, a reorder, or a second duplicate still fails.
  """
  if transported == projected:
    return
  assert any(
      transported[:k] == projected[:k] and transported[k:] == projected[k - 1:]
      for k in range(1,
                     len(projected) + 1)), f"drain not lossless\ntransported: {transported}\nprojected: {projected}"


def _full_projection(home: Path, session_id: str, cfg: CharlieBotConfig) -> list[dict]:
  """Project the recorded turn's whole raw log from offset 0, fresh translate."""
  raw = _raw_logs(home, session_id)[0]
  option = cfg.get_backend_option(_session_meta(home, session_id)["backend"])
  return runs.project_raw_events(runs.parse_raw_lines(raw.read_bytes()), master_cc._build_fresh_translate(cfg, option))


def _assert_round_operable(events: list[dict]) -> None:
  """The round closes with a separator whose event_index is present — the
  render condition for Clone to here / Elon-e / Recap."""
  agg = MessageAggregator()
  messages = [d["message"] for ev in events for d in agg.feed(ev) if d.get("type") == "message"]
  separators = [m for m in messages if m.get("role") == "separator"]
  assert separators, "no separator row projected for the recovered round"
  assert all(m.get("event_index") is not None for m in separators)


def _assert_round_closed_once(events: list[dict], home: Path, session_id: str, exit_code: int) -> None:
  """Exactly one MASTER_DONE closed the round with `exit_code`, and the record cleared."""
  master_done = [e for e in events if e.get("type") == "master_done"]
  assert len(master_done) == 1
  assert master_done[0].get("exit_code") == exit_code
  assert _session_meta(home, session_id)["master_run"] is None


async def _completed_turn_downtime_rig(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, transport: str, *,
    started_what: str) -> tuple[Path, Path, CharlieBotConfig, str]:
  """Launch a ``sleep_first`` turn, kill the server, wait for the result event
  to land on disk, and run recovery. The final bytes arrive while nobody
  consumes, so recovery must resolve a COMPLETED row, never re-attach.
  Returns ``(home, state, cfg, session_id)``."""
  home = tmp_path / "home"
  shim, state = _master_install_shim(tmp_path)
  proc, session_id = _master_launch_driver(tmp_path, home, shim, transport, "sleep_first")
  _wait_turn_started(home, session_id, what=started_what)
  proc.kill()
  proc.wait(timeout=10)
  _wait_for(
      lambda: _turn_finished_on_disk(home, session_id, "RESULT-INV-1"),
      timeout=20.0,
      what="agent did not finish during the server-down window")
  cfg = await _master_recover(monkeypatch, home, shim, state)
  return home, state, cfg, session_id


async def _assert_drain_lossless_and_idempotent(
    home: Path, session_id: str, cfg: CharlieBotConfig, events: list[dict], *, skip_user_event: bool) -> None:
  """A drained COMPLETED row is lossless — the transported events match a fresh
  full projection of the raw log modulo the one straddling event a kill in the
  persist->cursor gap replays (_assert_drain_matches_projection), the cursor
  ends at file size, and the round is operable — and re-running recovery over
  the same on-disk state appends nothing."""
  _assert_drain_matches_projection(
      _round_transported_events(events, skip_user_event=skip_user_event), _full_projection(home, session_id, cfg))
  raw = _raw_logs(home, session_id)[0]
  assert runs.read_raw_cursor(raw.parent / runs.CURSOR_NAME) == raw.stat().st_size
  _assert_round_operable(events)
  chat_path = home / "sessions" / session_id / "data" / "chat_events.jsonl"
  before = chat_path.read_bytes()
  await init_module.run_crash_recovery(cfg, datetime.now(UTC))
  await _master_await_recovery_tasks()
  assert chat_path.read_bytes() == before


def _transport_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      backends={"options": [backend_option(id="fake", label="Fake", type="cc-claude", model="fake-model")]},
  )


def _user(content: str, event_id: str) -> dict:
  return {"type": "user", "content": content, "id": event_id}


class _HungBackend:
  """A live backend that streams nothing; cancel-path tests cancel its owner task.

  ``terminate``/``detach`` are spies so each row of the let-go contract is
  asserted on the mechanism (which was called), never on a literal.
  """

  def __init__(self, *, fire_spawn: bool = True) -> None:
    self.pid_start = "424242.0"
    self.exit_code = 1
    self.stderr_text = ""
    self.terminated = False
    self.terminate = AsyncMock()
    self.detach = MagicMock()
    self.on_spawn = None
    self._fire_spawn = fire_spawn
    # Set after _on_spawn returned: the master_run record is on disk by then.
    self.spawned = asyncio.Event()
    # Set the moment the run loop starts, before any spawn callback.
    self.run_entered = asyncio.Event()

  async def run(self,
                prompt: str,
                cwd: str,
                env: dict,
                uploaded_files: list[dict] | None = None) -> AsyncIterator[dict]:
    self.run_entered.set()
    if self._fire_spawn:
      await self.on_spawn(4242)
      self.spawned.set()
    await asyncio.Event().wait()
    yield  # pragma: no cover — cancellation always lands first


def _install_backend(monkeypatch: pytest.MonkeyPatch, backend: _HungBackend) -> None:

  def _build(*args: Any, **kwargs: Any) -> _HungBackend:
    backend.on_spawn = kwargs["on_spawn"]
    return backend

  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, _build)
  patch_instructions_content(monkeypatch)


def _persisting_callbacks(session_mgr: SessionManager, *, mark_unread: AsyncMock | None = None) -> SessionCallbacks:
  """Real persist_master_run against a tmp-home manager; everything else mocked."""
  return SessionCallbacks(
      persist_and_broadcast=AsyncMock(),
      **mocked_callback_fields(mark_unread=mark_unread if mark_unread is not None else AsyncMock()),
      persist_master_run=session_mgr.persist_master_run,
  )


def _cancel_item(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    callbacks: SessionCallbacks,
    option: BackendOption,
    *,
    should_check_tex: bool = False) -> master_cc._WorkItem:
  return make_work_item(
      cfg,
      session_meta,
      option,
      user_content="hi",
      callbacks=callbacks,
      should_check_tex=should_check_tex,
      user_event_id="evt-1")


async def _cancel_run(item: master_cc._WorkItem, ready: asyncio.Event) -> None:
  """Drive _run_cc until *ready*, then cancel — the event-loop shutdown trigger."""
  task = asyncio.create_task(master_cc._run_cc(item))
  await asyncio.wait_for(ready.wait(), timeout=5)
  task.cancel()
  with pytest.raises(asyncio.CancelledError):
    await task


async def _uncovered_thread(
    home: Path,
    *,
    name: str,
    prompt: str,
    pid: int,
    pid_start: str | None,
) -> tuple[CharlieBotConfig, dict]:
  """Build the uncovered-backend running thread both legs share: a RUNNING thread
  on the fake-oc backend whose raw log holds assistant output with no result event
  yet. ``pid``/``pid_start`` are the leg's only lever: pinned they make the death
  provable, scrubbed they leave it unverifiable."""
  cfg = build_recovery_cfg(home)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session_meta = await session_mgr.create_session(CreateSessionRequest(name=name))
  thread = await thread_mgr.create_thread(session_meta, prompt)
  thread.status = ThreadStatus.RUNNING
  thread.backend = "fake-oc"
  thread.model = "fake-model"
  thread.pid = pid
  thread.pid_start = pid_start
  thread.started_at = utc_now()
  await thread_mgr.save_metadata(thread)
  data_dir = home / "sessions" / session_meta.id / "threads" / thread.id / "data"
  data_dir.mkdir(parents=True, exist_ok=True)
  (data_dir / runs.RAW_LOG_NAME).write_text(
      '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}\n', encoding="utf-8")
  return cfg, {"session": session_meta.id, "thread": thread.id}


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
async def test_finalize_idempotent_across_repeated_restarts(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Drive the reconcile N (>=3) times over the same terminal,
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
  worktrees_root = home / "worktrees"
  worktrees_root.mkdir(parents=True)
  wt_path = worktrees_root / git_worktree_dir_name(branch_name)
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

  async def fake_trigger_master(
      session_id: str, summary: str, cfg: CharlieBotConfig, session_mgr: SessionManager) -> None:
    master_wakes.append(summary)
    await session_mgr.persist_and_broadcast(
        session_id, {
            "type": ET.ASSISTANT,
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "text",
                    "text": "ack"
                }]
            }
        })

  monkeypatch.setattr(REVIEW_TRIGGER_MASTER_PATCH_TARGET, fake_trigger_master)

  # --- keep the shared worktree alive across every reconcile round: the real
  # finalize_review_chain removes it once a review lands, which would make rounds
  # 2 and 3 exit at validate_review_prerequisites' worktree-exists check before ever
  # reaching the reviewer_thread_exists judgment (src/core/review.py::spawn_review_worker,
  # src/core/init_worker_recovery.py::_effects_maybe_missing) — masking whether that judgment actually
  # works. Neutering
  # only the worktree-removal step (a test-side no-op) lets every round walk the
  # judgment for real. ---
  async def fake_finalize_review_chain(*args: object, **kwargs: object) -> None:
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
  chat_events = read_chat_events(home, session_meta.id)
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
  with pytest.raises(QuotaExhaustedError):
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
async def test_restart_finalizes_uncovered_transport_with_explicit_reason(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An opencode-backed verify thread left running by a graceful shutdown. The
  next boot cannot re-attach (transport not covered), so the thread fails with
  resolve_run's reason — the module constant, not a retyped literal — in the
  worker_summary the master reads."""
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
  _assert_failed_with_transport_reason(home, ids)
  assert len(master_wakes) == 1


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
async def test_ui_cancel_endpoint_still_finalizes_cancelled(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Regression: the cancel endpoint (SIGTERM + CANCELLED status, no task
  cancellation) is untouched by the shutdown change — the spawn task finishes
  normally and finalize keeps the cancelled status."""
  from src.api.threads import cancel_thread

  home = tmp_path / "home"
  shim_dir = _install_shim(tmp_path)
  monkeypatch.setenv("PATH", f"{shim_dir}:{os.environ['PATH']}")
  monkeypatch.setenv("FAKE_RESULT_DELAY", "20")

  master_wakes: list[str] = []

  async def fake_trigger_master(
      session_id: str, summary: str, cfg: CharlieBotConfig, session_mgr: SessionManager) -> None:
    master_wakes.append(summary)

  monkeypatch.setattr(REVIEW_TRIGGER_MASTER_PATCH_TARGET, fake_trigger_master)

  cfg = _cfg(home)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  session_meta = await session_mgr.create_session(CreateSessionRequest(name="ui-cancel"))
  thread = await thread_mgr.create_thread(session_meta, "e2e cancel task")
  ids = {"session": session_meta.id, "thread": thread.id}
  task = asyncio.create_task(
      spawner_module.spawn_worker(
          session_meta.id,
          "e2e cancel task",
          thread.id,
          cfg,
          session_mgr,
          thread_mgr,
          request=SpawnRequest(resolved_backend="fake", resolved_model="fake-model", prompt_override="do the thing")))

  def started() -> bool:
    try:
      m = _read_meta(home, ids["session"], ids["thread"])
    except (FileNotFoundError, json.JSONDecodeError):
      return False
    return m.get("pid") is not None and m.get("status") == "running"

  await _async_wait_for(started, 20.0, "worker never started")

  await cancel_thread(ids["session"], ids["thread"], thread_mgr)
  await task  # completes normally: this path never cancels the spawn task

  meta = _read_meta(home, ids["session"], ids["thread"])
  assert meta["status"] == "cancelled"
  summaries = _terminal_summaries(home, ids)
  assert len(summaries) == 1
  assert summaries[0]["status"] == "cancelled"


@pytest.mark.asyncio
async def test_master_reattach_after_server_kill(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Server died; agent kept running: re-attach, no second spawn, no replay."""
  home = tmp_path / "home"
  shim, state = _master_install_shim(tmp_path)
  proc, session_id = _master_launch_driver(tmp_path, home, shim, "chat", "sleep_first")

  # Wait until the turn is recorded and its first output is durable.
  def turn_started() -> bool:
    raw = _raw_logs(home, session_id)
    record = _session_meta(home, session_id)["master_run"]
    return bool(raw) and record is not None and "ASSISTANT-INV-1" in raw[0].read_text(
        encoding="utf-8", errors="replace")

  _wait_for(turn_started, timeout=20.0, what="master turn did not start/persist identity")
  proc.kill()
  proc.wait(timeout=10)

  # Backdate the persisted turn's recorded start 600s, so a correct re-attach
  # reports a thinking interval beginning before the restart's recovery window.
  # The mechanism under test—taking the interval start from the record—must
  # surface that 600s on the MASTER_DONE thinking_seconds; a restart-fresh
  # interval would report ~0s.
  meta_path = home / "sessions" / session_id / "metadata.json"
  meta = json.loads(meta_path.read_text(encoding="utf-8"))
  rec = meta["master_run"]
  assert rec is not None
  rec_started = datetime.fromisoformat(rec["started_at"])
  backdated = rec_started - timedelta(seconds=600)
  rec["started_at"] = backdated.isoformat()
  meta_path.write_text(json.dumps(meta), encoding="utf-8")
  # Keep the run "live": is_run_alive requires started_at to postdate the most
  # recent host boot (nothing survives a reboot), and _reconcile_master_runs
  # funnels this same value into the liveness closure it hands the re-attach.
  monkeypatch.setattr(runs, "read_host_boot_time", lambda: backdated - timedelta(hours=1))

  await _master_recover(monkeypatch, home, shim, state)

  # The decisive re-attach proof: exactly one shim invocation ever happened —
  # the turn was followed, never respawned nor replayed.
  assert not (state / "inv-2.argv").exists(), "a second master process was spawned"
  assert "message A" in _shim_prompt(state, 1)
  events = read_chat_events(home, session_id)
  assert sum(1 for e in events if e.get("type") == "master_done") == 1
  user_events = [e for e in events if e.get("type") == "user"]
  assert len(user_events) == 1
  assert _session_meta(home, session_id)["master_run"] is None
  # The cursor drained exactly to the end of the raw log.
  raw = _raw_logs(home, session_id)[0]
  cursor = raw.parent / runs.CURSOR_NAME
  assert runs.read_raw_cursor(cursor) == raw.stat().st_size

  # The re-attached turn's MASTER_DONE counts the whole interval — including
  # the backdated 600s before the restart — because the interval start came
  # from the persisted record, not from the restart's enqueue.
  master_done = [e for e in events if e.get("type") == "master_done"]
  assert len(master_done) == 1
  assert master_done[0].get(
      "thinking_seconds",
      0) >= 600, (f"interval must count the backdated start; got {master_done[0].get('thinking_seconds')}s")


@pytest.mark.asyncio
async def test_master_replay_when_master_killed_with_server(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Server and agent died together: the message is replayed with the marker."""
  home = tmp_path / "home"
  shim, state = _master_install_shim(tmp_path)
  proc, session_id = _master_launch_driver(tmp_path, home, shim, "chat", "hang")
  _wait_for(
      lambda: _session_meta(home, session_id)["master_run"] is not None,
      timeout=20.0,
      what="master turn identity was never recorded")
  proc.kill()
  proc.wait(timeout=10)
  _kill_agent_only(home, session_id)

  await _master_recover(monkeypatch, home, shim, state)

  # The replay spawned exactly one new agent, with the replay marker + the
  # original content, and the original user event was not rewritten.
  assert (state / "inv-2.argv").exists(), "replayed turn never spawned"
  replayed_prompt = _shim_prompt(state, 2)
  assert replayed_prompt.startswith(master_cc_queue._REPLAY_MARKER)
  assert "message A" in replayed_prompt
  events = read_chat_events(home, session_id)
  assert len([e for e in events if e.get("type") == "user"]) == 1
  assert sum(1 for e in events if e.get("type") == "master_done") == 1
  assert _session_meta(home, session_id)["master_run"] is None


@pytest.mark.asyncio
async def test_queued_message_answered_after_restart(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A running + B queued at kill: A is re-attached (not replayed), B is
  replayed with the marker and answered only after A drains."""
  home = tmp_path / "home"
  shim, state = _master_install_shim(tmp_path)
  proc, session_id = _master_launch_driver(tmp_path, home, shim, "queued", "sleep_first")
  _wait_turn_started(home, session_id, what="turn A did not start/persist identity and first output")
  proc.kill()
  proc.wait(timeout=10)

  await _master_recover(monkeypatch, home, shim, state)

  # A: re-attached, prompt unmarked. B: one new spawn, only after A. The
  # replayed prompt opens with the context-reset note (A's round completed
  # without landing a resume id, so the anchor-missing drop fired) and then
  # carries the replay marker + the original content.
  assert "message A" in _shim_prompt(state, 1)
  assert not _shim_prompt(state, 1).startswith(master_cc_queue._REPLAY_MARKER)
  assert (state / "inv-2.argv").exists(), "queued message B was never replayed"
  replayed_prompt = _shim_prompt(state, 2)
  assert replayed_prompt.startswith(
      f"[Context reset: the previous conversation could not be resumed. {HISTORY_LOCATION_NOTE}]\n\n"
      f"{master_cc_queue._REPLAY_MARKER}")
  assert "message B" in replayed_prompt

  events = read_chat_events(home, session_id)
  assert len([e for e in events if e.get("type") == "user"]) == 2
  assert sum(1 for e in events if e.get("type") == "master_done") == 2

  def text_of(ev: dict) -> str:
    msg = ev.get("message", {})
    return "".join(b.get("text", "") for b in msg.get("content", []) if isinstance(b, dict))

  idx_a = [i for i, e in enumerate(events) if "ASSISTANT-INV-1" in text_of(e)]
  idx_b = [i for i, e in enumerate(events) if "ASSISTANT-INV-2" in text_of(e)]
  assert idx_a and idx_b, "both turns must have persisted assistant output"
  assert max(idx_a) < min(idx_b), "queued message B was answered before A drained"
  assert _session_meta(home, session_id)["master_run"] is None


@pytest.mark.asyncio
async def test_completed_turn_drained_after_server_kill(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The turn's final result landed on disk inside the server-down window:
  recovery resolves the COMPLETED row, drains the bytes after the cursor
  through the follower, and closes the round exactly once."""
  home, state, cfg, session_id = await _completed_turn_downtime_rig(
      tmp_path, monkeypatch, "chat", started_what="turn A did not start/persist identity and first output")

  # Exactly one answer: MASTER_DONE landed once, and the user message was NOT
  # replayed (a replay would have spawned a second agent). Both would mean
  # the COMPLETED row was drained AND replayed; neither would mean it was
  # cleared unanswered — the regression this row prevents.
  assert not (state / "inv-2.argv").exists(), "recovering a completed turn must start no agent process"
  events = read_chat_events(home, session_id)
  assert len([e for e in events if e.get("type") == "user"]) == 1
  _assert_round_closed_once(events, home, session_id, exit_code=0)

  await _assert_drain_lossless_and_idempotent(home, session_id, cfg, events, skip_user_event=True)
  # The re-run must also leave the record cleared, not resurrect it.
  assert _session_meta(home, session_id)["master_run"] is None


@pytest.mark.asyncio
@pytest.mark.parametrize("with_user_message", [True, False], ids=["user-wake", "delegate-wake"])
async def test_uncovered_transport_turn_cleared_not_drained(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, with_user_message: bool) -> None:
  """An interrupted turn on an uncovered backend transport (opencode /
  antigravity / tui-cli) is drained NEVER: with the pid_start pin present the
  dead instance's death is provable, so the record resolves DIED with the
  transport reason, clears WITHOUT any uncovered-alive report, and the user
  message — when one exists — is answered by the replay pass. No user
  message, nothing to answer: the round simply closes."""
  home = tmp_path / "home"
  shim, state = _master_install_shim(tmp_path)
  cfg = _uncovered_transport_cfg(home, shim)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"), backend="oc")
  user_event_id = None
  if with_user_message:
    event = user_event("message A")
    await session_mgr.save_chat_event(meta.id, event)
    user_event_id = event["id"]
  record = MasterRunRecord(
      pid=999999,
      pid_start="1",
      started_at=datetime.now(UTC) - timedelta(seconds=5),
      raw_log=str(home / "sessions" / meta.id / "data" / "master_runs" / "gone" / runs.RAW_LOG_NAME),
      user_event_id=user_event_id,
  )
  await session_mgr.persist_master_run(meta.id, record)

  replays = _capture_replays(monkeypatch)

  with capture_logs() as logs:
    await init_module.run_crash_recovery(cfg, datetime.now(UTC))
  await _master_await_recovery_tasks()

  # Provable death, not liveness limbo: DIED carrying the transport reason —
  # never an uncovered-alive report, which is the pin-less record's judgment.
  resolved = [e for e in logs if e.get("event") == "master_run_resolved"]
  assert len(resolved) == 1
  assert resolved[0]["outcome"] == runs.RunOutcome.DIED.value
  assert resolved[0]["reason"] == runs.TRANSPORT_NOT_COVERED_REASON
  assert _session_meta(home, meta.id)["master_run"] is None
  events = read_chat_events(home, meta.id)
  assert not any(e.get("source") == "crash_recovery" for e in events)
  # The drain invariant: no drain ran, so this turn never produced a MASTER_DONE.
  assert not any(e.get("type") == "master_done" for e in events)
  if with_user_message:
    assert len(replays) == 1
    assert replays[0]["content"].startswith(master_cc_queue._REPLAY_MARKER)
    assert "message A" in replays[0]["content"]
    assert replays[0]["user_event_id"] == user_event_id
  else:
    assert not replays
  assert not (state / "inv-1.argv").exists(), "recovery must not spawn any agent for this row"


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("pid", "pid_start"),
    [(999999, "1"), (None, None)],
    ids=["legacy-raw-missing", "never-started"],
)
async def test_undrainable_dead_turn_replayed_with_marker(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, pid: int | None, pid_start: str | None) -> None:
  """Raw log missing (pre-transport record) or turn never spawned: nothing is
  drainable, the record clears, and the user message is replayed with the
  marker — exactly one answer, by replay and only by replay."""
  home = tmp_path / "home"
  shim, state = _master_install_shim(tmp_path)
  cfg = _master_cfg(home, shim)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"))
  event = user_event("message A")
  await session_mgr.save_chat_event(meta.id, event)
  record = MasterRunRecord(
      pid=pid,
      pid_start=pid_start,
      started_at=datetime.now(UTC) - timedelta(seconds=5),
      raw_log=str(home / "sessions" / meta.id / "data" / "master_runs" / "gone" / runs.RAW_LOG_NAME),
      user_event_id=event["id"],
  )
  await session_mgr.persist_master_run(meta.id, record)

  await _master_recover(monkeypatch, home, shim, state, cfg=cfg)

  # Exactly one answer: one new agent, carrying the marker + the original
  # content; the original user event was not rewritten nor duplicated.
  assert (state / "inv-1.argv").exists(), "replayed turn never spawned"
  assert not (state / "inv-2.argv").exists()
  replayed_prompt = _shim_prompt(state, 1)
  assert replayed_prompt.startswith(master_cc_queue._REPLAY_MARKER)
  assert "message A" in replayed_prompt
  events = read_chat_events(home, meta.id)
  assert len([e for e in events if e.get("type") == "user"]) == 1
  assert sum(1 for e in events if e.get("type") == "master_done") == 1
  assert _session_meta(home, meta.id)["master_run"] is None


def test_unanswered_scan_picks_only_events_after_last_master_done() -> None:
  events = [
      _user("answered already", "e1"),
      {
          "type": "assistant",
          "id": "a1"
      },
      {
          "type": "master_done",
          "id": "d1"
      },
      _user("still open", "e2"),
      _user("queued behind it", "e3"),
  ]
  pending = init_module.unanswered_user_events(events, set())
  assert [e["id"] for e in pending] == ["e2", "e3"]


def test_unanswered_scan_excludes_the_recorded_turns_event_only() -> None:
  # Exclusion must be per-event: excluding e2 (the crashed turn's message)
  # must NOT also shield e3, which disappeared with the killed in-memory queue.
  events = [
      _user("running when killed", "e2"),
      _user("queued behind it", "e3"),
  ]
  pending = init_module.unanswered_user_events(events, {"e2"})
  assert [e["id"] for e in pending] == ["e3"]


@pytest.mark.asyncio
async def test_master_run_persist_does_not_clobber_unrelated_fields(tmp_path: Path) -> None:
  cfg = _transport_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"))
  await session_mgr.persist_cc_session_id(meta.id, "cc-anchor")

  record = MasterRunRecord(started_at=utc_now(), raw_log="/x/agent.raw.ndjson")
  await session_mgr.persist_master_run(meta.id, record)

  fresh = await session_mgr.get_session(meta.id)
  assert fresh is not None
  assert fresh.cc_session_id == "cc-anchor"
  assert fresh.master_run == record


@pytest.mark.asyncio
async def test_cancel_covered_turn_detaches_and_keeps_the_record(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Covered transport + persisted record: the turn is handed to the next boot."""
  cfg = _transport_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"))
  backend = _HungBackend()
  _install_backend(monkeypatch, backend)

  await _cancel_run(
      _cancel_item(cfg, meta, _persisting_callbacks(session_mgr), cfg.backends.options[0]), backend.spawned)

  backend.detach.assert_called_once_with()
  backend.terminate.assert_not_awaited()
  # Fresh-process semantics: the record the next boot reconciles is on disk.
  raw = json.loads((cfg.sessions_dir / meta.id / "metadata.json").read_text(encoding="utf-8"))
  assert raw["master_run"] is not None
  assert raw["master_run"]["pid"] == 4242
  assert raw["master_run"]["pid_start"] == "424242.0"
  assert raw["master_run"]["user_event_id"] == "evt-1"


@pytest.mark.asyncio
async def test_cancel_master_kills_a_live_detached_record(monkeypatch: pytest.MonkeyPatch) -> None:
  """In-memory miss + provably live record: the turn kept running detached
  across a graceful restart, so the recorded pid group gets the SIGTERM grace
  -> SIGKILL contract, the record is cleared, and the endpoint gets its True."""
  session_id = "session-detached"
  record = MasterRunRecord(
      pid=4242,
      pid_start="1.0",
      started_at=utc_now(),
      raw_log="/x/agent.raw.ndjson",
      user_event_id="evt-1",
  )
  meta = SessionMetadata(id=session_id, name="t", master_run=record)
  session_mgr = AsyncMock()
  kill = MagicMock()
  # Alive at the judgment, gone after the SIGTERM: the SIGKILL never goes out.
  alive = iter([True, False, False])
  monkeypatch.setattr(master_cc_queue.runs, "is_run_alive", lambda *args: next(alive))
  monkeypatch.setattr(core_process, "kill_process_group", kill)
  master_cc_state._active_procs.pop(session_id, None)

  result = await master_cc.cancel_master(session_id, meta=meta, session_mgr=session_mgr)

  assert result is True
  assert kill.call_count == 1
  assert kill.call_args_list[0].args == (4242, signal.SIGTERM)
  session_mgr.persist_master_run.assert_awaited_once_with(session_id, None)


@pytest.mark.asyncio
async def test_pid_start_missing_running_worker_never_false_failed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Legs (a)+(e): pid_start scrubbed mid-run -> the boot judges RUNNING and
  mounts a constant-true probe (death unprovable), the shim's real result
  event then closes the run through the existing completion path — no failed
  finalize at any point."""
  home = tmp_path / "home"
  proc, ids = _launch_driver(tmp_path, home, result_delay=3.0)
  _kill_driver_mid_run(proc, home, ids)

  # Scrub pid_start: the shim process is alive, but the recorded identity can
  # no longer prove death.
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
  # No exception-gate report and nothing report-only: this thread always had a
  # followable transport, so nothing but the normal completion was emitted.
  assert _recovery_reports(home, ids["session"]) == []


@pytest.mark.asyncio
async def test_uncovered_effective_alive_run_reported_not_attached(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Leg (f-mount): uncovered backend + death unverifiable -> RUNNING
  uncovered-alive; recovery emits exactly one report and mounts nothing —
  the thread is left running and untouched."""
  home = tmp_path / "home"
  # The scrubbed-input shape: no pid_start, so death is unverifiable.
  cfg, ids = await _uncovered_thread(home, name="uncovered-alive", prompt="uncovered task", pid=4242, pid_start=None)

  recovered, alive_at_reattach, _master_wakes, outcomes = await _recover(monkeypatch, home, cfg=cfg)

  assert recovered == 1
  assert outcomes == [runs.RunOutcome.RUNNING]
  # Report-only: no follow was ever mounted for this thread.
  assert not alive_at_reattach
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
  # The provably-dead pin: pid 999999 has no /proc entry, and pid_start pinned
  # at spawn is what makes that death provable rather than unverifiable.
  cfg, ids = await _uncovered_thread(
      home, name="uncovered-dead", prompt="uncovered dead task", pid=999999, pid_start="1")

  recovered, alive_at_reattach, _master_wakes, outcomes = await _recover(monkeypatch, home, cfg=cfg)

  assert recovered == 1
  assert outcomes == [runs.RunOutcome.DIED]
  assert alive_at_reattach == [False]
  _assert_failed_with_transport_reason(home, ids)
