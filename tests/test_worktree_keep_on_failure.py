"""Tests for keeping worktrees on failure (spawner + improve + review) and surfacing
success-path cleanup failures. Also pins the local-artifact name set."""

from pathlib import Path
from typing import Any
from unittest.mock import MagicMock

import pytest
from conftest import SPAWNER_SPAWN_WORKER_PATCH_TARGET, make_fake_git_create_worktree

from src.core import git as git_module
from src.core import improve_command, review, spawner
from src.core.improve_command import load_loop_state
from src.core.models import ThreadMetadata


def _thread(**overrides: Any) -> ThreadMetadata:
  base: dict[str, Any] = {"session_id": "s", "description": "d"}
  base.update(overrides)
  return ThreadMetadata(**base)


# ---------------------------------------------------------------------------
# Part 5: artifact name set
# ---------------------------------------------------------------------------


def test_artifact_name_set_includes_new_caches() -> None:
  names = git_module._WORKTREE_LOCAL_ARTIFACT_NAMES
  assert ".pixi-cache" in names
  assert ".local" in names
  # Existing entries preserved.
  assert {".pixi", ".uv-cache", ".venv", "build"} <= names


# ---------------------------------------------------------------------------
# Part 1a: spawner keep-on-failure decision
# ---------------------------------------------------------------------------


def test_should_skip_cleanup_keeps_worktree_on_nonzero_exit() -> None:
  # No reviewer, no pins — a failure must still keep the worktree for debugging.
  thread = _thread(repo_path="/tmp/repo", branch_name="b", worktree_path="/tmp/wt", require_review=False)
  assert spawner._should_skip_worktree_cleanup(thread, exit_code=1) is True


def test_should_skip_cleanup_removes_on_success_without_review() -> None:
  thread = _thread(repo_path="/tmp/repo", branch_name="b", worktree_path="/tmp/wt", require_review=False)
  assert spawner._should_skip_worktree_cleanup(thread, exit_code=0) is False


def test_should_skip_cleanup_keeps_for_reviewer_handoff_on_success() -> None:
  thread = _thread(repo_path="/tmp/repo", branch_name="b", worktree_path="/tmp/wt", require_review=True)
  assert spawner._should_skip_worktree_cleanup(thread, exit_code=0) is True


def test_should_skip_cleanup_honours_existing_pins() -> None:
  assert spawner._should_skip_worktree_cleanup(_thread(keep_worktree=True), exit_code=0) is True
  assert spawner._should_skip_worktree_cleanup(_thread(skip_cleanup=True), exit_code=0) is True
  assert spawner._should_skip_worktree_cleanup(_thread(review_of="orig"), exit_code=0) is True


