"""Tests for the iterative improve loop orchestrator."""

import asyncio
import json
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock

import pytest

from src.core import improve_command
from src.core.git import BaseResolution
from src.core.improve_command import (
    ImproveLoopAlreadyRunningError,
    ImproveState,
    _quota_blocker_reason,
    clear_active_loop_lock,
    find_running_loop,
    load_loop_state,
    loop_goal_path,
    loop_plan_path,
    next_loop_id,
    read_loop_goal,
    read_loop_plan,
    reserve_loop_state,
    save_loop_state,
    stop_improve_loop,
)
from src.core.models import SpawnRequest, ThreadStatus


def _make_cfg(tmp_path: Path):
  """Create a minimal config-like object with session and worktree directories."""
  cfg = MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  cfg.worktree_dir = str(tmp_path / "worktrees")
  return cfg


def _make_state(loop_id: int, **overrides: object) -> ImproveState:
  payload = {
      "loop_id": loop_id,
      "goal": "optimize performance",
      "status": "running",
      "work_branch": "improve/test",
      "base_branch": "main",
      "repo_path": "/tmp/repo",
      "merge_back": False,
      "backend": "codex-o3",
      "model": "o3",
      "created_at": "2026-04-15T00:00:00+00:00",
      "iterations_completed": 0,
  }
  payload.update(overrides)
  return ImproveState(**payload)


@pytest.mark.asyncio
async def test_save_and_load_loop_state(tmp_path: Path):
  """State round-trips through per-loop storage."""
  cfg = _make_cfg(tmp_path)
  session_id = "test-session"

  state = _make_state(1)
  await save_loop_state(session_id, state, cfg)

  loaded = await load_loop_state(session_id, 1, cfg)
  assert loaded is not None
  assert loaded.model_dump() == state.model_dump()
  assert (cfg.sessions_dir / session_id / "loops" / "1" / "state.json").exists()


@pytest.mark.asyncio
async def test_load_missing_loop_state(tmp_path: Path):
  """Missing state file returns None."""
  cfg = _make_cfg(tmp_path)
  assert await load_loop_state("nonexistent", 1, cfg) is None


@pytest.mark.asyncio
async def test_load_corrupted_loop_state_raises(tmp_path: Path):
  """Corrupted state files fail fast."""
  cfg = _make_cfg(tmp_path)
  session_id = "corrupt-session"
  state_path = cfg.sessions_dir / session_id / "loops" / "1" / "state.json"
  state_path.parent.mkdir(parents=True, exist_ok=True)
  state_path.write_text("not valid json{{{")

  with pytest.raises(ValueError):
    await load_loop_state(session_id, 1, cfg)


@pytest.mark.asyncio
async def test_loop_state_serialization_includes_all_fields(tmp_path: Path):
  """ImproveState persists the expanded per-loop fields."""
  cfg = _make_cfg(tmp_path)
  session_id = "serialization-session"
  state = _make_state(7, iterations_completed=2, merge_back=True)
  await save_loop_state(session_id, state, cfg)

  loaded = await load_loop_state(session_id, 7, cfg)
  assert loaded is not None
  assert set(loaded.model_dump().keys()) == {
      "loop_id",
      "goal",
      "status",
      "work_branch",
      "base_branch",
      "repo_path",
      "merge_back",
      "backend",
      "model",
      "created_at",
      "iterations_completed",
  }


@pytest.mark.asyncio
async def test_next_loop_id_returns_1_when_loops_dir_missing(tmp_path: Path):
  """Sessions with no loops directory start at loop 1."""
  cfg = _make_cfg(tmp_path)
  assert await next_loop_id("new-session", cfg) == 1


@pytest.mark.asyncio
async def test_next_loop_id_uses_highest_numeric_loop_directory(tmp_path: Path):
  """next_loop_id ignores non-numeric entries and increments the max loop id."""
  cfg = _make_cfg(tmp_path)
  loops_dir = cfg.sessions_dir / "test-session" / "loops"
  (loops_dir / "1").mkdir(parents=True, exist_ok=True)
  (loops_dir / "3").mkdir(parents=True, exist_ok=True)
  (loops_dir / "alpha").mkdir(parents=True, exist_ok=True)
  (loops_dir / "note.txt").write_text("ignore me")

  assert await next_loop_id("test-session", cfg) == 4


@pytest.mark.asyncio
async def test_reserve_loop_state_persists_running_state_and_active_lock(tmp_path: Path):
  """Loop reservation happens before background execution begins."""
  cfg = _make_cfg(tmp_path)
  state = await reserve_loop_state(
      "reserved-session",
      "optimize",
      "improve/test",
      "/tmp/repo",
      cfg,
      base_branch="main",
      merge_back=True,
      resolved_backend="codex-o3",
      resolved_model="o3",
  )

  loaded = await load_loop_state("reserved-session", state.loop_id, cfg)
  assert loaded is not None
  assert loaded.model_dump() == state.model_dump()
  assert (cfg.sessions_dir / "reserved-session" / "loops" / "active.lock").read_text().strip() == str(state.loop_id)


@pytest.mark.asyncio
async def test_reserve_loop_state_raises_when_session_already_has_running_loop(tmp_path: Path):
  """Concurrent loop starts fail before they can schedule background work."""
  cfg = _make_cfg(tmp_path)
  first = await reserve_loop_state("reserved-session", "optimize", "improve/test", "/tmp/repo", cfg)

  with pytest.raises(ImproveLoopAlreadyRunningError, match=f"Loop {first.loop_id} is already running"):
    await reserve_loop_state("reserved-session", "optimize", "improve/other", "/tmp/repo", cfg)


