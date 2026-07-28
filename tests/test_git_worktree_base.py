"""Tests for git_create_worktree's strict --base-branch resolution matrix.

Validates resolve_base_branch semantics (the only accepted forms):
  - local equals origin     → start-point = origin tip
  - local behind origin     → hard error (stale local base is never picked silently)
  - local ahead of origin   → hard error (unpushed commits never silently become the base)
  - local diverged          → hard error
  - origin unreachable      → hard error (no silent local fallback)
  - branch exists only locally (no matching origin branch) → local tip
  - origin/<b> explicit     → freshly fetched origin tip, local state irrelevant
  - origin/<b> explicit + remote branch missing → hard error
  - no origin remote at all → local tip
  - full SHA                → pinned, origin may move meanwhile
  - unknown SHA / garbage   → hard error
"""

import subprocess
from pathlib import Path
from typing import Iterator

import pytest

from src.core.git import (
    BaseBranchResolutionError,
    BaseResolution,
    git_create_worktree,
)


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
  manipulate origin and/or local independently to construct the matrix states.
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
  resolution = await git_create_worktree(main_checkout, "feature", "charliebot/task-equal", wt_path)

  assert _worktree_head(wt_path) == expected
  assert isinstance(resolution, BaseResolution)
  assert resolution.canonical == "feature"
  assert resolution.start_point == "origin/feature"


def _expect_hard_error(excinfo: pytest.ExceptionInfo[BaseException], *fragments: str) -> None:
  message = str(excinfo.value)
  for fragment in fragments:
    assert fragment in message, f"expected {fragment!r} in error message: {message}"


@pytest.mark.asyncio
async def test_local_behind_origin_raises(repo_setup: dict[str, Path]) -> None:
  """A stale local base must never be picked silently: local behind origin is a hard error."""
  seed = repo_setup["seed"]
  main_checkout = repo_setup["main_checkout"]

  origin_tip = _commit(seed, "advance.txt", "advance\n", "advance origin")
  _git(seed, "push", "origin", "feature")
  local_tip = _git(main_checkout, "rev-parse", "feature")
  assert local_tip != origin_tip  # sanity

  with pytest.raises(BaseBranchResolutionError) as excinfo:
    await git_create_worktree(
        main_checkout, "feature", "charliebot/task-behind", repo_setup["tmp_path"] / "wt-behind")
  _expect_hard_error(excinfo, "differs from origin/feature", local_tip[:12], origin_tip[:12], "origin/feature")


@pytest.mark.asyncio
async def test_local_ahead_of_origin_raises(repo_setup: dict[str, Path]) -> None:
  """Unpushed local commits must never silently become the base: local ahead is a hard error."""
  main_checkout = repo_setup["main_checkout"]

  origin_tip = _git(main_checkout, "rev-parse", "origin/feature")
  local_tip = _commit(main_checkout, "local_only.txt", "ahead\n", "unpushed local commit")
  assert local_tip != origin_tip

  with pytest.raises(BaseBranchResolutionError) as excinfo:
    await git_create_worktree(
        main_checkout, "feature", "charliebot/task-ahead", repo_setup["tmp_path"] / "wt-ahead")
  _expect_hard_error(excinfo, "differs from origin/feature", local_tip[:12], origin_tip[:12])


@pytest.mark.asyncio
async def test_local_diverged_raises(repo_setup: dict[str, Path]) -> None:
  """Diverged local/remote is a hard error, never a silent local fallback."""
  seed = repo_setup["seed"]
  main_checkout = repo_setup["main_checkout"]

  _commit(seed, "origin_only.txt", "origin\n", "origin diverged")
  _git(seed, "push", "origin", "feature")
  _commit(main_checkout, "local_only.txt", "local\n", "local diverged")

  with pytest.raises(BaseBranchResolutionError) as excinfo:
    await git_create_worktree(
        main_checkout, "feature", "charliebot/task-diverged", repo_setup["tmp_path"] / "wt-diverged")
  _expect_hard_error(excinfo, "differs from origin/feature")


@pytest.mark.asyncio
async def test_unreachable_origin_raises(repo_setup: dict[str, Path], tmp_path: Path) -> None:
  """An unreachable origin is a hard error (the SHA form is the documented offline escape)."""
  main_checkout = repo_setup["main_checkout"]
  _git(main_checkout, "remote", "set-url", "origin", str(tmp_path / "does-not-exist.git"))

  with pytest.raises(BaseBranchResolutionError) as excinfo:
    await git_create_worktree(
        main_checkout, "feature", "charliebot/task-unreachable", repo_setup["tmp_path"] / "wt-unreach")
  _expect_hard_error(excinfo, "cannot reach origin", "commit SHA")


