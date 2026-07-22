"""Shared path handling for per-session plan artifacts."""

from pathlib import Path, PurePosixPath


def resolve_plan_file(session_dir: Path, file: str) -> tuple[Path, Path | None]:
  """Resolve a plan artifact and return its in-session relative path if any."""
  resolved_session_dir = session_dir.resolve()
  candidate = (resolved_session_dir / file).resolve()
  try:
    relative = candidate.relative_to(resolved_session_dir)
  except ValueError:
    relative = None
  return candidate, relative


def fallback_relative_path(session_dir: Path, candidate: Path) -> PurePosixPath:
  """Return a safe child-relative path for an artifact outside the parent session."""
  sessions_dir = session_dir.resolve().parent
  try:
    path_under_sessions = candidate.relative_to(sessions_dir)
  except ValueError:
    return PurePosixPath("artifacts", candidate.name)
  if len(path_under_sessions.parts) > 1:
    return PurePosixPath(*path_under_sessions.parts[1:])
  return PurePosixPath("artifacts", candidate.name)
