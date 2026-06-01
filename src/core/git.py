"""Shared git helpers — subprocess operations with timeouts and error handling."""

import asyncio
import os
import shutil
import subprocess
from pathlib import Path

import structlog

from src.core.timeouts import (
    SUBPROCESS_GIT_READ_TIMEOUT_ASYNC,
    SUBPROCESS_GIT_WRITE_TIMEOUT,
)

log = structlog.get_logger()

_WORKTREE_LOCAL_ARTIFACT_NAMES = frozenset({".pixi", ".uv-cache", ".venv", "build"})


def git_worktree_dir_name(branch_name: str) -> str:
  """Return the directory name CharlieBot uses for a git worktree branch."""
  return branch_name.replace("/", "-")


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


async def _resolve_worktree_start_point(repo_path: Path, base_branch: str) -> str:
  """Pick a start-point for `git worktree add` that prefers origin when local is behind.

  Fetches origin/<base_branch>, then compares the local branch tip to the remote tip:
    - local is ancestor of origin   → start from origin/<base_branch> (avoids stale local)
    - local is strictly ahead       → start from <base_branch> (preserve unpushed commits)
    - diverged                      → start from <base_branch> + warning
    - fetch failure / unknown state → start from <base_branch> + warning

  If <base_branch> already includes a remote prefix (e.g. 'origin/main'), the caller
  has already chosen an explicit ref; return it unchanged without fetching.
  """
  if base_branch.startswith("origin/"):
    return base_branch

  fetch_ok, fetch_err = await _run_git_cmd(
      repo_path,
      "fetch",
      "origin",
      base_branch,
      timeout=SUBPROCESS_GIT_WRITE_TIMEOUT,
      timeout_label="git fetch",
  )
  if not fetch_ok:
    log.warning(
        "git_fetch_failed_using_local_base",
        repo=str(repo_path),
        base=base_branch,
        stderr=fetch_err,
    )
    return base_branch

  remote_ref = f"origin/{base_branch}"
  local_is_ancestor = await _git_is_ancestor(repo_path, base_branch, remote_ref)
  if local_is_ancestor == 0:
    return remote_ref
  if local_is_ancestor == 1:
    origin_is_ancestor = await _git_is_ancestor(repo_path, remote_ref, base_branch)
    if origin_is_ancestor == 0:
      # Local strictly ahead — user has unpushed commits we must preserve.
      return base_branch
    if origin_is_ancestor == 1:
      log.warning("git_local_diverged_from_origin", repo=str(repo_path), base=base_branch)
      return base_branch

  log.warning(
      "git_ancestor_check_failed_using_local_base",
      repo=str(repo_path),
      base=base_branch,
      returncode=local_is_ancestor,
  )
  return base_branch


async def _git_is_ancestor(repo_path: Path, ancestor: str, descendant: str) -> int:
  """Run `git merge-base --is-ancestor <ancestor> <descendant>` and return the exit code.

  Exit code 0 → ancestor relationship holds; 1 → does not hold; other → error.
  Returns -1 on timeout to distinguish from git's documented exit codes.
  """
  proc = await asyncio.create_subprocess_exec(
      "git",
      "merge-base",
      "--is-ancestor",
      ancestor,
      descendant,
      cwd=str(repo_path),
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
  )
  try:
    await asyncio.wait_for(proc.communicate(), timeout=SUBPROCESS_GIT_READ_TIMEOUT_ASYNC)
  except asyncio.TimeoutError:
    proc.kill()
    return -1
  return proc.returncode if proc.returncode is not None else -1


async def git_create_worktree(repo_path: Path, base_branch: str, branch_name: str, wt_path: Path) -> None:
  """Create a git worktree and fail loudly if git reports an error.

  Resolves the start-point via `_resolve_worktree_start_point` so that a stale local
  copy of <base_branch> doesn't silently produce a worker rooted at an old commit.
  """
  start_point = await _resolve_worktree_start_point(repo_path, base_branch)
  proc = await asyncio.create_subprocess_exec(
      "git",
      "worktree",
      "add",
      "-b",
      branch_name,
      str(wt_path),
      start_point,
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
        start_point=start_point,
        stdout=out,
        stderr=err,
        returncode=proc.returncode,
    )
    raise RuntimeError(f"git worktree add failed for {branch_name}: {err or out or 'unknown error'}")
  log.info(
      "git_worktree_created",
      repo=str(repo_path),
      base=base_branch,
      start_point=start_point,
      worktree=str(wt_path),
  )


def _is_relative_to(path: Path, parent: Path) -> bool:
  try:
    path.relative_to(parent)
  except ValueError:
    return False
  return True


