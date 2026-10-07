"""Tests for safe git worktree cleanup."""

import pathlib
from typing import Any

import pytest

from src.infra import git as git_module


class _FakeProc:
  """Stand-in for the subprocess handle ``git_worktree_remove`` drives.

  Each test pins ``returncode`` and overrides ``communicate``; ``kill`` raising
  is the assertion that none of the exercised paths reaches the timeout kill.
  """

  returncode: int = 0

  def kill(self) -> None:
    raise AssertionError("process should not be killed")


def _patch_git_exec(monkeypatch: pytest.MonkeyPatch, proc: _FakeProc) -> None:
  """Point ``create_subprocess_exec`` where ``git_module`` looks it up: at *proc*."""

  async def fake_create_subprocess_exec(*args: Any, **kwargs: Any) -> _FakeProc:
    del args, kwargs
    return proc

  monkeypatch.setattr(git_module.asyncio, "create_subprocess_exec", fake_create_subprocess_exec)


async def _remove_worktree(tmp_path: pathlib.Path, wt_path: pathlib.Path, expected_residue_name: str) -> bool:
  """The suite's standard remove call: the fake repo path, the shared thread id, and the worktrees parent."""
  return await git_module.git_worktree_remove(
      str(tmp_path / "repo"),
      wt_path,
      "thread-id",
      allowed_parent=tmp_path / "worktrees",
      expected_residue_name=expected_residue_name,
  )


@pytest.mark.asyncio
async def test_git_worktree_remove_does_not_delete_residue_after_git_failure(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  worktree_parent = tmp_path / "worktrees"
  wt_path = worktree_parent / "charliebot-task-leftover"
  wt_path.mkdir(parents=True)
  (wt_path / "scratch.log").write_text("leftover\n", encoding="utf-8")

  class FakeProc(_FakeProc):
    returncode = 1

    async def communicate(self) -> tuple[bytes, bytes]:
      return b"", b"not a worktree"

  _patch_git_exec(monkeypatch, FakeProc())

  removed = await _remove_worktree(tmp_path, wt_path, "charliebot-task-leftover")

  assert removed is False
  assert wt_path.exists()
  assert (wt_path / "scratch.log").exists()


@pytest.mark.asyncio
async def test_git_worktree_remove_refuses_path_outside_allowed_parent(tmp_path: pathlib.Path) -> None:
  wt_path = tmp_path / "other" / "charliebot-task-elsewhere"
  wt_path.mkdir(parents=True)

  with pytest.raises(RuntimeError, match="outside allowed parent"):
    await _remove_worktree(tmp_path, wt_path, "charliebot-task-elsewhere")

  assert wt_path.exists()


@pytest.mark.asyncio
async def test_git_worktree_remove_refuses_repo_root(tmp_path: pathlib.Path) -> None:
  worktree_parent = tmp_path / "worktrees"
  repo_path = worktree_parent / "charliebot-task-repo-root"
  repo_path.mkdir(parents=True)

  with pytest.raises(RuntimeError, match="repo root"):
    await git_module.git_worktree_remove(
        str(repo_path),
        repo_path,
        "thread-id",
        allowed_parent=worktree_parent,
        expected_residue_name="charliebot-task-repo-root",
    )

  assert repo_path.exists()


@pytest.mark.asyncio
async def test_git_worktree_remove_refuses_symlink_target(tmp_path: pathlib.Path) -> None:
  worktree_parent = tmp_path / "worktrees"
  real_target = tmp_path / "real-target"
  real_target.mkdir()
  wt_path = worktree_parent / "charliebot-task-symlink"
  worktree_parent.mkdir()
  wt_path.symlink_to(real_target, target_is_directory=True)

  with pytest.raises(RuntimeError, match="symlink"):
    await _remove_worktree(tmp_path, wt_path, "charliebot-task-symlink")

  assert wt_path.is_symlink()
  assert real_target.exists()
