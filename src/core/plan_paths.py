"""Shared path handling for per-session plan artifacts."""

import pathlib


def resolve_plan_file(session_dir: pathlib.Path, file: str) -> tuple[pathlib.Path, pathlib.Path | None]:
  """Resolve a plan artifact and return its in-session relative path if any."""
  resolved_session_dir = session_dir.resolve()
  candidate = (resolved_session_dir / file).resolve()
  try:
    relative = candidate.relative_to(resolved_session_dir)
  except ValueError:
    relative = None
  return candidate, relative


def fallback_relative_path(session_dir: pathlib.Path, candidate: pathlib.Path) -> pathlib.PurePosixPath:
  """Return a safe child-relative path for an artifact outside the parent session."""
  sessions_dir = session_dir.resolve().parent
  try:
    path_under_sessions = candidate.relative_to(sessions_dir)
  except ValueError:
    return pathlib.PurePosixPath("artifacts", candidate.name)
  if len(path_under_sessions.parts) > 1:
    return pathlib.PurePosixPath(*path_under_sessions.parts[1:])
  return pathlib.PurePosixPath("artifacts", candidate.name)