@pytest.mark.asyncio
async def test_second_loop_starts_after_first_completes(tmp_path: Path):
  """After loop 1 finishes and the lock is cleared, loop 2 reserves successfully."""
  cfg = _make_cfg(tmp_path)
  session_id = "sequential-session"

  # Loop 1 reserves
  first = await reserve_loop_state(session_id, "goal-1", "improve/first", "/tmp/repo", cfg)
  assert first.loop_id == 1

  # Concurrent attempt blocked
  with pytest.raises(ImproveLoopAlreadyRunningError):
    await reserve_loop_state(session_id, "goal-2", "improve/second", "/tmp/repo", cfg)

  # Simulate loop 1 completing: mark completed + clear lock
  first.status = "completed"
  first.iterations_completed = 3
  await save_loop_state(session_id, first, cfg)
  await clear_active_loop_lock(session_id, cfg)

  # Loop 2 now succeeds
  second = await reserve_loop_state(session_id, "goal-2", "improve/second", "/tmp/repo", cfg)
  assert second.loop_id == 2
  assert second.status == "running"

  # Both loops coexist on disk
  first_loaded = await load_loop_state(session_id, 1, cfg)
  second_loaded = await load_loop_state(session_id, 2, cfg)
  assert first_loaded is not None and first_loaded.status == "completed"
  assert second_loaded is not None and second_loaded.status == "running"
  assert (cfg.sessions_dir / session_id / "loops" / "1" / "state.json").exists()
  assert (cfg.sessions_dir / session_id / "loops" / "2" / "state.json").exists()


@pytest.mark.asyncio
async def test_find_running_loop_returns_first_running_loop(tmp_path: Path):
  """The earliest running loop is returned when multiple loops exist."""
  cfg = _make_cfg(tmp_path)
  session_id = "running-session"
  await save_loop_state(session_id, _make_state(1, status="completed"), cfg)
  await save_loop_state(session_id, _make_state(2, status="running"), cfg)
  await save_loop_state(session_id, _make_state(5, status="running"), cfg)

  running = await find_running_loop(session_id, cfg)
  assert running is not None
  assert running.loop_id == 2


@pytest.mark.asyncio
async def test_find_running_loop_returns_none_when_absent(tmp_path: Path):
  """find_running_loop handles sessions with no active loop."""
  cfg = _make_cfg(tmp_path)
  session_id = "idle-session"
  await save_loop_state(session_id, _make_state(1, status="completed"), cfg)

  assert await find_running_loop(session_id, cfg) is None
  assert await find_running_loop("missing-session", cfg) is None


@pytest.mark.asyncio
async def test_stop_active_loop_updates_running_loop_state(tmp_path: Path):
  """Stopping an active loop marks only the running loop as stopped."""
  cfg = _make_cfg(tmp_path)
  session_id = "stop-session"
  await save_loop_state(session_id, _make_state(1, status="completed"), cfg)
  await save_loop_state(session_id, _make_state(2, status="running"), cfg)

  result = await stop_improve_loop(session_id, cfg)
  assert result is True

  stopped = await load_loop_state(session_id, 2, cfg)
  assert stopped is not None
  assert stopped.status == "stopped"
  assert await find_running_loop(session_id, cfg) is None


@pytest.mark.asyncio
async def test_stop_no_active_loop_returns_false(tmp_path: Path):
  """Stopping when no active loop exists returns False."""
  cfg = _make_cfg(tmp_path)
  assert await stop_improve_loop("nonexistent", cfg) is False


@pytest.mark.asyncio
async def test_stop_completed_loop_returns_false(tmp_path: Path):
  """Completed loops are not treated as active."""
  cfg = _make_cfg(tmp_path)
  session_id = "done-session"
  await save_loop_state(session_id, _make_state(1, status="completed"), cfg)

  assert await stop_improve_loop(session_id, cfg) is False


def test_quota_blocker_reason_detects_overage_rejected():
  """overageStatus == 'rejected' is treated as quota exhaustion (legacy shape)."""
  events = [{"type": "rate_limit_event", "rate_limit_info": {"status": "allowed", "overageStatus": "rejected"}}]
  assert _quota_blocker_reason(events) is not None


def test_quota_blocker_reason_detects_top_level_status_rejected():
  """Top-level status == 'rejected' counts even when overageStatus is 'allowed'."""
  events = [
      {
          "type": "rate_limit_event",
          "rate_limit_info":
              {
                  "status": "rejected",
                  "resetsAt": 1781119800,
                  "rateLimitType": "five_hour",
                  "overageStatus": "allowed",
                  "overageResetsAt": 1782864000,
                  "isUsingOverage": True,
                  "overageInUse": True,
              },
      }
  ]
  assert _quota_blocker_reason(events) is not None


def test_quota_blocker_reason_detects_out_of_tokens_error():
  """'out of tokens' error text is treated as provider token exhaustion."""
  events = [{
      "type": "error",
      "content": "Provider rejected the request: out of tokens for this model.",
  }]
  assert _quota_blocker_reason(events) is not None


def test_quota_blocker_reason_ignores_allowed_rate_limit_event():
  """A fully allowed rate_limit_event yields no blocker reason."""
  events = [{"type": "rate_limit_event", "rate_limit_info": {"status": "allowed", "overageStatus": "allowed"}}]
  assert _quota_blocker_reason(events) is None


class _FakeImproveSessionManager:

  def __init__(self) -> None:
    self.persisted_events: list[dict] = []

  async def get_session(self, session: str):
    return MagicMock(id=session, name="Improve", backend="codex-o3")

  async def persist_and_broadcast(self, session: str, event: dict) -> None:
    del session
    self.persisted_events.append(event)

  async def deliver_to_successor(self, session: str, event: dict) -> str:
    await self.persist_and_broadcast(session, event)
    return session


