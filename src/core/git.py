"""Async git operations for CharlieBot."""

import asyncio
from pathlib import Path


class GitError(Exception):
  pass


async def _run_git(args: list[str], cwd: Path | None = None) -> str:
  """Run a git command asynchronously and return stdout."""
  proc = await asyncio.create_subprocess_exec(
    "git",
    *args,
    cwd=str(cwd) if cwd else None,
    stdout=asyncio.subprocess.PIPE,
    stderr=asyncio.subprocess.PIPE,
  )
  stdout, stderr = await proc.communicate()
  if proc.returncode != 0:
    raise GitError(f"git {' '.join(args)} failed: {stderr.decode().strip()}")
  return stdout.decode().strip()


class GitManager:
  """Wraps git operations needed by CharlieBot."""

  async def create_branch_and_worktree(
    self,
    repo_path: Path,
    worktree_path: Path,
    branch_name: str,
    base_branch: str,
  ) -> None:
    """Create a new branch and add a worktree for it atomically."""
    await _run_git(
      ["worktree", "add", "-b", branch_name, str(worktree_path), base_branch],
      cwd=repo_path,
    )

  async def remove_worktree(self, repo_path: Path, worktree_path: Path) -> None:
    """Remove a worktree (force to handle unclean state)."""
    await _run_git(["worktree", "remove", "--force", str(worktree_path)], cwd=repo_path)

  async def get_commit_log(self, repo_path: Path, branch: str, n: int = 20) -> list[str]:
    """Return the last n commit messages for a branch."""
    output = await _run_git(
      ["log", "--oneline", f"-{n}", branch],
      cwd=repo_path,
    )
    return [line.strip() for line in output.splitlines() if line.strip()]

  async def get_diff(self, repo_path: Path, branch_a: str, branch_b: str) -> str:
    """Return diff between two branches."""
    return await _run_git(["diff", f"{branch_a}...{branch_b}"], cwd=repo_path)

  async def get_current_branch(self, worktree_path: Path) -> str:
    """Get the current branch name of a worktree."""
    return await _run_git(["rev-parse", "--abbrev-ref", "HEAD"], cwd=worktree_path)
