"""Tests for the iterative improve loop orchestrator."""

from pathlib import Path
from unittest.mock import MagicMock

import pytest

from src.core import improve_command
from src.core.improve_command import (
    ImproveLoopAlreadyRunningError,
    ImproveState,
    clear_active_loop_lock,
    find_running_loop,
    load_loop_state,
    next_loop_id,
    reserve_loop_state,
    save_loop_state,
    stop_improve_loop,
)
from src.core.models import SpawnRequest


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
      "max_iterations": 3,
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


def test_save_and_load_loop_state(tmp_path: Path):
  """State round-trips through per-loop storage."""
  cfg = _make_cfg(tmp_path)
  session_id = "test-session"

  state = _make_state(1)
  save_loop_state(session_id, state, cfg)

  loaded = load_loop_state(session_id, 1, cfg)
  assert loaded is not None
  assert loaded.model_dump() == state.model_dump()
  assert (cfg.sessions_dir / session_id / "loops" / "1" / "state.json").exists()


def test_load_missing_loop_state(tmp_path: Path):
  """Missing state file returns None."""
  cfg = _make_cfg(tmp_path)
  assert load_loop_state("nonexistent", 1, cfg) is None


def test_load_corrupted_loop_state_raises(tmp_path: Path):
  """Corrupted state files fail fast."""
  cfg = _make_cfg(tmp_path)
  session_id = "corrupt-session"
  state_path = cfg.sessions_dir / session_id / "loops" / "1" / "state.json"
  state_path.parent.mkdir(parents=True, exist_ok=True)
  state_path.write_text("not valid json{{{")

  with pytest.raises(ValueError):
    load_loop_state(session_id, 1, cfg)


