"""Tests for safe git worktree cleanup."""

from pathlib import Path
from typing import Any

import pytest

from src.core import git as git_module


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


@pytest.mark.asyncio
async def test_git_worktree_remove_does_not_delete_residue_after_git_failure(
    tmp_path: Path,
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

  removed = await git_module.git_worktree_remove(
      str(tmp_path / "repo"),
      wt_path,
      "thread-id",
      allowed_parent=worktree_parent,
      expected_residue_name="charliebot-task-leftover",
  )

  assert removed is False
  assert wt_path.exists()
  assert (wt_path / "scratch.log").exists()


@pytest.mark.asyncio
async def test_git_worktree_remove_cleans_residue_left_after_git_success(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  worktree_parent = tmp_path / "worktrees"
  wt_path = worktree_parent / "charliebot-task-residue"
  wt_path.mkdir(parents=True)
  (wt_path / ".git").write_text("gitdir: ../repo/.git/worktrees/residue\n", encoding="utf-8")
  (wt_path / "scratch.log").write_text("leftover\n", encoding="utf-8")
  captured: dict[str, Any] = {}

  class FakeProc(_FakeProc):

    async def communicate(self) -> tuple[bytes, bytes]:
      (wt_path / ".git").unlink()
      return b"", b""

  async def fake_create_subprocess_exec(*args: Any, **kwargs: Any) -> FakeProc:
    captured["args"] = args
    captured["kwargs"] = kwargs
    return FakeProc()

  monkeypatch.setattr(git_module.asyncio, "create_subprocess_exec", fake_create_subprocess_exec)

  removed = await git_module.git_worktree_remove(
      str(tmp_path / "repo"),
      wt_path,
      "thread-id",
      allowed_parent=worktree_parent,
      expected_residue_name="charliebot-task-residue",
  )

  assert removed is True
  assert captured["args"][:4] == ("git", "worktree", "remove", "--force")
  assert captured["args"][4] == str(wt_path)
  assert not wt_path.exists()


@pytest.mark.asyncio
async def test_git_worktree_remove_refuses_unexpected_residue_name(tmp_path: Path) -> None:
  worktree_parent = tmp_path / "worktrees"
  wt_path = worktree_parent / "unrelated-directory"
  wt_path.mkdir(parents=True)

  with pytest.raises(RuntimeError, match="directory name does not match expected"):
    await git_module.git_worktree_remove(
        str(tmp_path / "repo"),
        wt_path,
        "thread-id",
        allowed_parent=worktree_parent,
        expected_residue_name="charliebot-task-expected",
    )

  assert wt_path.exists()


@pytest.mark.asyncio
async def test_git_worktree_remove_refuses_residue_with_git_marker(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  worktree_parent = tmp_path / "worktrees"
  wt_path = worktree_parent / "charliebot-task-still-attached"
  wt_path.mkdir(parents=True)
  (wt_path / ".git").write_text("gitdir: ../repo/.git/worktrees/still-attached\n", encoding="utf-8")

  class FakeProc(_FakeProc):

    async def communicate(self) -> tuple[bytes, bytes]:
      return b"", b""

  _patch_git_exec(monkeypatch, FakeProc())

  with pytest.raises(RuntimeError, match=r"with \.git marker"):
    await git_module.git_worktree_remove(
        str(tmp_path / "repo"),
        wt_path,
        "thread-id",
        allowed_parent=worktree_parent,
        expected_residue_name="charliebot-task-still-attached",
    )

  assert wt_path.exists()
  assert (wt_path / ".git").exists()


@pytest.mark.asyncio
async def test_git_worktree_remove_refuses_path_outside_allowed_parent(tmp_path: Path) -> None:
  worktree_parent = tmp_path / "worktrees"
  wt_path = tmp_path / "other" / "charliebot-task-elsewhere"
  wt_path.mkdir(parents=True)

  with pytest.raises(RuntimeError, match="outside allowed parent"):
    await git_module.git_worktree_remove(
        str(tmp_path / "repo"),
        wt_path,
        "thread-id",
        allowed_parent=worktree_parent,
        expected_residue_name="charliebot-task-elsewhere",
    )

  assert wt_path.exists()


@pytest.mark.asyncio
async def test_git_worktree_remove_refuses_repo_root(tmp_path: Path) -> None:
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
async def test_git_worktree_remove_refuses_symlink_target(tmp_path: Path) -> None:
  worktree_parent = tmp_path / "worktrees"
  real_target = tmp_path / "real-target"
  real_target.mkdir()
  wt_path = worktree_parent / "charliebot-task-symlink"
  worktree_parent.mkdir()
  wt_path.symlink_to(real_target, target_is_directory=True)

  with pytest.raises(RuntimeError, match="symlink"):
    await git_module.git_worktree_remove(
        str(tmp_path / "repo"),
        wt_path,
        "thread-id",
        allowed_parent=worktree_parent,
        expected_residue_name="charliebot-task-symlink",
    )

  assert wt_path.is_symlink()
  assert real_target.exists()


@pytest.mark.asyncio
async def test_git_worktree_remove_precleans_nested_local_artifacts(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  worktree_parent = tmp_path / "worktrees"
  wt_path = worktree_parent / "charliebot-task-preclean"
  project_dir = wt_path / "workspace" / "project"
  project_dir.mkdir(parents=True)
  (wt_path / ".git").write_text("gitdir: ../repo/.git/worktrees/preclean\n", encoding="utf-8")
  (project_dir / ".pixi").mkdir()
  (project_dir / ".pixi" / "env.txt").write_text("env\n", encoding="utf-8")
  (project_dir / ".uv-cache").mkdir()
  (project_dir / ".uv-cache" / "cache.txt").write_text("cache\n", encoding="utf-8")
  (project_dir / ".venv").mkdir()
  (project_dir / ".venv" / "python").write_text("env\n", encoding="utf-8")
  (project_dir / "source.txt").write_text("source\n", encoding="utf-8")

  class FakeProc(_FakeProc):

    async def communicate(self) -> tuple[bytes, bytes]:
      assert not (project_dir / ".pixi").exists()
      assert not (project_dir / ".uv-cache").exists()
      assert not (project_dir / ".venv").exists()
      assert (project_dir / "source.txt").exists()
      (wt_path / ".git").unlink()
      return b"", b""

  _patch_git_exec(monkeypatch, FakeProc())

  removed = await git_module.git_worktree_remove(
      str(tmp_path / "repo"),
      wt_path,
      "thread-id",
      allowed_parent=worktree_parent,
      expected_residue_name="charliebot-task-preclean",
  )

  assert removed is True
  assert not wt_path.exists()
