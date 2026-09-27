"""Tests for the iterative improve loop orchestrator."""

import asyncio
import json
from collections.abc import Callable
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock

import pytest
from conftest import (
    SPAWNER_SPAWN_WORKER_PATCH_TARGET,
    SuccessorDeliveryShim,
    patch_improve_git_ops,
)

from src.core import improve_command
from src.core.improve_command import (
    ImproveLoopAlreadyRunningError,
    _failed_iteration_judgments,
    load_loop_state,
    reserve_loop_state,
)
from src.core.models import SessionMetadata, SpawnRequest, ThreadStatus


def _make_cfg(tmp_path: Path) -> MagicMock:
  """Create a minimal config-like object with session and worktree directories."""
  cfg = MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  cfg.paths.worktree_dir = str(tmp_path / "worktrees")
  return cfg


@pytest.mark.asyncio
async def test_reserve_loop_state_raises_when_session_already_has_running_loop(tmp_path: Path) -> None:
  """Concurrent loop starts fail before they can schedule background work."""
  cfg = _make_cfg(tmp_path)
  first = await reserve_loop_state("reserved-session", "optimize", "improve/test", "/tmp/repo", cfg)

  with pytest.raises(ImproveLoopAlreadyRunningError, match=f"Loop {first.loop_id} is already running"):
    await reserve_loop_state("reserved-session", "optimize", "improve/other", "/tmp/repo", cfg)


def test_quota_blocker_reason_ignores_allowed_rate_limit_event() -> None:
  """A fully allowed rate_limit_event yields no blocker reason."""
  events = [{"type": "rate_limit_event", "rate_limit_info": {"status": "allowed", "overageStatus": "allowed"}}]
  assert _failed_iteration_judgments(iter(reversed(events)), 1, "failed")[0] is None


class _FakeImproveSessionManager(SuccessorDeliveryShim):

  def __init__(self) -> None:
    self.persisted_events: list[dict] = []

  async def get_session(self, session: str) -> MagicMock:
    return MagicMock(id=session, name="Improve", backend="codex-o3")

  async def persist_and_broadcast(self, session: str, event: dict) -> None:
    del session
    self.persisted_events.append(event)


class _FakeImproveThreadManager:

  def __init__(
      self, tmp_path: Path, events_by_thread: dict[str, list[dict]], statuses: dict[str, ThreadStatus]) -> None:
    self._tmp_path = tmp_path
    self._events_by_thread = events_by_thread
    self._statuses = statuses
    self._threads: dict[str, Any] = {}

  async def create_thread(self, meta: SessionMetadata, description: str, require_review: bool = False) -> MagicMock:
    del meta, require_review
    thread_id = f"thread-{len(self._threads) + 1}"
    events_path = self._tmp_path / f"{thread_id}.jsonl"
    events = self._events_by_thread[thread_id]
    events_path.write_text("\n".join(json.dumps(ev) for ev in events) + "\n")
    thread = MagicMock(id=thread_id, description=description, branch_name=None, status=None)
    self._threads[thread_id] = thread
    return thread

  async def get_thread(self, session: str, thread_id: str) -> MagicMock:
    del session
    thread = self._threads[thread_id]
    thread.status = self._statuses[thread_id]
    return thread

  async def get_events_log_path(self, session: str, thread_id: str) -> Path:
    del session
    return self._tmp_path / f"{thread_id}.jsonl"


def _completed_thread_mgr(tmp_path: Path, iterations: int, results: dict[int, str] | None) -> _FakeImproveThreadManager:
  """Thread manager running `iterations` threads that complete with one result event each.

  results overrides the canned `"iterN done"` text by 1-based iteration number, for tests
  that must pin a distinct event payload.
  """
  overrides = results or {}
  events = {
      f"thread-{n}": [{
          "type": "result",
          "result": overrides.get(n, f"iter{n} done")
      }] for n in range(1, iterations + 1)
  }
  statuses = {f"thread-{n}": ThreadStatus.COMPLETED for n in range(1, iterations + 1)}
  return _FakeImproveThreadManager(tmp_path, events, statuses)


def _capture_descriptions(
    monkeypatch: pytest.MonkeyPatch,
    on_spawn: Callable[[SpawnRequest], None] | None,
) -> list[str]:
  """Record each worker description, replacing the stub _patch_improve_loop_io installed.

  on_spawn runs after the description is recorded, receiving the SpawnRequest, so a test
  can edit or remove loop files at a chosen iteration.
  """
  descriptions: list[str] = []

  async def capturing_spawn_worker(*args: Any, **kwargs: Any) -> None:
    request = kwargs["request"]
    assert isinstance(request, SpawnRequest)
    descriptions.append(args[1])
    if on_spawn is not None:
      on_spawn(request)

  monkeypatch.setattr(SPAWNER_SPAWN_WORKER_PATCH_TARGET, capturing_spawn_worker)
  return descriptions