class _FakeImproveThreadManager:

  def __init__(
      self, tmp_path: Path, events_by_thread: dict[str, list[dict]], statuses: dict[str, ThreadStatus]) -> None:
    self._tmp_path = tmp_path
    self._events_by_thread = events_by_thread
    self._statuses = statuses
    self._threads: dict[str, Any] = {}

  async def create_thread(self, meta, description: str, require_review: bool = False):
    del meta, require_review
    thread_id = f"thread-{len(self._threads) + 1}"
    events_path = self._tmp_path / f"{thread_id}.jsonl"
    events = self._events_by_thread[thread_id]
    events_path.write_text("\n".join(json.dumps(ev) for ev in events) + "\n")
    thread = MagicMock(id=thread_id, description=description, branch_name=None, status=None)
    self._threads[thread_id] = thread
    return thread

  async def get_thread(self, session: str, thread_id: str):
    del session
    thread = self._threads[thread_id]
    thread.status = self._statuses[thread_id]
    return thread

  async def get_events_log_path(self, session: str, thread_id: str) -> Path:
    del session
    return self._tmp_path / f"{thread_id}.jsonl"


def _patch_improve_loop_io(monkeypatch: pytest.MonkeyPatch) -> tuple[list[SpawnRequest], list[dict]]:
  spawn_requests: list[SpawnRequest] = []
  triggered_payloads: list[dict] = []

  async def fake_spawn_worker(*args, **kwargs) -> None:
    request = kwargs["request"]
    assert isinstance(request, SpawnRequest)
    spawn_requests.append(request)

  async def fake_trigger_master(session: str, summary: str, _cfg, _session_mgr) -> None:
    del session, _cfg, _session_mgr
    triggered_payloads.append(json.loads(summary))

  async def fake_git_create_worktree(
      repo_path: Path, base_branch: str, branch_name: str, wt_path: Path) -> BaseResolution:
    del repo_path
    wt_path.mkdir(parents=True, exist_ok=True)
    return BaseResolution(canonical=base_branch, start_point=base_branch, detail="fake")

  async def fake_git_push_branch(repo_path: Path, branch_name: str) -> tuple[bool, str]:
    del repo_path, branch_name
    return True, ""

  async def fake_git_worktree_remove(
      repo_path: str,
      wt_path: Path,
      session: str,
      *,
      allowed_parent: Path,
      expected_residue_name: str,
  ) -> bool:
    del repo_path, session, allowed_parent, expected_residue_name
    if wt_path.exists():
      wt_path.rmdir()
    return True

  async def fake_git_worktree_prune(repo_path: str, session: str) -> None:
    del repo_path, session

  monkeypatch.setattr("src.core.spawner.spawn_worker", fake_spawn_worker)
  monkeypatch.setattr(improve_command, "trigger_master", fake_trigger_master)
  monkeypatch.setattr(improve_command, "git_create_worktree", fake_git_create_worktree)
  monkeypatch.setattr(improve_command, "git_push_branch", fake_git_push_branch)
  monkeypatch.setattr(improve_command, "git_worktree_remove", fake_git_worktree_remove)
  monkeypatch.setattr(improve_command, "git_worktree_prune", fake_git_worktree_prune)
  return spawn_requests, triggered_payloads


def _patch_git(monkeypatch: pytest.MonkeyPatch, *, count: str = "0", tip: str = "a" * 40) -> list[tuple]:
  """Monkeypatch the shared-worktree git helpers used for the commit delta.

  ``rev-parse HEAD`` returns ``tip`` on every call, ``rev-list --count`` returns
  ``count``, and ``diff --shortstat`` returns a fixed line. Returns the recorded
  args so tests can assert the git commands that actually ran.
  """
  calls: list[tuple] = []

  async def fake_rev_parse(repo_path: Path, ref: str) -> str:
    del repo_path, ref
    return tip

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


