"""Server build info: git SHA and UTC start time, captured at server startup.

Importable by the API layer so ``GET /api/internal/version`` can surface the running
server's build identity without re-running git on every request.
"""

import subprocess

from src.infra import constants, timeouts

_sha: str = "unknown"
_started_at: str = ""


def init_build_info() -> None:
  """Capture the git SHA and UTC start time. Called once at server startup.

  Idempotent — re-calling overwrites the previously captured values (used by tests).
  """
  # Lazy: this module stays stdlib-only (src.runtime.cli.common lazy-imports
  # read_repo_head_sha to keep buildinfo off its import floor), so the pydantic
  # stack loads only here, inside the startup caller that already carries it.
  from src.infra import models

  global _sha, _started_at
  _sha = read_repo_head_sha(timeouts.SUBPROCESS_GIT_SHA_TIMEOUT) or "unknown"
  _started_at = models.utc_now_iso()


def read_repo_head_sha(timeout: float) -> str | None:
  """Return `git rev-parse --short HEAD` output in the repo root, or None on any failure.

  Every failure mode (missing git, non-zero exit, timeout) yields None so each caller
  picks its own failure sentinel.
  """
  try:
    proc = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"],
        cwd=str(constants.REPO_ROOT),
        capture_output=True,
        check=False,
        timeout=timeout,
    )
  except (OSError, subprocess.SubprocessError):
    return None
  if proc.returncode != 0:
    return None
  return proc.stdout.decode().strip() or None


def build_info() -> dict:
  """Return the captured build info as ``{"sha": ..., "started_at": ...}``."""
  return {"sha": _sha, "started_at": _started_at}
