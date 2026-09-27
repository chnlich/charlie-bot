"""Tests for keeping worktrees on failure (spawner + improve + review) and surfacing
success-path cleanup failures. Also pins the local-artifact name set."""

from pathlib import Path
from typing import Any
from unittest.mock import MagicMock

import pytest
from conftest import SPAWNER_SPAWN_WORKER_PATCH_TARGET, make_fake_git_create_worktree

from src.core import git as git_module
from src.core import improve_command
from src.core.improve_command import load_loop_state

# ---------------------------------------------------------------------------
# Part 5: artifact name set


# ---------------------------------------------------------------------------
# Part 1a: spawner keep-on-failure decision


# ---------------------------------------------------------------------------
# Part 4: spawner surfaces success-path cleanup failures


# ---------------------------------------------------------------------------
# Part 1b / Part 4: review keep-on-exhaustion + cleanup-failure surfacing


# ---------------------------------------------------------------------------
# Part 1c: improve loop keeps worktree on failure
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_improve_loop_keeps_worktree_on_failure(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  cfg.paths.worktree_dir = str(tmp_path / "worktrees")

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
  monkeypatch.setattr(git_module, "git_worktree_remove", fake_remove)
  monkeypatch.setattr(git_module, "git_worktree_prune", noop_async)
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

  wt_path = Path(cfg.paths.worktree_dir) / "improve-test"
  assert wt_path.exists()  # kept on failure
  assert not remove_calls  # cleanup never attempted
  state = await load_loop_state("s", 1, cfg)
  assert state is not None and state.status == "failed"