def _patch_improve_loop_io(monkeypatch: pytest.MonkeyPatch) -> tuple[list[SpawnRequest], list[dict]]:
  spawn_requests: list[SpawnRequest] = []
  triggered_payloads: list[dict] = []

  async def fake_spawn_worker(*args: Any, **kwargs: Any) -> None:
    request = kwargs["request"]
    assert isinstance(request, SpawnRequest)
    spawn_requests.append(request)

  async def fake_trigger_master(session: str, summary: str, _cfg: Any, _session_mgr: Any, _etype: str) -> None:
    del session, _cfg, _session_mgr, _etype
    triggered_payloads.append(json.loads(summary))

  monkeypatch.setattr(SPAWNER_SPAWN_WORKER_PATCH_TARGET, fake_spawn_worker)
  monkeypatch.setattr(improve_command, "trigger_master", fake_trigger_master)
  patch_improve_git_ops(monkeypatch)
  return spawn_requests, triggered_payloads


def _patch_git(monkeypatch: pytest.MonkeyPatch, *, count: str) -> list[tuple]:
  """Monkeypatch the shared-worktree git helpers used for the commit delta.

  ``rev-parse HEAD`` returns the same 40-``a`` sha on every call, ``rev-list --count`` returns
  ``count``, and ``diff --shortstat`` returns a fixed line. Returns the recorded
  args so tests can assert the git commands that actually ran.
  """
  calls: list[tuple] = []

  async def fake_rev_parse(repo_path: Path, ref: str) -> str:
    del repo_path, ref
    return "a" * 40

  async def fake_stdout(repo_path: Path, *args: str, **_kwargs: object) -> tuple[bool, str, str]:
    del repo_path, _kwargs
    calls.append(args)
    if args[:2] == ("rev-list", "--count"):
      return True, count, ""
    if args[:2] == ("diff", "--shortstat"):
      return True, "1 file changed, 1 insertion(+)", ""
    return True, "", ""

  monkeypatch.setattr(improve_command, "_git_rev_parse", fake_rev_parse)
  monkeypatch.setattr(improve_command, "_git_stdout", fake_stdout)
  return calls


def _description_rig(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    iterations: int,
    *,
    results: dict[int, str] | None = None,
    on_spawn: Callable[[SpawnRequest], None] | None = None,
) -> tuple[Any, _FakeImproveSessionManager, _FakeImproveThreadManager, list[str]]:
  """Wire the description-capture loop rig; returns (cfg, session_mgr, thread_mgr, descriptions).

  ``descriptions`` holds each spawned worker's full description in iteration order; ``on_spawn``
  runs after the description is recorded (see ``_capture_descriptions``), ``results`` pins the
  per-iteration result payloads (see ``_completed_thread_mgr``).
  """
  cfg = _make_cfg(tmp_path)
  session_mgr = _FakeImproveSessionManager()
  thread_mgr = _completed_thread_mgr(tmp_path, iterations, results=results)
  _patch_improve_loop_io(monkeypatch)
  descriptions = _capture_descriptions(monkeypatch, on_spawn=on_spawn)
  return cfg, session_mgr, thread_mgr, descriptions


async def _run_loop(
    *,
    session_id: str,
    iterations: int,
    goal: str,
    cfg: Any,
    session_mgr: Any,
    thread_mgr: Any,
    plan: str | None = None,
) -> None:
  """run_improve_loop on the call tail the run-path tests share: /tmp/repo, improve/test off
  main, backend codex-o3/o3; plan defaults to run_improve_loop's own None."""
  await improve_command.run_improve_loop(
      session_id=session_id,
      repo_path="/tmp/repo",
      iterations=iterations,
      goal=goal,
      plan=plan,
      cfg=cfg,
      session_mgr=session_mgr,
      thread_mgr=thread_mgr,
      base_branch="main",
      work_branch="improve/test",
      resolved_backend="codex-o3",
      resolved_model="o3",
  )


# ---------------------------------------------------------------------------
# Live goal file (per-iteration re-read)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("plan", "edited_name", "edited_body", "first_marker", "second_marker"),
    [
        pytest.param(None, "goal.md", "edited goal", "Goal: original goal", "Goal: edited goal", id="goal"),
        pytest.param(
            "1. initial lever",
            "plan.md",
            "2. edited lever",
            "Plan:\n1. initial lever",
            "Plan:\n2. edited lever",
            id="plan"),
    ],
)
async def test_run_improve_loop_rereads_edited_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, plan: str | None, edited_name: str, edited_body: str,
    first_marker: str, second_marker: str) -> None:
  """Editing goal.md or plan.md mid-loop steers iteration N>1's worker prompt."""

  def edit_file_after_iter1(request: SpawnRequest) -> None:
    # Simulate the user editing the live file between iterations.
    if request.iteration_number == 1:
      (Path(request.loop_dir) / edited_name).write_text(edited_body)

  cfg, session_mgr, thread_mgr, descriptions = _description_rig(
      tmp_path, monkeypatch, 2, on_spawn=edit_file_after_iter1)

  await _run_loop(
      session_id="edit-file-session",
      iterations=2,
      goal="original goal",
      plan=plan,
      cfg=cfg,
      session_mgr=session_mgr,
      thread_mgr=thread_mgr,
  )

  assert len(descriptions) == 2
  assert first_marker in descriptions[0]
  assert second_marker in descriptions[1]
  # state.json's goal field stays the startup snapshot.
  state = await load_loop_state("edit-file-session", 1, cfg)
  assert state is not None
  assert state.goal == "original goal"