def test_loop_state_serialization_includes_all_fields(tmp_path: Path):
  """ImproveState persists the expanded per-loop fields."""
  cfg = _make_cfg(tmp_path)
  session_id = "serialization-session"
  state = _make_state(7, iterations_completed=2, merge_back=True)
  save_loop_state(session_id, state, cfg)

  loaded = load_loop_state(session_id, 7, cfg)
  assert loaded is not None
  assert set(loaded.model_dump().keys()) == {
      "loop_id",
      "goal",
      "max_iterations",
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


def test_next_loop_id_returns_1_when_loops_dir_missing(tmp_path: Path):
  """Sessions with no loops directory start at loop 1."""
  cfg = _make_cfg(tmp_path)
  assert next_loop_id("new-session", cfg) == 1


def test_next_loop_id_uses_highest_numeric_loop_directory(tmp_path: Path):
  """next_loop_id ignores non-numeric entries and increments the max loop id."""
  cfg = _make_cfg(tmp_path)
  loops_dir = cfg.sessions_dir / "test-session" / "loops"
  (loops_dir / "1").mkdir(parents=True, exist_ok=True)
  (loops_dir / "3").mkdir(parents=True, exist_ok=True)
  (loops_dir / "alpha").mkdir(parents=True, exist_ok=True)
  (loops_dir / "note.txt").write_text("ignore me")

  assert next_loop_id("test-session", cfg) == 4


def test_reserve_loop_state_persists_running_state_and_active_lock(tmp_path: Path):
  """Loop reservation happens before background execution begins."""
  cfg = _make_cfg(tmp_path)
  state = reserve_loop_state(
      "reserved-session",
      "optimize",
      4,
      "improve/test",
      "/tmp/repo",
      cfg,
      base_branch="main",
      merge_back=True,
      resolved_backend="codex-o3",
      resolved_model="o3",
  )

  loaded = load_loop_state("reserved-session", state.loop_id, cfg)
  assert loaded is not None
  assert loaded.model_dump() == state.model_dump()
  assert (
      cfg.sessions_dir / "reserved-session" / "loops" / "active.lock"
  ).read_text().strip() == str(state.loop_id)


def test_reserve_loop_state_raises_when_session_already_has_running_loop(tmp_path: Path):
  """Concurrent loop starts fail before they can schedule background work."""
  cfg = _make_cfg(tmp_path)
  first = reserve_loop_state("reserved-session", "optimize", 2, "improve/test", "/tmp/repo", cfg)

  with pytest.raises(ImproveLoopAlreadyRunningError, match=f"Loop {first.loop_id} is already running"):
    reserve_loop_state("reserved-session", "optimize", 2, "improve/other", "/tmp/repo", cfg)


def test_second_loop_starts_after_first_completes(tmp_path: Path):
  """After loop 1 finishes and the lock is cleared, loop 2 reserves successfully."""
  cfg = _make_cfg(tmp_path)
  session_id = "sequential-session"

  # Loop 1 reserves
  first = reserve_loop_state(session_id, "goal-1", 3, "improve/first", "/tmp/repo", cfg)
  assert first.loop_id == 1

  # Concurrent attempt blocked
  with pytest.raises(ImproveLoopAlreadyRunningError):
    reserve_loop_state(session_id, "goal-2", 3, "improve/second", "/tmp/repo", cfg)

  # Simulate loop 1 completing: mark completed + clear lock
  first.status = "completed"
  first.iterations_completed = 3
  save_loop_state(session_id, first, cfg)
  clear_active_loop_lock(session_id, cfg)

  # Loop 2 now succeeds
  second = reserve_loop_state(session_id, "goal-2", 5, "improve/second", "/tmp/repo", cfg)
  assert second.loop_id == 2
  assert second.status == "running"

  # Both loops coexist on disk
  assert load_loop_state(session_id, 1, cfg).status == "completed"
  assert load_loop_state(session_id, 2, cfg).status == "running"
  assert (cfg.sessions_dir / session_id / "loops" / "1" / "state.json").exists()
  assert (cfg.sessions_dir / session_id / "loops" / "2" / "state.json").exists()


def test_find_running_loop_returns_first_running_loop(tmp_path: Path):
  """The earliest running loop is returned when multiple loops exist."""
  cfg = _make_cfg(tmp_path)
  session_id = "running-session"
  save_loop_state(session_id, _make_state(1, status="completed"), cfg)
  save_loop_state(session_id, _make_state(2, status="running"), cfg)
  save_loop_state(session_id, _make_state(5, status="running"), cfg)

  running = find_running_loop(session_id, cfg)
  assert running is not None
  assert running.loop_id == 2


def test_find_running_loop_returns_none_when_absent(tmp_path: Path):
  """find_running_loop handles sessions with no active loop."""
  cfg = _make_cfg(tmp_path)
  session_id = "idle-session"
  save_loop_state(session_id, _make_state(1, status="completed"), cfg)

  assert find_running_loop(session_id, cfg) is None
  assert find_running_loop("missing-session", cfg) is None


@pytest.mark.asyncio
async def test_stop_active_loop_updates_running_loop_state(tmp_path: Path):
  """Stopping an active loop marks only the running loop as stopped."""
  cfg = _make_cfg(tmp_path)
  session_id = "stop-session"
  save_loop_state(session_id, _make_state(1, status="completed"), cfg)
  save_loop_state(session_id, _make_state(2, status="running"), cfg)

  result = await stop_improve_loop(session_id, cfg)
  assert result is True

  stopped = load_loop_state(session_id, 2, cfg)
  assert stopped is not None
  assert stopped.status == "stopped"
  assert find_running_loop(session_id, cfg) is None


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
  save_loop_state(session_id, _make_state(1, status="completed"), cfg)

  assert await stop_improve_loop(session_id, cfg) is False


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

  async def fake_git_fetch(repo_path: Path, remote: str, branch: str) -> tuple[bool, str]:
    del repo_path, remote, branch
    return True, ""

  async def fake_git_create_worktree(repo_path: Path, base_branch: str, branch_name: str, wt_path: Path) -> None:
    del repo_path, base_branch, branch_name
    wt_path.mkdir(parents=True, exist_ok=True)

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
  monkeypatch.setattr(improve_command, "git_fetch", fake_git_fetch)
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

  assert [(req.resolved_backend, req.resolved_model) for req in spawn_requests] == [("codex-o3", "o3"), ("codex-o3", "o3")]
  assert [req.loop_dir for req in spawn_requests] == [
      str(cfg.sessions_dir / session_id / "loops" / "1"),
      str(cfg.sessions_dir / session_id / "loops" / "1"),
  ]

  state = load_loop_state(session_id, 1, cfg)
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