@pytest.mark.asyncio
async def test_unpublished_branch_uses_local_tip(repo_setup: dict[str, Path]) -> None:
  """A branch with no matching origin branch starts from the local tip."""
  main_checkout = repo_setup["main_checkout"]
  _git(main_checkout, "checkout", "-b", "localonly")
  local_tip = _commit(main_checkout, "local.txt", "x\n", "local-only commit")

  wt_path = repo_setup["tmp_path"] / "wt-unpublished"
  resolution = await git_create_worktree(main_checkout, "localonly", "charliebot/task-local", wt_path)

  assert _worktree_head(wt_path) == local_tip
  assert resolution.canonical == "localonly"
  assert resolution.start_point == "localonly"


@pytest.mark.asyncio
async def test_explicit_origin_form_uses_remote_regardless_of_local(repo_setup: dict[str, Path]) -> None:
  """origin/<b> is an explicit remote choice: fresh origin tip even when local moved ahead."""
  main_checkout = repo_setup["main_checkout"]

  origin_tip = _git(main_checkout, "rev-parse", "origin/feature")
  local_tip = _commit(main_checkout, "local_only.txt", "ahead\n", "unpushed local commit")
  assert local_tip != origin_tip

  wt_path = repo_setup["tmp_path"] / "wt-explicit"
  resolution = await git_create_worktree(main_checkout, "origin/feature", "charliebot/task-exp", wt_path)

  assert _worktree_head(wt_path) == origin_tip
  assert resolution.canonical == "feature"
  assert resolution.start_point == "origin/feature"


@pytest.mark.asyncio
async def test_explicit_origin_form_remote_branch_missing_raises(repo_setup: dict[str, Path]) -> None:
  """Explicitly requesting a remote branch that does not exist is a hard error."""
  main_checkout = repo_setup["main_checkout"]

  with pytest.raises(BaseBranchResolutionError) as excinfo:
    await git_create_worktree(
        main_checkout, "origin/nosuchbranch", "charliebot/task-exp-miss", repo_setup["tmp_path"] / "wt-miss")
  _expect_hard_error(excinfo, "does not exist")


@pytest.mark.asyncio
async def test_no_origin_remote_uses_local(repo_setup: dict[str, Path]) -> None:
  """Without any origin remote configured, the bare branch resolves to the local tip."""
  main_checkout = repo_setup["main_checkout"]
  _git(main_checkout, "remote", "remove", "origin")
  local_tip = _git(main_checkout, "rev-parse", "feature")

  wt_path = repo_setup["tmp_path"] / "wt-noremote"
  resolution = await git_create_worktree(main_checkout, "feature", "charliebot/task-noremote", wt_path)

  assert _worktree_head(wt_path) == local_tip
  assert resolution.start_point == "feature"


@pytest.mark.asyncio
async def test_full_sha_pins_base(repo_setup: dict[str, Path]) -> None:
  """A 40-hex SHA pins the base exactly; origin moving meanwhile must not matter."""
  seed = repo_setup["seed"]
  main_checkout = repo_setup["main_checkout"]

  pinned = _git(main_checkout, "rev-parse", "feature")
  moved_tip = _commit(seed, "advance.txt", "advance\n", "advance origin")
  _git(seed, "push", "origin", "feature")
  assert moved_tip != pinned

  wt_path = repo_setup["tmp_path"] / "wt-sha"
  resolution = await git_create_worktree(main_checkout, pinned, "charliebot/task-sha", wt_path)

  assert _worktree_head(wt_path) == pinned
  assert resolution.canonical == pinned
  assert resolution.start_point == pinned


@pytest.mark.asyncio
async def test_unknown_sha_raises(repo_setup: dict[str, Path]) -> None:
  main_checkout = repo_setup["main_checkout"]
  with pytest.raises(BaseBranchResolutionError) as excinfo:
    await git_create_worktree(
        main_checkout,
        "0" * 40,
        "charliebot/task-badsha",
        repo_setup["tmp_path"] / "wt-badsha")
  _expect_hard_error(excinfo, "not found")


@pytest.mark.asyncio
@pytest.mark.parametrize("raw", ["", "   ", "origin/", "has space", "has..dots"])
async def test_garbage_input_raises(repo_setup: dict[str, Path], raw: str) -> None:
  main_checkout = repo_setup["main_checkout"]
  with pytest.raises(BaseBranchResolutionError):
    await git_create_worktree(
        main_checkout, raw, "charliebot/task-garbage", repo_setup["tmp_path"] / "wt-garbage")
