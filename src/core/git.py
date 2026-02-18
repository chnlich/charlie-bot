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

  async def clone_bare(self, url: str, dest: Path) -> None:
    """Clone a repository as a bare repo."""
    await _run_git(["clone", "--bare", url, str(dest)])

  async def init_bare(self, dest: Path) -> None:
    """Initialize a new bare repository (for local repos without a remote)."""
    dest.mkdir(parents=True, exist_ok=True)
    await _run_git(["init", "--bare", str(dest)])

  async def add_worktree(self, repo_path: Path, worktree_path: Path, branch: str) -> None:
    """Add a worktree for an existing branch."""
    await _run_git(["worktree", "add", str(worktree_path), branch], cwd=repo_path)

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

  async def delete_branch(self, repo_path: Path, branch_name: str) -> None:
    """Delete a branch from the repository."""
    await _run_git(["branch", "-D", branch_name], cwd=repo_path)

  async def merge_branch(
    self,
    repo_path: Path,
    target_branch: str,
    source_branch: str,
  ) -> bool:
    """
    Attempt to merge source_branch into target_branch.
    Returns True on success, False on conflict.
    """
    try:
      # Switch to target branch first (works in bare repos via worktree)
      await _run_git(
        ["merge", "--no-ff", source_branch, "-m", f"chore: merge {source_branch}"],
        cwd=repo_path,
      )
      return True
    except GitError as e:
      if "CONFLICT" in str(e) or "conflict" in str(e).lower():
        return False
      raise

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

  async def get_conflicted_files(self, worktree_path: Path) -> list[str]:
    """Return list of files with merge conflicts."""
    output = await _run_git(["diff", "--name-only", "--diff-filter=U"], cwd=worktree_path)
    return [f.strip() for f in output.splitlines() if f.strip()]

  async def fetch(self, repo_path: Path) -> None:
    """Fetch latest refs from remote."""
    await _run_git(["fetch", "--all"], cwd=repo_path)

  async def list_branches(self, repo_path: Path) -> list[str]:
    """List all branches in the repo."""
    output = await _run_git(["branch", "--format=%(refname:short)"], cwd=repo_path)
    return [b.strip() for b in output.splitlines() if b.strip()]

  async def get_current_branch(self, worktree_path: Path) -> str:
    """Get the current branch name of a worktree."""
    return await _run_git(["rev-parse", "--abbrev-ref", "HEAD"], cwd=worktree_path)

  async def link_local_repo(self, local_path: Path, bare_dest: Path) -> None:
    """
    Set up a bare repo linked to a local path by fetching all its refs.
    Used when session is created with a local repo_path instead of repo_url.
    """
    bare_dest.mkdir(parents=True, exist_ok=True)
    await _run_git(["init", "--bare", str(bare_dest)])
    await _run_git(
      ["remote", "add", "origin", str(local_path)],
      cwd=bare_dest,
    )
    await _run_git(["fetch", "origin"], cwd=bare_dest)
    # Set up local tracking
    branches = await _run_git(["branch", "-r", "--format=%(refname:short)"], cwd=bare_dest)
    for branch in branches.splitlines():
      branch = branch.strip().removeprefix("origin/")
      if branch and branch != "HEAD":
        try:
          await _run_git(
            ["branch", branch, f"origin/{branch}"],
            cwd=bare_dest,
          )
        except GitError:
          pass  # Branch may already exist