@pytest.mark.asyncio
async def test_run_improve_loop_stops_on_quota_blocker_without_incrementing_iteration(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _make_cfg(tmp_path)
  session_mgr = _FakeImproveSessionManager()
  thread_mgr = _FakeImproveThreadManager(
      tmp_path,
      {
          "thread-1":
              [{
                  "type": "rate_limit_event",
                  "rate_limit_info": {
                      "status": "rejected",
                      "rateLimitType": "five_hour",
                  },
              }],
      },
      {"thread-1": ThreadStatus.FAILED},
  )
  spawn_requests, triggered_payloads = _patch_improve_loop_io(monkeypatch)

  await improve_command.run_improve_loop(
      session_id="quota-session",
      repo_path="/tmp/repo",
      iterations=3,
      goal="optimize",
      cfg=cfg,
      session_mgr=session_mgr,
      thread_mgr=thread_mgr,
      base_branch="main",
      work_branch="improve/test",
      resolved_backend="codex-o3",
      resolved_model="o3",
  )

  assert len(spawn_requests) == 1
  state = await load_loop_state("quota-session", 1, cfg)
  assert state is not None
  assert state.status == "failed"
  assert state.iterations_completed == 0
  assert not (cfg.sessions_dir / "quota-session" / "loops" / "1" / "iter_0001.md").exists()
  assert not (cfg.sessions_dir / "quota-session" / "loops" / "active.lock").exists()

  failed_payloads = [event for event in session_mgr.persisted_events if event.get("type") == "improve_failed"]
  assert len(failed_payloads) == 1
  assert failed_payloads[0]["blocked_iteration"] == 1
  assert "rate-limit rejection" in failed_payloads[0]["reason"]
  assert failed_payloads[0]["iterations_completed"] == 0
  assert "No further iterations were spawned" in failed_payloads[0]["summary"]
  assert triggered_payloads[-1]["type"] == "improve_failed"
  assert "wait, switch backend, or relaunch" in triggered_payloads[-1]["instructions"]


@pytest.mark.asyncio
async def test_run_improve_loop_fails_when_session_missing(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A missing session on iteration k ends the loop as a failure, not a completion.

  ``_run_single_iteration`` raises when ``get_session`` returns None, so the loop
  exits with ``status == 'failed'``, ``iterations_completed == k-1``, broadcasts
  an ``IMPROVE_FAILED`` payload with ``failed_iteration == k``, and runs no
  merge-back landing/push on that path.
  """
  cfg = _make_cfg(tmp_path)
  session_id = "missing-session"
  session_mgr = _FakeImproveSessionManager()
  get_session_count = {"n": 0}

  class MissingOnSecondIteration(_FakeImproveSessionManager):

    async def get_session(self, session: str):
      get_session_count["n"] += 1
      if get_session_count["n"] >= 2:
        return None
      return MagicMock(id=session, name="Improve", backend="codex-o3")

  session_mgr = MissingOnSecondIteration()
  thread_mgr = _FakeImproveThreadManager(
      tmp_path,
      {
          "thread-1": [{
              "type": "result",
              "result": "first iteration completed"
          }],
      },
      {"thread-1": ThreadStatus.COMPLETED},
  )
  spawn_requests, triggered_payloads = _patch_improve_loop_io(monkeypatch)

  await improve_command.run_improve_loop(
      session_id=session_id,
      repo_path="/tmp/repo",
      iterations=3,
      goal="optimize",
      cfg=cfg,
      session_mgr=session_mgr,
      thread_mgr=thread_mgr,
      base_branch="main",
      work_branch="improve/test",
      resolved_backend="codex-o3",
      resolved_model="o3",
  )

  assert len(spawn_requests) == 1  # only iteration 1 was spawned before the miss
  state = await load_loop_state(session_id, 1, cfg)
  assert state is not None
  assert state.status == "failed"
  assert state.iterations_completed == 1  # k-1 == 1

  failed_payloads = [event for event in session_mgr.persisted_events if event.get("type") == "improve_failed"]
  assert len(failed_payloads) == 1
  assert failed_payloads[0]["failed_iteration"] == 2
  assert failed_payloads[0]["iterations_completed"] == 1
  assert failed_payloads[0].get("merge_result") is None  # no landing ran
  assert triggered_payloads[-1]["type"] == "improve_failed"
  assert triggered_payloads[-1]["failed_iteration"] == 2
  assert "Do not spawn another" in triggered_payloads[-1]["instructions"]


@pytest.mark.asyncio
async def test_run_improve_loop_keeps_non_quota_worker_failures_unchanged(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _make_cfg(tmp_path)
  session_mgr = _FakeImproveSessionManager()
  thread_mgr = _FakeImproveThreadManager(
      tmp_path,
      {
          "thread-1": [{
              "type": "error",
              "content": "unit tests failed in the rate limit feature"
          }],
          "thread-2": [{
              "type": "result",
              "result": "second iteration completed"
          }],
      },
      {
          "thread-1": ThreadStatus.FAILED,
          "thread-2": ThreadStatus.COMPLETED,
      },
  )
  spawn_requests, triggered_payloads = _patch_improve_loop_io(monkeypatch)

  await improve_command.run_improve_loop(
      session_id="ordinary-failure-session",
      repo_path="/tmp/repo",
      iterations=2,
      goal="optimize",
      cfg=cfg,
      session_mgr=session_mgr,
      thread_mgr=thread_mgr,
      base_branch="main",
      work_branch="improve/test",
      resolved_backend="codex-o3",
      resolved_model="o3",
  )

  assert len(spawn_requests) == 2
  state = await load_loop_state("ordinary-failure-session", 1, cfg)
  assert state is not None
  assert state.status == "completed"
  assert state.iterations_completed == 2
  iteration_statuses = [
      event["status"] for event in session_mgr.persisted_events if event.get("type") == "improve_iteration_completed"
  ]
  assert iteration_statuses == ["failed", "completed"]
  assert any(event.get("type") == "improve_completed" for event in session_mgr.persisted_events)
  assert triggered_payloads[-1]["type"] == "improve_completed"


@pytest.mark.asyncio
async def test_run_improve_loop_pins_resolved_backend_model(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _make_cfg(tmp_path)
  session_id = "pinned-session"
  thread_store: dict[str, object] = {}
  spawn_requests: list[SpawnRequest] = []
  persisted_events: list[dict] = []

  class FakeSessionManager:

    async def get_session(self, session: str):
      return MagicMock(id=session, name="Pinned", backend="claude-opus-4.6")

    async def persist_and_broadcast(self, session: str, event: dict) -> None:
      del session
      persisted_events.append(event)

    async def deliver_to_successor(self, session: str, event: dict) -> str:
      await self.persist_and_broadcast(session, event)
      return session

  class FakeThreadManager:

    async def create_thread(self, meta, description: str, require_review: bool = False):
      del meta, require_review
      thread_id = f"thread-{len(thread_store) + 1}"
      thread = MagicMock(id=thread_id, description=description, branch_name=None, status=None)
      thread_store[thread_id] = thread
      return thread

    async def get_thread(self, session: str, thread_id: str):
      del session
      thread = thread_store[thread_id]
      thread.status = MagicMock(value="completed")
      return thread

    async def get_events_log_path(self, session: str, thread_id: str) -> Path:
      del session, thread_id
      return tmp_path / "events.jsonl"

  async def fake_spawn_worker(*args, **kwargs) -> None:
    request = kwargs["request"]
    assert isinstance(request, SpawnRequest)
    spawn_requests.append(request)
    thread_id = kwargs["thread_id"] if "thread_id" in kwargs else args[2]
    thread_store[thread_id].branch_name = f"branch-{len(spawn_requests)}"

  async def fake_trigger_master(session: str, summary: str, _cfg, _session_mgr) -> None:
    del session, summary, _cfg, _session_mgr

  async def fake_git_create_worktree(
      repo_path: Path, base_branch: str, branch_name: str, wt_path: Path) -> BaseResolution:
    del repo_path
    wt_path.mkdir(parents=True, exist_ok=True)
    return BaseResolution(canonical=base_branch, start_point=base_branch, detail="fake")

  async def fake_git_push_branch(repo_path: Path, branch_name: str) -> tuple[bool, str]:
    del repo_path, branch_name
    return True, ""

  async def fake_git_worktree_remove(
      repo_path: str,
      wt_path: Path,
      session: str,
      *,
      allowed_parent: Path,
      expected_residue_name: str,
  ) -> bool:
    del allowed_parent, expected_residue_name
    del repo_path, session
    if wt_path.exists():
      wt_path.rmdir()
    return True

  async def fake_git_worktree_prune(repo_path: str, session: str) -> None:
    del repo_path, session

  monkeypatch.setattr("src.core.spawner.spawn_worker", fake_spawn_worker)
  monkeypatch.setattr("src.core.improve_command.trigger_master", fake_trigger_master)
  monkeypatch.setattr(improve_command, "git_create_worktree", fake_git_create_worktree)
  monkeypatch.setattr(improve_command, "git_push_branch", fake_git_push_branch)
  monkeypatch.setattr(improve_command, "git_worktree_remove", fake_git_worktree_remove)
  monkeypatch.setattr(improve_command, "git_worktree_prune", fake_git_worktree_prune)
  monkeypatch.setattr("src.core.ndjson.parse_ndjson_file", lambda path: [{"type": "result", "result": "ok"}])

  await improve_command.run_improve_loop(
      session_id=session_id,
      repo_path="/tmp/repo",
      iterations=2,
      goal="optimize",
      cfg=cfg,
      session_mgr=FakeSessionManager(),
      thread_mgr=FakeThreadManager(),
      base_branch="main",
      work_branch="improve/test",
      resolved_backend="codex-o3",
      resolved_model="o3",
  )

  assert [(req.resolved_backend, req.resolved_model) for req in spawn_requests] == [
      ("codex-o3", "o3"), ("codex-o3", "o3")
  ]
  assert [req.loop_dir for req in spawn_requests] == [
      str(cfg.sessions_dir / session_id / "loops" / "1"),
      str(cfg.sessions_dir / session_id / "loops" / "1"),
  ]

  state = await load_loop_state(session_id, 1, cfg)
  assert state is not None
  assert state.status == "completed"
  assert state.iterations_completed == 2
  assert state.work_branch == "improve/test"
  assert state.backend == "codex-o3"
  assert state.model == "o3"
  assert (cfg.sessions_dir / session_id / "loops" / "1" / "iter_0001.md").exists()
  assert (cfg.sessions_dir / session_id / "loops" / "1" / "iter_0002.md").exists()
  assert not (cfg.sessions_dir / session_id / "loops" / "active.lock").exists()
  assert any(event.get("type") == "improve_completed" for event in persisted_events)


# ---------------------------------------------------------------------------
# Live goal file (per-iteration re-read)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_reserve_loop_state_writes_goal_file(tmp_path: Path) -> None:
  """The live goal is written to loops/{id}/goal.md exactly once at reservation."""
  cfg = _make_cfg(tmp_path)
  state = await reserve_loop_state("goal-session", "make it faster", "improve/test", "/tmp/repo", cfg)

  goal_path = loop_goal_path("goal-session", state.loop_id, cfg)
  assert goal_path == cfg.sessions_dir / "goal-session" / "loops" / str(state.loop_id) / "goal.md"
  assert goal_path.read_text() == "make it faster"
  # state.json keeps the startup snapshot.
  loaded = await load_loop_state("goal-session", state.loop_id, cfg)
  assert loaded is not None and loaded.goal == "make it faster"


@pytest.mark.asyncio
async def test_reserve_loop_state_writes_optional_plan_file(tmp_path: Path) -> None:
  """plan.md is written only when the caller provides plan content."""
  cfg = _make_cfg(tmp_path)
  state = await reserve_loop_state(
      "plan-session",
      "make it faster",
      "improve/test",
      "/tmp/repo",
      cfg,
      plan="1. measure bottleneck",
  )

  plan_path = loop_plan_path("plan-session", state.loop_id, cfg)
  assert plan_path == cfg.sessions_dir / "plan-session" / "loops" / str(state.loop_id) / "plan.md"
  assert plan_path.read_text() == "1. measure bottleneck"


@pytest.mark.asyncio
async def test_read_loop_goal_raises_when_missing(tmp_path: Path) -> None:
  """A missing goal.md is a hard failure — no fallback to a snapshot."""
  loop_dir = tmp_path / "loops" / "1"
  loop_dir.mkdir(parents=True)
  with pytest.raises(RuntimeError, match="goal file missing"):
    await read_loop_goal(loop_dir)


@pytest.mark.asyncio
async def test_read_loop_plan_returns_none_when_missing(tmp_path: Path) -> None:
  """A missing plan.md means the loop is using thin-goal behavior."""
  loop_dir = tmp_path / "loops" / "1"
  loop_dir.mkdir(parents=True)
  assert await read_loop_plan(loop_dir) is None


@pytest.mark.asyncio
async def test_run_improve_loop_rereads_edited_goal_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Editing goal.md mid-loop steers iteration N>1's worker prompt."""
  cfg = _make_cfg(tmp_path)
  session_mgr = _FakeImproveSessionManager()
  thread_mgr = _FakeImproveThreadManager(
      tmp_path,
      {
          "thread-1": [{
              "type": "result",
              "result": "iter1 done"
          }],
          "thread-2": [{
              "type": "result",
              "result": "iter2 done"
          }],
      },
      {
          "thread-1": ThreadStatus.COMPLETED,
          "thread-2": ThreadStatus.COMPLETED
      },
  )
  _patch_improve_loop_io(monkeypatch)

  descriptions: list[str] = []

  async def capturing_spawn_worker(*args, **kwargs) -> None:
    descriptions.append(args[1])
    request = kwargs["request"]
    assert isinstance(request, SpawnRequest)
    if request.iteration_number == 1:
      # Simulate the user editing the live goal between iterations.
      (Path(request.loop_dir) / "goal.md").write_text("edited goal")

  monkeypatch.setattr("src.core.spawner.spawn_worker", capturing_spawn_worker)

  await improve_command.run_improve_loop(
      session_id="edit-session",
      repo_path="/tmp/repo",
      iterations=2,
      goal="original goal",
      cfg=cfg,
      session_mgr=session_mgr,
      thread_mgr=thread_mgr,
      base_branch="main",
      work_branch="improve/test",
      resolved_backend="codex-o3",
      resolved_model="o3",
  )

  assert len(descriptions) == 2
  assert "Goal: original goal" in descriptions[0]
  assert "Goal: edited goal" in descriptions[1]
  # state.json's goal field stays the startup snapshot.
  state = await load_loop_state("edit-session", 1, cfg)
  assert state is not None
  assert state.goal == "original goal"
  assert state.iterations_completed == 2


@pytest.mark.asyncio
async def test_run_improve_loop_injects_previous_summaries_in_next_description(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Iteration N>1 sees summaries sourced from earlier iteration report files."""
  cfg = _make_cfg(tmp_path)
  session_mgr = _FakeImproveSessionManager()
  thread_mgr = _FakeImproveThreadManager(
      tmp_path,
      {
          "thread-1": [{
              "type": "result",
              "result": "iter1 event text that must NOT be the summary"
          }],
          "thread-2": [{
              "type": "result",
              "result": "iter2 done"
          }],
      },
      {
          "thread-1": ThreadStatus.COMPLETED,
          "thread-2": ThreadStatus.COMPLETED
      },
  )
  _patch_improve_loop_io(monkeypatch)
  _patch_git(monkeypatch, count="1")

  iter1_report = (
      "## Iter 1 — completed\n"
      "### What Changed\n- did the work\n"
      "### Commits\n- 1111111 did the work\n")
  descriptions: list[str] = []

  async def capturing_spawn_worker(*args, **kwargs) -> None:
    request = kwargs["request"]
    descriptions.append(args[1])
    if request.iteration_number == 1:
      (Path(request.loop_dir) / f'iter_{request.iteration_number:04d}.md').write_text(iter1_report)

  monkeypatch.setattr("src.core.spawner.spawn_worker", capturing_spawn_worker)

  await improve_command.run_improve_loop(
      session_id="summary-session",
      repo_path="/tmp/repo",
      iterations=2,
      goal="original goal",
      cfg=cfg,
      session_mgr=session_mgr,
      thread_mgr=thread_mgr,
      base_branch="main",
      work_branch="improve/test",
      resolved_backend="codex-o3",
      resolved_model="o3",
  )

  assert len(descriptions) == 2
  assert "Previous iteration summaries:" not in descriptions[0]
  assert "iter1 event text that must NOT be the summary" not in descriptions[1]
  assert "Previous iteration summaries:\n## Iter 1 — completed" in descriptions[1]


@pytest.mark.asyncio
async def test_run_improve_loop_injects_plan_when_provided(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Workers see plan.md content when the loop was launched with a plan."""
  cfg = _make_cfg(tmp_path)
  session_mgr = _FakeImproveSessionManager()
  thread_mgr = _FakeImproveThreadManager(
      tmp_path,
      {"thread-1": [{
          "type": "result",
          "result": "iter1 done"
      }]},
      {"thread-1": ThreadStatus.COMPLETED},
  )
  _patch_improve_loop_io(monkeypatch)

  descriptions: list[str] = []

  async def capturing_spawn_worker(*args, **kwargs) -> None:
    del kwargs
    descriptions.append(args[1])

  monkeypatch.setattr("src.core.spawner.spawn_worker", capturing_spawn_worker)

  await improve_command.run_improve_loop(
      session_id="plan-injection-session",
      repo_path="/tmp/repo",
      iterations=1,
      goal="original goal",
      plan="1. fix largest measured bottleneck",
      cfg=cfg,
      session_mgr=session_mgr,
      thread_mgr=thread_mgr,
      base_branch="main",
      work_branch="improve/test",
      resolved_backend="codex-o3",
      resolved_model="o3",
  )

  assert len(descriptions) == 1
  assert "Plan:\n1. fix largest measured bottleneck" in descriptions[0]


@pytest.mark.asyncio
async def test_run_improve_loop_works_without_plan_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Omitting plan.md preserves the original thin-goal loop behavior."""
  cfg = _make_cfg(tmp_path)
  session_mgr = _FakeImproveSessionManager()
  thread_mgr = _FakeImproveThreadManager(
      tmp_path,
      {"thread-1": [{
          "type": "result",
          "result": "iter1 done"
      }]},
      {"thread-1": ThreadStatus.COMPLETED},
  )
  _patch_improve_loop_io(monkeypatch)

  descriptions: list[str] = []

  async def capturing_spawn_worker(*args, **kwargs) -> None:
    del kwargs
    descriptions.append(args[1])

  monkeypatch.setattr("src.core.spawner.spawn_worker", capturing_spawn_worker)

  await improve_command.run_improve_loop(
      session_id="no-plan-session",
      repo_path="/tmp/repo",
      iterations=1,
      goal="original goal",
      cfg=cfg,
      session_mgr=session_mgr,
      thread_mgr=thread_mgr,
      base_branch="main",
      work_branch="improve/test",
      resolved_backend="codex-o3",
      resolved_model="o3",
  )

  assert len(descriptions) == 1
  assert "Goal: original goal" in descriptions[0]
  assert "Plan:" not in descriptions[0]
  assert await read_loop_plan(cfg.sessions_dir / "no-plan-session" / "loops" / "1") is None


@pytest.mark.asyncio
async def test_run_improve_loop_rereads_edited_plan_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Editing plan.md mid-loop steers iteration N>1's worker prompt."""
  cfg = _make_cfg(tmp_path)
  session_mgr = _FakeImproveSessionManager()
  thread_mgr = _FakeImproveThreadManager(
      tmp_path,
      {
          "thread-1": [{
              "type": "result",
              "result": "iter1 done"
          }],
          "thread-2": [{
              "type": "result",
              "result": "iter2 done"
          }],
      },
      {
          "thread-1": ThreadStatus.COMPLETED,
          "thread-2": ThreadStatus.COMPLETED
      },
  )
  _patch_improve_loop_io(monkeypatch)

  descriptions: list[str] = []

  async def capturing_spawn_worker(*args, **kwargs) -> None:
    descriptions.append(args[1])
    request = kwargs["request"]
    assert isinstance(request, SpawnRequest)
    if request.iteration_number == 1:
      (Path(request.loop_dir) / "plan.md").write_text("2. edited lever")

  monkeypatch.setattr("src.core.spawner.spawn_worker", capturing_spawn_worker)

  await improve_command.run_improve_loop(
      session_id="edit-plan-session",
      repo_path="/tmp/repo",
      iterations=2,
      goal="original goal",
      plan="1. initial lever",
      cfg=cfg,
      session_mgr=session_mgr,
      thread_mgr=thread_mgr,
      base_branch="main",
      work_branch="improve/test",
      resolved_backend="codex-o3",
      resolved_model="o3",
  )

  assert len(descriptions) == 2
  assert "Plan:\n1. initial lever" in descriptions[0]
  assert "Plan:\n2. edited lever" in descriptions[1]


@pytest.mark.asyncio
async def test_run_improve_loop_fails_when_goal_file_missing_mid_loop(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A goal.md removed mid-loop fails the loop loudly instead of falling back."""
  cfg = _make_cfg(tmp_path)
  session_mgr = _FakeImproveSessionManager()
  thread_mgr = _FakeImproveThreadManager(
      tmp_path,
      {"thread-1": [{
          "type": "result",
          "result": "iter1 done"
      }]},
      {"thread-1": ThreadStatus.COMPLETED},
  )
  _patch_improve_loop_io(monkeypatch)

  async def deleting_spawn_worker(*args, **kwargs) -> None:
    del args
    request = kwargs["request"]
    assert isinstance(request, SpawnRequest)
    if request.iteration_number == 1:
      (Path(request.loop_dir) / "goal.md").unlink()

  monkeypatch.setattr("src.core.spawner.spawn_worker", deleting_spawn_worker)

  await improve_command.run_improve_loop(
      session_id="missing-goal-session",
      repo_path="/tmp/repo",
      iterations=2,
      goal="original goal",
      cfg=cfg,
      session_mgr=session_mgr,
      thread_mgr=thread_mgr,
      base_branch="main",
      work_branch="improve/test",
      resolved_backend="codex-o3",
      resolved_model="o3",
  )

  state = await load_loop_state("missing-goal-session", 1, cfg)
  assert state is not None
  assert state.status == "failed"
  assert state.iterations_completed == 1
  assert not (cfg.sessions_dir / "missing-goal-session" / "loops" / "active.lock").exists()


# ---------------------------------------------------------------------------
# Invalid-iteration gate: validity, quarantine, and wake-payload fidelity
# ---------------------------------------------------------------------------


def _valid_report(iteration: int, *, em_dash: bool = True) -> str:
  dash = "\u2014" if em_dash else "-"
  return f"## Iter {iteration} {dash} completed\n### What Changed\n- did the work\n### Commits\n- 1111111 did the work\n"


def _nothing_done_report(iteration: int) -> str:
  return (
      f"## Iter {iteration} \u2014 completed\n"
      "### What Changed\n- investigated, nothing changed\n"
      "### Commits\n- none \u2014 zero progress, no regression introduced\n")


async def _pump_event_loop() -> None:
  for _ in range(20):
    await asyncio.sleep(0)


async def _gate_loop(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    iterations: int,
    reports: dict[int, str | None],
    count: str = "1",
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

  monkeypatch.setattr("src.core.spawner.spawn_worker", writing_spawn_worker)

  await improve_command.run_improve_loop(
      session_id="gate-session",
      repo_path="/tmp/repo",
      iterations=iterations,
      goal="original goal",
      cfg=cfg,
      session_mgr=session_mgr,
      thread_mgr=thread_mgr,
      base_branch="main",
      work_branch="improve/test",
      resolved_backend="codex-o3",
      resolved_model="o3",
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
@pytest.mark.parametrize(
    "report,count,expected_valid,expected_reason",
    [
        (None, "1", False, "missing_report"),
        ("no heading here at all\n", "1", False, "malformed_report"),
        ("## Iter 1 \u2014 completed\n### What Changed\n- x\n", "0", False, "no_commit_no_verdict"),
        (_valid_report(1), "1", True, None),
        (_nothing_done_report(1), "0", True, None),
        (_valid_report(1, em_dash=False), "1", True, None),
    ],
)
async def test_iteration_validity_matrix(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    report: str | None,
    count: str,
    expected_valid: bool,
    expected_reason: str | None,
) -> None:
  """Each validity path yields the expected verdict on broadcast and trigger payloads."""
  _cfg, session_mgr, triggered_payloads, _descs = await _gate_loop(
      tmp_path, monkeypatch, iterations=1, reports={1: report}, count=count)

  broadcast = _iter_broadcast(session_mgr, 1)
  trigger = _iter_trigger(triggered_payloads, 1)
  assert broadcast["report_valid"] is expected_valid
  assert broadcast["invalid_reason"] == expected_reason
  assert trigger["report_valid"] is expected_valid
  assert trigger["invalid_reason"] == expected_reason
  assert set(broadcast.keys()) >= {"report_path", "report_valid", "invalid_reason", "tip", "commits_added", "diffstat"}
  assert set(trigger.keys()) >= {"report_path", "report_valid", "invalid_reason", "tip", "commits_added", "diffstat"}


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


@pytest.mark.asyncio
async def test_invalid_placeholder_feeds_next_worker_description(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The placeholder (not event text) reaches the next worker's description."""
  _cfg, session_mgr, _triggered_payloads, descriptions = await _gate_loop(
      tmp_path,
      monkeypatch,
      iterations=2,
      reports={1: "## Iter 1 \u2014 completed\n### What Changed\n- x\n"},  # no_commit_no_verdict
      count="0",
  )

  assert len(descriptions) == 2
  # Iteration 2's worker sees only the placeholder, never the event text.
  assert "Iteration 1 INVALID (no_commit_no_verdict)" in descriptions[1]
  assert "event text iter1" not in descriptions[1]

  # The placeholder also replaces the event text in both the broadcast and trigger.
  broadcast = _iter_broadcast(session_mgr, 1)
  assert "INVALID (no_commit_no_verdict)" in broadcast["summary"]
  assert "event text iter1" not in broadcast["summary"]


@pytest.mark.asyncio
async def test_wake_payload_fidelity(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The per-iteration payload carries report-path/fidelity fields and a report-head summary."""
  cfg = _make_cfg(tmp_path)
  loop_dir = cfg.sessions_dir / "gate-session" / "loops" / "1"
  report_text = "## Iter 1 \u2014 completed\n### What Changed\n- work\n### Commits\n- 1111111 work\n"
  _cfg, session_mgr, triggered_payloads, _descriptions = await _gate_loop(
      tmp_path, monkeypatch, iterations=1, reports={1: report_text}, count="1")

  trigger = _iter_trigger(triggered_payloads, 1)
  expected_report_path = str(loop_dir / "iter_0001.md")
  assert trigger["report_path"] == expected_report_path
  assert Path(trigger["report_path"]).is_absolute()
  assert trigger["report_valid"] is True
  assert trigger["invalid_reason"] is None
  assert trigger["tip"] == "a" * 40
  assert trigger["commits_added"] == 1
  assert trigger["diffstat"]
  # Summary is sourced from the report file head, not the event stream.
  assert trigger["summary"].startswith("## Iter 1")
  assert "event text iter1" not in trigger["summary"]

  broadcast = _iter_broadcast(session_mgr, 1)
  assert broadcast["report_path"] == expected_report_path
  assert broadcast["diffstat"]


@pytest.mark.asyncio
async def test_diffstat_empty_when_zero_commits(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """diffstat is '' and commits_added is 0 when no commits were added this iteration."""
  _cfg, _session_mgr, triggered_payloads, _descriptions = await _gate_loop(
      tmp_path, monkeypatch, iterations=1, reports={1: _nothing_done_report(1)}, count="0")

  trigger = _iter_trigger(triggered_payloads, 1)
  assert trigger["commits_added"] == 0
  assert trigger["diffstat"] == ""
  assert trigger["report_valid"] is True


@pytest.mark.asyncio
async def test_fallback_report_write_carries_marker(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A missing worker report gets a fallback write prefixed by the marker and a missing verdict."""
  cfg = _make_cfg(tmp_path)
  _cfg, session_mgr, triggered_payloads, _descriptions = await _gate_loop(
      tmp_path, monkeypatch, iterations=1, reports={1: None}, count="1")

  marker = "<!-- runner fallback: worker wrote no report -->"
  report_path = cfg.sessions_dir / "gate-session" / "loops" / "1" / "iter_0001.md"
  assert report_path.exists()
  text = report_path.read_text()
  assert text.startswith(marker)

  # Validity was decided as missing_report BEFORE the fallback write.
  trigger = _iter_trigger(triggered_payloads, 1)
  assert trigger["report_valid"] is False
  assert trigger["invalid_reason"] == "missing_report"
  assert marker not in trigger["summary"]
  assert marker in text
