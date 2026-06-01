"""Tests for safe git worktree cleanup."""

from pathlib import Path
from typing import Any

import pytest

from src.core import git as git_module


@pytest.mark.asyncio
async def test_git_worktree_remove_deletes_expected_gitless_residue(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  wt_path = tmp_path / "charliebot-task-leftover"
  wt_path.mkdir()
  (wt_path / "scratch.log").write_text("leftover\n", encoding="utf-8")

  async def fail_create_subprocess_exec(*args: Any, **kwargs: Any) -> Any:
    del args, kwargs
    raise AssertionError("git should not run for an already git-detached residue")

  monkeypatch.setattr(git_module.asyncio, "create_subprocess_exec", fail_create_subprocess_exec)

  removed = await git_module.git_worktree_remove(
      str(tmp_path / "repo"),
      wt_path,
      "thread-id",
      expected_residue_name="charliebot-task-leftover",
  )

  assert removed is True
  assert not wt_path.exists()


@pytest.mark.asyncio
async def test_git_worktree_remove_cleans_residue_left_after_git_success(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  wt_path = tmp_path / "charliebot-task-residue"
  wt_path.mkdir()
  (wt_path / ".git").write_text("gitdir: ../repo/.git/worktrees/residue\n", encoding="utf-8")
  (wt_path / "scratch.log").write_text("leftover\n", encoding="utf-8")
  captured: dict[str, Any] = {}

  class FakeProc:
    returncode = 0

    async def communicate(self) -> tuple[bytes, bytes]:
      (wt_path / ".git").unlink()
      return b"", b""

    def kill(self) -> None:
      raise AssertionError("process should not be killed")

  async def fake_create_subprocess_exec(*args: Any, **kwargs: Any) -> FakeProc:
    captured["args"] = args
    captured["kwargs"] = kwargs
    return FakeProc()

  monkeypatch.setattr(git_module.asyncio, "create_subprocess_exec", fake_create_subprocess_exec)

  removed = await git_module.git_worktree_remove(
      str(tmp_path / "repo"),
      wt_path,
      "thread-id",
      expected_residue_name="charliebot-task-residue",
  )

  assert removed is True
  assert captured["args"][:4] == ("git", "worktree", "remove", "--force")
  assert captured["args"][4] == str(wt_path)
  assert not wt_path.exists()


@pytest.mark.asyncio
async def test_git_worktree_remove_refuses_unexpected_residue_name(tmp_path: Path) -> None:
  wt_path = tmp_path / "unrelated-directory"
  wt_path.mkdir()

  with pytest.raises(RuntimeError, match="directory name does not match expected"):
    await git_module.git_worktree_remove(
        str(tmp_path / "repo"),
        wt_path,
        "thread-id",
        expected_residue_name="charliebot-task-expected",
    )

  assert wt_path.exists()


@pytest.mark.asyncio
async def test_git_worktree_remove_refuses_residue_with_git_marker(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  wt_path = tmp_path / "charliebot-task-still-attached"
  wt_path.mkdir()
  (wt_path / ".git").write_text("gitdir: ../repo/.git/worktrees/still-attached\n", encoding="utf-8")

  class FakeProc:
    returncode = 0

    async def communicate(self) -> tuple[bytes, bytes]:
      return b"", b""

    def kill(self) -> None:
      raise AssertionError("process should not be killed")

  async def fake_create_subprocess_exec(*args: Any, **kwargs: Any) -> FakeProc:
    del args, kwargs
    return FakeProc()

  monkeypatch.setattr(git_module.asyncio, "create_subprocess_exec", fake_create_subprocess_exec)

  with pytest.raises(RuntimeError, match="with .git marker"):
    await git_module.git_worktree_remove(
        str(tmp_path / "repo"),
        wt_path,
        "thread-id",
        expected_residue_name="charliebot-task-still-attached",
    )

  assert wt_path.exists()
  assert (wt_path / ".git").exists()
