"""Tests for startup worktree quarantine: git helper, sweep selection, and trash listing."""

import shutil
from datetime import timedelta
from pathlib import Path
from typing import Any

import pytest
from conftest import (
    build_worktree_cfg,
)

from src.core import git as git_module
from src.core import init as init_module
from src.core import init_worker_recovery as worker_recovery_module
from src.core.models import utc_now


def _make_worktree(parent: Path, name: str) -> Path:
  """Create a realistic worktree dir with caches, a .git marker, and preserved content."""
  wt = parent / name
  (wt / ".pixi").mkdir(parents=True)
  (wt / ".pixi" / "cached").write_text("regenerable", encoding="utf-8")
  (wt / ".local").mkdir()
  (wt / ".local" / "bin").write_text("regenerable", encoding="utf-8")
  (wt / "src.py").write_text("print('keep me')", encoding="utf-8")
  (wt / "diff.txt").write_text("the diff", encoding="utf-8")
  (wt / ".git").write_text("gitdir: ../repo/.git/worktrees/x\n", encoding="utf-8")
  return wt


def _thread(
    *,
    thread_id: str,
    status: str,
    worktree_path: Path,
    branch_name: str = "charliebot/task-x",
    age_days: float = 30.0,
    keep_worktree: bool = False,
    completed_at: Any = "__auto__",
) -> dict:
  if completed_at == "__auto__":
    completed_at = (utc_now() - timedelta(days=age_days)).isoformat()
  return {
      "id": thread_id,
      "session_id": "s1",
      "description": "test task",
      "status": status,
      "branch_name": branch_name,
      "repo_path": "/tmp/repo",
      "worktree_path": str(worktree_path),
      "keep_worktree": keep_worktree,
      "completed_at": completed_at,
  }


# ---------------------------------------------------------------------------
# git_quarantine_worktree helper
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_quarantine_strips_caches_moves_remainder_and_prunes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  worktree_parent = tmp_path / "worktrees"
  trash = worktree_parent / ".trash"
  wt = _make_worktree(worktree_parent, "charliebot-task-q1")

  prune_calls: list[tuple[str, str]] = []

  async def fake_prune(repo_path: str, thread_id: str) -> None:
    prune_calls.append((repo_path, thread_id))

  monkeypatch.setattr(git_module, "git_worktree_prune", fake_prune)

  dest = await git_module.git_quarantine_worktree(
      str(tmp_path / "repo"),
      wt,
      "thread-q1",
      allowed_parent=worktree_parent,
      expected_residue_name="charliebot-task-q1",
      trash_dir=trash,
  )

  # Original moved away; remainder lives in trash.
  assert not wt.exists()
  assert dest == trash / "charliebot-task-q1"
  assert dest.is_dir()
  # Regenerable caches stripped before the move.
  assert not (dest / ".pixi").exists()
  assert not (dest / ".local").exists()
  # All non-cache content preserved (code, diff, git marker).
  assert (dest / "src.py").read_text(encoding="utf-8") == "print('keep me')"
  assert (dest / "diff.txt").read_text(encoding="utf-8") == "the diff"
  assert (dest / ".git").exists()
  assert prune_calls == [(str(tmp_path / "repo"), "thread-q1")]


@pytest.mark.asyncio
async def test_quarantine_rejects_unexpected_residue_name(tmp_path: Path) -> None:
  worktree_parent = tmp_path / "worktrees"
  wt = worktree_parent / "unrelated-dir"
  wt.mkdir(parents=True)

  with pytest.raises(RuntimeError, match="does not match expected"):
    await git_module.git_quarantine_worktree(
        str(tmp_path / "repo"),
        wt,
        "thread-bad",
        allowed_parent=worktree_parent,
        expected_residue_name="charliebot-task-expected",
        trash_dir=worktree_parent / ".trash",
    )

  assert wt.exists()


# ---------------------------------------------------------------------------
# _remove_local_worktree_artifacts


# ---------------------------------------------------------------------------
# _quarantine_stale_failed_worktrees sweep
# ---------------------------------------------------------------------------


def _install_recording_quarantine(monkeypatch: pytest.MonkeyPatch) -> list[str]:
  """Replace the git quarantine helper with a recorder that simulates the move."""
  quarantined: list[str] = []

  async def fake_quarantine(
      repo_path: str,
      wt_path: Path,
      thread_id: str,
      *,
      allowed_parent: Path,
      expected_residue_name: str,
      trash_dir: Path,
  ) -> Path:
    del repo_path, thread_id, allowed_parent, expected_residue_name
    quarantined.append(str(wt_path))
    trash_dir.mkdir(parents=True, exist_ok=True)
    dest = trash_dir / wt_path.name
    shutil.move(str(wt_path), str(dest))
    return dest

  monkeypatch.setattr(worker_recovery_module, "git_quarantine_worktree", fake_quarantine)
  return quarantined


@pytest.mark.asyncio
async def test_sweep_quarantines_old_failed_worktree(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = build_worktree_cfg(tmp_path)
  parent = Path(cfg.paths.worktree_dir)
  wt = _make_worktree(parent, "charliebot-task-old")
  quarantined = _install_recording_quarantine(monkeypatch)

  await init_module._quarantine_stale_failed_worktrees(
      cfg, [_thread(thread_id="t1", status="failed", worktree_path=wt, age_days=8.0)])

  assert quarantined == [str(wt)]
  assert not wt.exists()
  assert (parent / ".trash" / "charliebot-task-old").exists()


# ---------------------------------------------------------------------------
# run_crash_recovery: interrupted-run reconcile (never kills) + sweep


# ---------------------------------------------------------------------------
# RUNNING_SCAN_WINDOW: stat-before-read gating