@pytest.mark.asyncio
async def test_run_improve_loop_fails_when_goal_file_missing_mid_loop(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A goal.md removed mid-loop fails the loop loudly instead of falling back."""

  def delete_goal_after_iter1(request: SpawnRequest) -> None:
    if request.iteration_number == 1:
      (Path(request.loop_dir) / "goal.md").unlink()

  cfg, session_mgr, thread_mgr, _ = _description_rig(tmp_path, monkeypatch, 1, on_spawn=delete_goal_after_iter1)

  await _run_loop(
      session_id="missing-goal-session",
      iterations=2,
      goal="original goal",
      cfg=cfg,
      session_mgr=session_mgr,
      thread_mgr=thread_mgr,
  )

  state = await load_loop_state("missing-goal-session", 1, cfg)
  assert state is not None
  assert state.status == "failed"
  assert not (cfg.sessions_dir / "missing-goal-session" / "loops" / "active.lock").exists()


# ---------------------------------------------------------------------------
# Invalid-iteration gate: validity, quarantine, and wake-payload fidelity
# ---------------------------------------------------------------------------


async def _pump_event_loop() -> None:
  for _ in range(20):
    await asyncio.sleep(0)


async def _gate_loop(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    iterations: int,
    reports: dict[int, str | None],
    count: str,
) -> tuple[Any, _FakeImproveSessionManager, list[dict], list[str]]:
  """Run a loop writing the given per-iteration reports.

  Returns (cfg, session_mgr, triggered_payloads, descriptions). ``reports`` maps an
  iteration number to its report text (None = worker wrote no report that iteration);
  ``descriptions`` holds each spawned worker's full description in iteration order.
  """
  cfg = _make_cfg(tmp_path)
  session_mgr = _FakeImproveSessionManager()
  events = {f"thread-{k}": [{"type": "result", "result": f"event text iter{k}"}] for k in range(1, iterations + 1)}
  statuses = {f"thread-{k}": ThreadStatus.COMPLETED for k in range(1, iterations + 1)}
  thread_mgr = _FakeImproveThreadManager(tmp_path, events, statuses)
  _spawns, triggered_payloads = _patch_improve_loop_io(monkeypatch)
  _patch_git(monkeypatch, count=count)

  descriptions: list[str] = []

  async def writing_spawn_worker(*args: Any, **kwargs: Any) -> None:
    description = args[1]
    request = kwargs["request"]
    assert isinstance(request, SpawnRequest)
    descriptions.append(description)
    report = reports.get(request.iteration_number)
    if report is not None:
      (Path(request.loop_dir) / f'iter_{request.iteration_number:04d}.md').write_text(report)

  monkeypatch.setattr(SPAWNER_SPAWN_WORKER_PATCH_TARGET, writing_spawn_worker)

  await _run_loop(
      session_id="gate-session",
      iterations=iterations,
      goal="original goal",
      cfg=cfg,
      session_mgr=session_mgr,
      thread_mgr=thread_mgr,
  )
  await _pump_event_loop()
  return cfg, session_mgr, triggered_payloads, descriptions


def _iter_broadcast(session_mgr: _FakeImproveSessionManager, iteration: int) -> dict:
  return next(
      p for p in session_mgr.persisted_events
      if p.get("type") == "improve_iteration_completed" and p.get("iteration") == iteration)


def _iter_trigger(payloads: list[dict], iteration: int) -> dict:
  return next(p for p in payloads if p.get("type") == "improve_iteration_completed" and p.get("iteration") == iteration)


@pytest.mark.asyncio
async def test_invalid_iteration_is_quarantined_across_all_channels(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An invalid iteration's event summary leaks nowhere; the placeholder is used everywhere."""
  _cfg, session_mgr, triggered_payloads, _descriptions = await _gate_loop(
      tmp_path,
      monkeypatch,
      iterations=2,
      # Iter 1: heading present, zero commits, no `### Commits` — no_commit_no_verdict.
      reports={1: "## Iter 1 \u2014 completed\n### What Changed\n- x\n"},
      count="0",
  )

  # The event-extracted text must not leak into the broadcast event.
  assert all("event text iter1" not in str(p) for p in session_mgr.persisted_events)
  iter_trigger = _iter_trigger(triggered_payloads, 1)
  assert "event text iter1" not in iter_trigger["summary"]

  broadcast = _iter_broadcast(session_mgr, 1)
  assert broadcast["report_valid"] is False
  assert broadcast["invalid_reason"] == "no_commit_no_verdict"
  placeholder_snip = "INVALID (no_commit_no_verdict)"
  assert placeholder_snip in broadcast["summary"]
  assert placeholder_snip in iter_trigger["summary"]

  # The invalid iteration still consumes a slot and still wakes the master.
  assert iter_trigger["iteration"] == 1
  state = await load_loop_state("gate-session", 1, _cfg)
  assert state is not None
