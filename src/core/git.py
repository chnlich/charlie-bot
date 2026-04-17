"""Shared git helpers — subprocess operations with timeouts and error handling."""

import asyncio
import subprocess
from pathlib import Path

import structlog

from src.core.timeouts import (
    SUBPROCESS_GIT_READ_TIMEOUT_ASYNC,
    SUBPROCESS_GIT_WRITE_TIMEOUT,
)

log = structlog.get_logger()


async def git_current_branch(repo_path: Path) -> str:
  """Get the current branch of the repo."""
  proc = await asyncio.create_subprocess_exec(
      "git",
      "rev-parse",
      "--abbrev-ref",
      "HEAD",
      cwd=str(repo_path),
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
  )
  try:
    stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=SUBPROCESS_GIT_READ_TIMEOUT_ASYNC)
  except asyncio.TimeoutError:
    proc.kill()
    raise RuntimeError(f'git rev-parse timed out after {SUBPROCESS_GIT_READ_TIMEOUT_ASYNC}s in {repo_path}')
  if proc.returncode != 0:
    err_msg = stderr.decode().strip()
    if 'unknown revision' in err_msg:
      log.warning('git_empty_repo_fallback', repo=str(repo_path), detail='no commits yet, defaulting to main')
      return 'main'
    raise RuntimeError(f'git rev-parse failed: {err_msg}')
  return stdout.decode().strip()


async def git_create_worktree(repo_path: Path, base_branch: str, branch_name: str, wt_path: Path) -> None:
  """Create a git worktree and fail loudly if git reports an error."""
  proc = await asyncio.create_subprocess_exec(
      "git",
      "worktree",
      "add",
      "-b",
      branch_name,
      str(wt_path),
      base_branch,
      cwd=str(repo_path),
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
  )
  try:
    stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=SUBPROCESS_GIT_WRITE_TIMEOUT)
  except asyncio.TimeoutError:
    proc.kill()
    raise RuntimeError(f'git worktree add timed out after {SUBPROCESS_GIT_WRITE_TIMEOUT}s for {branch_name}')
  if proc.returncode != 0:
    out = stdout.decode().strip()
    err = stderr.decode().strip()
    log.error(
        "spawn_worker_worktree_create_failed",
        repo=str(repo_path),
        branch=branch_name,
        worktree=str(wt_path),
        base_branch=base_branch,
        stdout=out,
        stderr=err,
        returncode=proc.returncode,
    )
    raise RuntimeError(f"git worktree add failed for {branch_name}: {err or out or 'unknown error'}")


async def git_worktree_remove(repo_path: str, wt_path: Path, thread_id: str) -> bool:
  """Remove a git worktree. Returns True on success, False on failure."""
  proc = await asyncio.create_subprocess_exec(
      "git",
      "worktree",
      "remove",
      "--force",
      str(wt_path),
      cwd=repo_path,
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
  )
  try:
    _, stderr = await asyncio.wait_for(proc.communicate(), timeout=SUBPROCESS_GIT_READ_TIMEOUT_ASYNC)
  except asyncio.TimeoutError:
    proc.kill()
    log.warning("worktree_remove_timeout", thread_id=thread_id, path=str(wt_path))
    return False
  if proc.returncode != 0:
    log.warning("worktree_remove_failed", thread_id=thread_id, stderr=stderr.decode().strip())
    return False
  log.info("worktree_removed", thread_id=thread_id, path=str(wt_path))
  return True


async def git_worktree_prune(repo_path: str, thread_id: str) -> None:
  """Prune stale worktree refs."""
  prune_proc = await asyncio.create_subprocess_exec(
      "git",
      "worktree",
      "prune",
      cwd=repo_path,
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
  )
  try:
    await asyncio.wait_for(prune_proc.communicate(), timeout=SUBPROCESS_GIT_READ_TIMEOUT_ASYNC)
  except asyncio.TimeoutError:
    prune_proc.kill()
    log.warning("worktree_prune_timeout", thread_id=thread_id)


async def _run_git_cmd(
    repo_path: Path,
    *args: str,
    timeout: int,
    timeout_label: str,
) -> tuple[bool, str]:
  """Run a git command with timeout. Returns (success, stderr)."""
  proc = await asyncio.create_subprocess_exec(
      "git",
      *args,
      cwd=str(repo_path),
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
  )
  try:
    _, stderr = await asyncio.wait_for(proc.communicate(), timeout=timeout)
  except asyncio.TimeoutError:
    proc.kill()
    return False, f"{timeout_label} timed out after {timeout}s"
  if proc.returncode != 0:
    return False, stderr.decode().strip()
  return True, ""


async def git_merge_ff_only(repo_path: Path, source_branch: str) -> tuple[bool, str]:
  """Run git merge --ff-only. Returns (success, stderr)."""
  return await _run_git_cmd(
      repo_path,
      "merge",
      "--ff-only",
      source_branch,
      timeout=SUBPROCESS_GIT_WRITE_TIMEOUT,
      timeout_label="git merge --ff-only",
  )


async def git_push_branch(repo_path: Path, branch: str) -> tuple[bool, str]:
  """Run git push origin <branch>. Returns (success, stderr)."""
  return await _run_git_cmd(
      repo_path,
      "push",
      "origin",
      branch,
      timeout=SUBPROCESS_GIT_WRITE_TIMEOUT,
      timeout_label="git push",
  )


async def git_push_refspec(repo_path: Path, local_branch: str, remote_branch: str) -> tuple[bool, str]:
  """Run git push origin <local>:refs/heads/<remote>. Returns (success, stderr)."""
  return await _run_git_cmd(
      repo_path,
      "push",
      "origin",
      f"{local_branch}:refs/heads/{remote_branch}",
      timeout=SUBPROCESS_GIT_WRITE_TIMEOUT,
      timeout_label="git push refspec",
  )


async def git_pull_ff_only(repo_path: Path, branch: str) -> tuple[bool, str]:
  """Run git pull --ff-only origin <branch>. Returns (success, stderr)."""
  return await _run_git_cmd(
      repo_path,
      "pull",
      "--ff-only",
      "origin",
      branch,
      timeout=SUBPROCESS_GIT_WRITE_TIMEOUT,
      timeout_label="git pull --ff-only",
  )


async def git_add_commit_push(
    repo_path: Path,
    files: list[str],
    message: str,
    timeout: int = SUBPROCESS_GIT_WRITE_TIMEOUT,
) -> None:
  """Git add, commit, and push with subprocess timeouts.

  Runs each git command in a thread-pool worker with a per-command timeout.
  Catches CalledProcessError and TimeoutExpired consistently, logging warnings
  rather than propagating.

  Args:
    repo_path: Working directory for git commands.
    files: Paths (relative to repo_path) to stage.
    message: Commit message.
    timeout: Per-command timeout in seconds.
  """

  def _run() -> None:
    subprocess.run(['git', 'add'] + files, cwd=repo_path, check=True, capture_output=True, timeout=timeout)
    subprocess.run(['git', 'commit', '-m', message], cwd=repo_path, check=True, capture_output=True, timeout=timeout)
    subprocess.run(['git', 'push'], cwd=repo_path, check=True, capture_output=True, timeout=timeout)

  try:
    await asyncio.to_thread(_run)
  except subprocess.CalledProcessError as e:
    log.warning('git_commit_push_failed', cmd=e.cmd, stderr=e.stderr.decode(errors='replace'))
  except subprocess.TimeoutExpired as e:
    log.warning('git_commit_push_timeout', cmd=e.cmd, timeout=e.timeout)
  except Exception as e:
    log.warning('git_commit_push_error', error=str(e))
