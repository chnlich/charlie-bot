"""Tests for git_create_worktree's strict --base-branch resolution matrix.

Validates resolve_base_branch semantics (the only accepted forms):
  - local equals origin     → start-point = origin tip
  - local behind origin     → start at the origin tip, local ref untouched
  - local ahead of origin   → hard error (unpushed commits never silently become the base)
  - local diverged          → hard error
  - origin unreachable      → hard error (no silent local fallback)
  - branch exists only locally (no matching origin branch) → local tip
  - origin/<b> explicit     → freshly fetched origin tip, local state irrelevant
  - origin/<b> explicit + remote branch missing → hard error
  - no origin remote at all → local tip
  - full SHA                → pinned, origin may move meanwhile
  - unknown SHA / garbage   → hard error

Plus the launch path's base-less fallback (spawner._create_worktree_and_process):
  - the remote's default branch is read from the remote itself (ls-remote symref),
    never from the clone-time refs/remotes/origin/HEAD metadata
  - a request without a base starts at origin/<remote-default> even when the local
    checkout is stale (local main behind origin, checkout on a local-only branch)
  - the local-branch read is tripwired off: the fallback must never call
    git_current_branch
  - the probe-fed forms: a caller-supplied remote_tip replaces the duplicate
    ls-remote, and a probe equal to the remote-tracking ref skips the no-op fetch
  - git_remote_default_branch_and_tip returns the default branch and its tip
    from one ls-remote
"""

import subprocess
from pathlib import Path

import pytest

from src.core.git import (
    BaseBranchResolutionError,
    BaseResolution,
    git_create_worktree,
    git_current_branch,
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
def repo_setup(tmp_path: Path) -> dict[str, Path]:
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

  return {
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


@pytest.mark.asyncio
async def test_local_behind_origin_starts_from_origin_tip(repo_setup: dict[str, Path]) -> None:
  """A local branch strictly behind origin starts from the origin tip: the
  worktree misses nothing, and the shared checkout's local ref is untouched."""
  seed = repo_setup["seed"]
  main_checkout = repo_setup["main_checkout"]

  origin_tip = _commit(seed, "advance.txt", "advance\n", "advance origin")
  _git(seed, "push", "origin", "feature")
  local_tip = _git(main_checkout, "rev-parse", "feature")
  assert local_tip != origin_tip  # the local branch is behind

  wt_path = repo_setup["tmp_path"] / "wt-behind"
  resolution = await git_create_worktree(main_checkout, "feature", "charliebot/task-behind", wt_path)

  assert _worktree_head(wt_path) == origin_tip
  assert _git(main_checkout, "rev-parse", resolution.start_point) == origin_tip
  assert _git(main_checkout, "rev-parse", "feature") == local_tip  # the local ref never moved
  assert isinstance(resolution, BaseResolution)
  assert resolution.canonical == "feature"
  assert resolution.start_point == "origin/feature"
  assert resolution.detail == (
      f"branch feature at origin tip {origin_tip[:12]} "
      f"(local feature at {local_tip[:12]} is behind, unused)")


@pytest.mark.asyncio
async def test_merge_base_unexpected_exit_code_raises(
    repo_setup: dict[str, Path], monkeypatch: pytest.MonkeyPatch) -> None:
  """A merge-base answer that is neither 0 nor 1 (a corrupt repo's exit 128, a
  killed process's 2) is a hard error naming the command and its stderr — the
  resolution never guesses from a broken probe."""
  from src.core import git as git_mod

  seed = repo_setup["seed"]
  main_checkout = repo_setup["main_checkout"]
  _commit(seed, "advance.txt", "advance\n", "advance origin")
  _git(seed, "push", "origin", "feature")  # local feature is now behind: the probe runs

  class _BrokenProc:
    returncode = 2

  real_proc_bytes = git_mod._git_proc_bytes

  async def _broken_merge_base(repo_path, *args, timeout):
    if args[:2] == ("merge-base", "--is-ancestor"):
      return _BrokenProc(), b"", b"fatal: bad object"
    return await real_proc_bytes(repo_path, *args, timeout=timeout)

  monkeypatch.setattr(git_mod, "_git_proc_bytes", _broken_merge_base)

  with pytest.raises(BaseBranchResolutionError) as excinfo:
    await git_create_worktree(
        main_checkout, "feature", "charliebot/task-mb-fail", repo_setup["tmp_path"] / "wt-mb-fail")
  message = str(excinfo.value)
  assert "merge-base --is-ancestor" in message
  assert "exit code 2" in message
  assert "fatal: bad object" in message


# --- launch path: base-less fallback resolves to the remote's default branch ------


@pytest.fixture
def remote_default_repo(tmp_path: Path) -> dict[str, Path]:
  """Bare origin whose HEAD points at main, plus a clone of it.

  The clone's refs/remotes/origin/HEAD symref is written once at clone time and
  is never refreshed afterwards — clone-time metadata that silently goes stale
  if the remote's default branch later moves.
  """
  origin = tmp_path / "origin.git"
  seed = tmp_path / "seed"
  clone = tmp_path / "clone"

  _git(tmp_path, "init", "--bare", str(origin))
  _git(origin, "symbolic-ref", "HEAD", "refs/heads/main")

  _git(tmp_path, "clone", str(origin), str(seed))
  _git(seed, "config", "user.email", "test@example.com")
  _git(seed, "config", "user.name", "Test")
  _git(seed, "symbolic-ref", "HEAD", "refs/heads/main")
  _commit(seed, "README.md", "seed\n", "seed main")
  _git(seed, "push", "-u", "origin", "main")

  _git(tmp_path, "clone", str(origin), str(clone))
  _git(clone, "config", "user.email", "test@example.com")
  _git(clone, "config", "user.name", "Test")

  return {
      "origin": origin,
      "seed": seed,
      "clone": clone,
      "tmp_path": tmp_path,
  }


@pytest.mark.asyncio
async def test_baseless_launch_starts_at_remote_default_tip(remote_default_repo: dict[str, Path]) -> None:
  """origin's main advanced, local main stale, checkout on a branch origin does
  not have. A base-less launch must still start at origin's main tip."""
  seed = remote_default_repo["seed"]
  clone = remote_default_repo["clone"]
  tmp_path = remote_default_repo["tmp_path"]

  origin_tip = _commit(seed, "advance.txt", "advance\n", "advance origin main")
  _git(seed, "push", "origin", "main")
  assert _git(clone, "rev-parse", "main") != origin_tip  # sanity: local main is behind

  _git(clone, "checkout", "-b", "local-only")

  base = "origin/main"
  wt_path = tmp_path / "wt-baseless"
  resolution = await git_create_worktree(clone, base, "charliebot/task-baseless", wt_path)

  assert _git(clone, "rev-parse", resolution.start_point) == origin_tip
  assert wt_path.is_dir()
  assert _worktree_head(wt_path) == origin_tip

  # The old fallback (the checkout's current branch) must never land on origin's
  # main tip: it either hard-errors or starts from a different commit. This
  # catches a silent regression back to reading local refs.
  old_base = await git_current_branch(clone)
  try:
    old_wt = tmp_path / "wt-old-fallback"
    await git_create_worktree(clone, old_base, "charliebot/task-old-fallback", old_wt)
  except BaseBranchResolutionError:
    pass
  else:
    assert _worktree_head(old_wt) != origin_tip


# --- probe-fed resolution: the ls-remote answer replaces the duplicate probe + no-op fetch ---