# ---------------------------------------------------------------------------
# Part 4: spawner surfaces success-path cleanup failures
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_cleanup_worker_directory_returns_error_when_remove_fails(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  wt = tmp_path / "worktrees" / "charliebot-task-x"
  wt.mkdir(parents=True)
  thread = _thread(id="t1", repo_path=str(tmp_path / "repo"), branch_name="charliebot/task-x", worktree_path=str(wt))

  async def fake_remove(*args: Any, **kwargs: Any) -> bool:
    return False

  monkeypatch.setattr(git_module, "git_worktree_remove", fake_remove)
  error = await spawner._cleanup_worker_directory(thread, skip_cleanup=False, worktree_parent=tmp_path / "worktrees")
  assert error is not None and "cleanup failed" in error.lower()
  assert wt.exists()


@pytest.mark.asyncio
async def test_cleanup_worker_directory_returns_error_when_remove_raises(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  wt = tmp_path / "worktrees" / "charliebot-task-x"
  wt.mkdir(parents=True)
  thread = _thread(id="t1", repo_path=str(tmp_path / "repo"), branch_name="charliebot/task-x", worktree_path=str(wt))

  async def boom(*args: Any, **kwargs: Any) -> bool:
    raise PermissionError("root-owned file")

  monkeypatch.setattr(git_module, "git_worktree_remove", boom)
  error = await spawner._cleanup_worker_directory(thread, skip_cleanup=False, worktree_parent=tmp_path / "worktrees")
  assert error is not None and "root-owned file" in error


# ---------------------------------------------------------------------------
# Part 1b / Part 4: review keep-on-exhaustion + cleanup-failure surfacing
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_retry_failed_reviewer_keeps_worktree_when_exhausted(monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = MagicMock()
  cfg.model_preference = []
  finalize_called: list[bool] = []

  async def fake_spawn_review_worker(*args: Any, **kwargs: Any) -> bool:
    return False  # all backends exhausted

  async def fake_finalize(*args: Any, **kwargs: Any) -> None:
    finalize_called.append(True)

  monkeypatch.setattr(review, "spawn_review_worker", fake_spawn_review_worker)
  monkeypatch.setattr(review, "finalize_review_chain", fake_finalize)

  thread_meta = _thread(id="rev1", review_of="orig", tried_backends=["a", "b"])
  original = _thread(id="orig")
  result = await review._retry_failed_reviewer("s", thread_meta, original, cfg, object(), object())

  assert result is False
  assert not finalize_called  # worktree kept; chain not finalized


@pytest.mark.asyncio
async def test_finalize_review_chain_returns_error_when_remove_fails(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  wt = tmp_path / "worktrees" / "charliebot-task-x"
  wt.mkdir(parents=True)
  original = _thread(
      id="t1",
      repo_path=str(tmp_path / "repo"),
      branch_name="charliebot/task-x",
      worktree_path=str(wt),
      base_branch="main")

  async def fake_remove(*args: Any, **kwargs: Any) -> bool:
    return False

  monkeypatch.setattr(git_module, "git_worktree_remove", fake_remove)
  error = await review.finalize_review_chain("s", original, worktree_parent=tmp_path / "worktrees")
  assert error is not None and "cleanup failed" in error.lower()
  assert wt.exists()


# ---------------------------------------------------------------------------
# Part 1c: improve loop keeps worktree on failure
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_improve_loop_keeps_worktree_on_failure(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  cfg.worktree_dir = str(tmp_path / "worktrees")

  class FakeSessionManager:

    async def get_session(self, session: str) -> Any:
      return MagicMock(id=session, name="x", backend="codex-o3")

    async def persist_and_broadcast(self, session: str, event: dict) -> None:
      del session, event

  class FakeThreadManager:

    async def create_thread(self, meta: Any, description: str, require_review: bool = False) -> Any:
      del meta, require_review
      return MagicMock(id="thread-1", description=description)

  async def boom_spawn_worker(*args: Any, **kwargs: Any) -> None:
    raise RuntimeError("worker blew up")

  remove_calls: list[Any] = []

  async def fake_remove(*args: Any, **kwargs: Any) -> bool:
    remove_calls.append(args)
    return True

  async def noop_async(*args: Any, **kwargs: Any) -> None:
    del args, kwargs

  monkeypatch.setattr(improve_command, "git_create_worktree", make_fake_git_create_worktree(mkdir=True))
  monkeypatch.setattr(SPAWNER_SPAWN_WORKER_PATCH_TARGET, boom_spawn_worker)
  monkeypatch.setattr(improve_command, "git_worktree_remove", fake_remove)
  monkeypatch.setattr(improve_command, "git_worktree_prune", noop_async)
  monkeypatch.setattr(improve_command, "trigger_master", noop_async)

  await improve_command.run_improve_loop(
      session_id="s",
      repo_path="/tmp/repo",
      iterations=1,
      goal="g",
      cfg=cfg,
      session_mgr=FakeSessionManager(),
      thread_mgr=FakeThreadManager(),
      base_branch="main",
      work_branch="improve/test",
      resolved_backend="codex-o3",
      resolved_model="o3",
  )

  wt_path = Path(cfg.worktree_dir) / "improve-test"
  assert wt_path.exists()  # kept on failure
  assert not remove_calls  # cleanup never attempted
  state = await load_loop_state("s", 1, cfg)
  assert state is not None and state.status == "failed"
