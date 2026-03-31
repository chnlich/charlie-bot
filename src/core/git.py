"""Shared git helpers — add, commit, push with timeouts and error handling."""

import asyncio
import subprocess
from pathlib import Path

import structlog

from src.core.timeouts import SUBPROCESS_GIT_WRITE_TIMEOUT

log = structlog.get_logger()


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
