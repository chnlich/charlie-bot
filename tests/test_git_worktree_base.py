"""Tests for git_create_worktree's origin-aware start-point resolution.

Validates the behavior matrix:
  - local equals origin     → start-point = origin tip (stable, both equal)
  - local behind origin     → start-point = origin tip (avoid stale base)
  - local ahead of origin   → start-point = local tip (preserve unpushed work)
  - local diverged          → start-point = local tip + warning
  - fetch failure           → start-point = local tip + warning
"""

import subprocess
from pathlib import Path
from typing import Iterator

import pytest
from structlog.testing import capture_logs

from src.core.git import git_create_worktree


def _git(cwd: Path, *args: str) -> str:
  """Run a git command synchronously and return stdout. Raises on non-zero exit."""
  result = subprocess.run(
      ["git", *args],
      cwd=str(cwd),
      check=True,
      capture_output=True,
      text=True,
  )
  return result.stdout.strip()


def _commit(cwd: Path, filename: str, content: str, message: str) -> str:
  """Write a file, commit it, and return the new commit's SHA."""
  (cwd / filename).write_text(content, encoding="utf-8")
  _git(cwd, "add", filename)
  _git(cwd, "commit", "-m", message)
  return _git(cwd, "rev-parse", "HEAD")


@pytest.fixture
def repo_setup(tmp_path: Path) -> Iterator[dict[str, Path]]:
  """Create a bare 'origin' repo plus a 'main_checkout' clone with one shared commit.

  Both sides start with the same single commit on branch 'feature'. Tests then
  manipulate origin and/or local independently to construct the four states.
  """
  origin = tmp_path / "origin.git"
  seed = tmp_path / "seed"
  main_checkout = tmp_path / "main_checkout"

  # Bare remote.
  _git(tmp_path, "init", "--bare", str(origin))

  # Seed clone — author the initial commit on branch 'feature' and push.
  _git(tmp_path, "clone", str(origin), str(seed))
  _git(seed, "config", "user.email", "test@example.com")
  _git(seed, "config", "user.name", "Test")
  _git(seed, "checkout", "-b", "feature")
  _commit(seed, "README.md", "seed\n", "seed")
  _git(seed, "push", "-u", "origin", "feature")

  # Main checkout — represents the user's working clone.
  _git(tmp_path, "clone", "--branch", "feature", str(origin), str(main_checkout))
  _git(main_checkout, "config", "user.email", "test@example.com")
  _git(main_checkout, "config", "user.name", "Test")

  yield {
      "origin": origin,
      "seed": seed,
      "main_checkout": main_checkout,
      "tmp_path": tmp_path,
  }


def _worktree_head(wt_path: Path) -> str:
  return _git(wt_path, "rev-parse", "HEAD")


@pytest.mark.asyncio
async def test_local_equals_origin_uses_origin_tip(repo_setup: dict[str, Path]) -> None:
  """When local and origin point at the same commit, the worktree starts from origin/<base>."""
  main_checkout = repo_setup["main_checkout"]
  expected = _git(main_checkout, "rev-parse", "feature")

  wt_path = repo_setup["tmp_path"] / "wt-equal"
  await git_create_worktree(main_checkout, "feature", "charliebot/task-equal", wt_path)

  assert _worktree_head(wt_path) == expected


@pytest.mark.asyncio
async def test_local_behind_origin_uses_origin_tip(repo_setup: dict[str, Path]) -> None:
  """When local is behind origin, the worktree must start from the (newer) remote tip."""
  seed = repo_setup["seed"]
  main_checkout = repo_setup["main_checkout"]

  # Advance origin via the seed clone, leaving main_checkout untouched (=> behind).
  origin_tip = _commit(seed, "advance.txt", "advance\n", "advance origin")
  _git(seed, "push", "origin", "feature")

  local_tip = _git(main_checkout, "rev-parse", "feature")
  assert local_tip != origin_tip  # sanity

  wt_path = repo_setup["tmp_path"] / "wt-behind"
  await git_create_worktree(main_checkout, "feature", "charliebot/task-behind", wt_path)

  assert _worktree_head(wt_path) == origin_tip


@pytest.mark.asyncio
async def test_local_ahead_of_origin_uses_local_tip(repo_setup: dict[str, Path]) -> None:
  """When local is strictly ahead, the worktree must preserve unpushed local commits."""
  main_checkout = repo_setup["main_checkout"]

  origin_tip = _git(main_checkout, "rev-parse", "origin/feature")
  local_tip = _commit(main_checkout, "local_only.txt", "ahead\n", "unpushed local commit")
  assert local_tip != origin_tip

  wt_path = repo_setup["tmp_path"] / "wt-ahead"
  await git_create_worktree(main_checkout, "feature", "charliebot/task-ahead", wt_path)

  assert _worktree_head(wt_path) == local_tip


@pytest.mark.asyncio
async def test_local_diverged_uses_local_and_warns(repo_setup: dict[str, Path]) -> None:
  """When local and origin have diverged, fall back to local and emit a warning."""
  seed = repo_setup["seed"]
  main_checkout = repo_setup["main_checkout"]

  # Diverge: both sides commit independently from the shared seed.
  _commit(seed, "origin_only.txt", "origin\n", "origin diverged")
  _git(seed, "push", "origin", "feature")
  local_tip = _commit(main_checkout, "local_only.txt", "local\n", "local diverged")

  wt_path = repo_setup["tmp_path"] / "wt-diverged"
  with capture_logs() as logs:
    await git_create_worktree(main_checkout, "feature", "charliebot/task-diverged", wt_path)

  assert _worktree_head(wt_path) == local_tip
  assert any(
      entry.get("event") == "git_local_diverged_from_origin" and entry.get("log_level") == "warning"
      for entry in logs), f"expected git_local_diverged_from_origin warning, got: {logs}"


@pytest.mark.asyncio
async def test_fetch_failure_uses_local_and_warns(repo_setup: dict[str, Path], tmp_path: Path) -> None:
  """When `git fetch` fails (bad remote URL), fall back to local and emit a warning."""
  main_checkout = repo_setup["main_checkout"]

  # Repoint origin at a path that doesn't exist so fetch can never succeed.
  bad_remote = tmp_path / "does-not-exist.git"
  _git(main_checkout, "remote", "set-url", "origin", str(bad_remote))

  local_tip = _git(main_checkout, "rev-parse", "feature")

  wt_path = repo_setup["tmp_path"] / "wt-fetchfail"
  with capture_logs() as logs:
    await git_create_worktree(main_checkout, "feature", "charliebot/task-fetchfail", wt_path)

  assert _worktree_head(wt_path) == local_tip
  assert any(
      entry.get("event") == "git_fetch_failed_using_local_base" and entry.get("log_level") == "warning"
      for entry in logs), f"expected git_fetch_failed_using_local_base warning, got: {logs}"
