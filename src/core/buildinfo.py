"""Server build info: git SHA and UTC start time, captured at server startup.

Importable by the API layer so ``GET /api/internal/version`` can surface the running
server's build identity without re-running git on every request.
"""

import subprocess
from datetime import UTC, datetime
from pathlib import Path

# Repo root: src/core/buildinfo.py -> parents[2] == repo root (where pyproject.toml lives).
_REPO_ROOT = Path(__file__).resolve().parents[2]

# subprocess.run timeout for `git rev-parse --short HEAD`. ~2 s per the version-skew hint spec.
_GIT_SHA_TIMEOUT = 2.0

_sha: str = "unknown"
_started_at: str = ""


def init_build_info() -> None:
  """Capture the git SHA and UTC start time. Called once at server startup.

  Idempotent — re-calling overwrites the previously captured values (used by tests).
  """
  global _sha, _started_at
  _sha = _read_git_sha()
  _started_at = datetime.now(UTC).isoformat()


def _read_git_sha() -> str:
  """Return `git rev-parse --short HEAD` output in the repo root, or ``"unknown"`` on any failure."""
  try:
    proc = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"],
        cwd=str(_REPO_ROOT),
        capture_output=True,
        check=False,
        timeout=_GIT_SHA_TIMEOUT,
    )
  except (OSError, subprocess.SubprocessError):
    return "unknown"
  if proc.returncode != 0:
    return "unknown"
  return proc.stdout.decode().strip() or "unknown"


def build_info() -> dict:
  """Return the captured build info as ``{"sha": ..., "started_at": ...}``."""
  return {"sha": _sha, "started_at": _started_at}
