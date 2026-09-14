"""Server build info: git SHA and UTC start time, captured at server startup.

Importable by the API layer so ``GET /api/internal/version`` can surface the running
server's build identity without re-running git on every request.
"""

import subprocess
from datetime import UTC, datetime

from src.core.constants import REPO_ROOT
from src.core.timeouts import SUBPROCESS_GIT_SHA_TIMEOUT

_sha: str = "unknown"
_started_at: str = ""


def init_build_info() -> None:
  """Capture the git SHA and UTC start time. Called once at server startup.

  Idempotent — re-calling overwrites the previously captured values (used by tests).
  """
  global _sha, _started_at
  _sha = read_repo_head_sha(SUBPROCESS_GIT_SHA_TIMEOUT) or "unknown"
  _started_at = datetime.now(UTC).isoformat()


def read_repo_head_sha(timeout: float) -> str | None:
  """Return `git rev-parse --short HEAD` output in the repo root, or None on any failure.

  Every failure mode (missing git, non-zero exit, timeout) yields None so each caller
  picks its own failure sentinel.
  """
  try:
    proc = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"],
        cwd=str(REPO_ROOT),
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