def _assert_safe_worktree_cleanup_target(
    repo_path: str,
    wt_path: Path,
    *,
    allowed_parent: Path,
    expected_residue_name: str,
) -> Path:
  """Validate the worktree path before removing anything inside it."""
  if wt_path.name != expected_residue_name:
    raise RuntimeError(
        f"refusing to clean worktree at {wt_path}: "
        f"directory name does not match expected {expected_residue_name}")
  if wt_path.is_symlink():
    raise RuntimeError(f"refusing to clean worktree symlink: {wt_path}")

  resolved_wt = wt_path.resolve()
  resolved_allowed_parent = allowed_parent.resolve()
  resolved_repo = Path(repo_path).resolve()
  home = Path.home().resolve()
  if resolved_wt in {Path("/"), home}:
    raise RuntimeError(f"refusing to clean unsafe worktree path: {wt_path}")
  if resolved_wt == resolved_repo:
    raise RuntimeError(f"refusing to clean repo root as worktree: {wt_path}")
  if resolved_wt == resolved_allowed_parent or not _is_relative_to(resolved_wt, resolved_allowed_parent):
    raise RuntimeError(f"refusing to clean worktree outside allowed parent {resolved_allowed_parent}: {wt_path}")
  return resolved_wt


def _contains_git_marker(path: Path) -> bool:
  """Return True if path contains any .git marker without following symlinks."""
  for dirpath, dirnames, filenames in os.walk(path, followlinks=False):
    if ".git" in filenames or ".git" in dirnames:
      return True
  return False


async def _remove_local_worktree_artifacts(wt_path: Path, thread_id: str) -> None:
  """Pre-clean known local cache/env/build artifacts inside the worktree."""
  for dirpath, dirnames, filenames in os.walk(wt_path, topdown=True, followlinks=False):
    if Path(dirpath) != wt_path and (".git" in filenames or ".git" in dirnames):
      dirnames[:] = []
      continue
    dirnames[:] = [name for name in dirnames if name != ".git"]
    artifact_names = [name for name in dirnames if name in _WORKTREE_LOCAL_ARTIFACT_NAMES]
    for name in artifact_names:
      artifact_path = Path(dirpath) / name
      if artifact_path.is_symlink():
        raise RuntimeError(f"refusing to remove symlinked worktree artifact: {artifact_path}")
      await asyncio.to_thread(shutil.rmtree, artifact_path)
      log.info("worktree_local_artifact_removed", thread_id=thread_id, path=str(artifact_path))
    dirnames[:] = [name for name in dirnames if name not in _WORKTREE_LOCAL_ARTIFACT_NAMES]


async def _remove_worktree_residue(
    wt_path: Path,
    thread_id: str,
    *,
    repo_path: str,
    allowed_parent: Path,
    expected_residue_name: str,
) -> None:
  """Delete a git-detached residual worktree directory after explicit safety checks."""
  _assert_safe_worktree_cleanup_target(
      repo_path,
      wt_path,
      allowed_parent=allowed_parent,
      expected_residue_name=expected_residue_name,
  )
  if not wt_path.is_dir():
    raise RuntimeError(f"refusing to remove non-directory worktree residue: {wt_path}")
  if _contains_git_marker(wt_path):
    raise RuntimeError(f"refusing to remove worktree residue with .git marker: {wt_path}")

  await asyncio.to_thread(shutil.rmtree, wt_path)
  if os.path.lexists(wt_path):
    raise RuntimeError(f"failed to remove worktree residue: {wt_path}")
  log.info("worktree_residue_removed", thread_id=thread_id, path=str(wt_path))


async def git_worktree_remove(
    repo_path: str,
    wt_path: Path,
    thread_id: str,
    *,
    allowed_parent: Path,
    expected_residue_name: str,
) -> bool:
  """Remove a git worktree and any safe git-detached residual directory."""
  _assert_safe_worktree_cleanup_target(
      repo_path,
      wt_path,
      allowed_parent=allowed_parent,
      expected_residue_name=expected_residue_name,
  )
  if os.path.lexists(wt_path):
    if wt_path.is_symlink():
      raise RuntimeError(f"refusing to remove worktree symlink: {wt_path}")
    if not wt_path.is_dir():
      raise RuntimeError(f"refusing to remove non-directory worktree: {wt_path}")
    await _remove_local_worktree_artifacts(wt_path, thread_id)

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
  if os.path.lexists(wt_path):
    await _remove_worktree_residue(
        wt_path,
        thread_id,
        repo_path=repo_path,
        allowed_parent=allowed_parent,
        expected_residue_name=expected_residue_name,
    )
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


async def git_fetch(repo_path: Path, remote: str, branch: str) -> tuple[bool, str]:
  """Run git fetch <remote> <branch>. Returns (success, stderr)."""
  return await _run_git_cmd(
      repo_path,
      "fetch",
      remote,
      branch,
      timeout=SUBPROCESS_GIT_WRITE_TIMEOUT,
      timeout_label="git fetch",
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
  """Run git push origin <local>:refs/heads/<remote>. Returns (success, stderr).

  Git rejects non-fast-forward pushes by default, so this is implicitly an FF-only push.
  """
  return await _run_git_cmd(
      repo_path,
      "push",
      "origin",
      f"{local_branch}:refs/heads/{remote_branch}",
      timeout=SUBPROCESS_GIT_WRITE_TIMEOUT,
      timeout_label="git push refspec",
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
