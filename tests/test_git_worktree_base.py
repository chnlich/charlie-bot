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

Plus the resolution's probe-fed forms:
  - a caller-supplied remote_tip replaces the duplicate ls-remote, and a probe
    equal to the remote-tracking ref skips the no-op fetch
  - git_remote_default_branch_and_tip returns the default branch and its tip
    from one ls-remote
"""

import asyncio
from pathlib import Path
from types import SimpleNamespace

import pytest
from conftest import run_git

from src.core.git import (
    BaseBranchResolutionError,
    BaseResolution,
    git_create_worktree,
    git_current_branch,
)


def _commit(cwd: Path, filename: str, content: str, message: str) -> str:
  """Write a file, commit it, and return the new commit's SHA."""
  (cwd / filename).write_text(content, encoding="utf-8")
  run_git(cwd, "add", filename)
  run_git(cwd, "commit", "-m", message)
  return run_git(cwd, "rev-parse", "HEAD")


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
  run_git(tmp_path, "init", "--bare", str(origin))

  # Seed clone — author the initial commit on branch 'feature' and push.
  run_git(tmp_path, "clone", str(origin), str(seed))
  run_git(seed, "config", "user.email", "test@example.com")
  run_git(seed, "config", "user.name", "Test")
  run_git(seed, "checkout", "-b", "feature")
  _commit(seed, "README.md", "seed\n", "seed")
  run_git(seed, "push", "-u", "origin", "feature")

  # Main checkout — represents the user's working clone.
  run_git(tmp_path, "clone", "--branch", "feature", str(origin), str(main_checkout))
  run_git(main_checkout, "config", "user.email", "test@example.com")
  run_git(main_checkout, "config", "user.name", "Test")

  return {
      "origin": origin,
      "seed": seed,
      "main_checkout": main_checkout,
      "tmp_path": tmp_path,
  }


def _worktree_head(wt_path: Path) -> str:
  return run_git(wt_path, "rev-parse", "HEAD")


@pytest.mark.asyncio
async def test_local_equals_origin_uses_origin_tip(repo_setup: dict[str, Path]) -> None:
  """When local and origin point at the same commit, the worktree starts from origin/<base>."""
  main_checkout = repo_setup["main_checkout"]
  expected = run_git(main_checkout, "rev-parse", "feature")

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
  run_git(seed, "push", "origin", "feature")
  local_tip = run_git(main_checkout, "rev-parse", "feature")
  assert local_tip != origin_tip  # the local branch is behind

  wt_path = repo_setup["tmp_path"] / "wt-behind"
  resolution = await git_create_worktree(main_checkout, "feature", "charliebot/task-behind", wt_path)

  assert _worktree_head(wt_path) == origin_tip
  assert run_git(main_checkout, "rev-parse", resolution.start_point) == origin_tip
  assert run_git(main_checkout, "rev-parse", "feature") == local_tip  # the local ref never moved
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
  run_git(seed, "push", "origin", "feature")  # local feature is now behind: the probe runs

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


@pytest.mark.asyncio
async def test_worktree_add_retries_a_transient_config_lock(
    repo_setup: dict[str, Path], monkeypatch: pytest.MonkeyPatch) -> None:
  """A lock-class failure from a concurrent worktree add is retried: the second
  attempt's result is the one used, and the worktree exists."""
  from src.core import git as git_mod

  main_checkout = repo_setup["main_checkout"]
  expected = run_git(main_checkout, "rev-parse", "feature")
  wt_path = repo_setup["tmp_path"] / "wt-retry"
  real_proc_bytes = git_mod._git_proc_bytes
  calls = {"n": 0}

  async def _lock_once_then_real(repo_path, *args, timeout):
    if args[:2] == ("worktree", "add"):
      calls["n"] += 1
      if calls["n"] == 1:
        return (
            SimpleNamespace(returncode=128),
            b"",
            b"fatal: could not lock config file .git/config: File exists")
      return await real_proc_bytes(repo_path, *args, timeout=timeout)
    return await real_proc_bytes(repo_path, *args, timeout=timeout)

  delays: list[float] = []

  async def _instant_sleep(seconds: float) -> None:
    delays.append(seconds)

  monkeypatch.setattr(git_mod, "_git_proc_bytes", _lock_once_then_real)
  monkeypatch.setattr(asyncio, "sleep", _instant_sleep)

  resolution = await git_create_worktree(main_checkout, "feature", "charliebot/task-retry", wt_path)

  assert calls["n"] == 2
  assert delays == [0.1]
  assert _worktree_head(wt_path) == expected
  assert resolution.canonical == "feature"


@pytest.mark.asyncio
async def test_worktree_add_persistent_lock_error_raises_after_three_retries(
    repo_setup: dict[str, Path], monkeypatch: pytest.MonkeyPatch) -> None:
  """A lock that never clears fails loudly on the last attempt with git's own
  error text, after the full 100ms/200ms/400ms backoff schedule."""
  from src.core import git as git_mod

  main_checkout = repo_setup["main_checkout"]
  wt_path = repo_setup["tmp_path"] / "wt-locked"
  real_proc_bytes = git_mod._git_proc_bytes
  calls = {"n": 0}

  async def _always_locked(repo_path, *args, timeout):
    if args[:2] == ("worktree", "add"):
      calls["n"] += 1
      return (
          SimpleNamespace(returncode=128),
          b"",
          b"fatal: could not lock config file .git/config: File exists")
    return await real_proc_bytes(repo_path, *args, timeout=timeout)

  delays: list[float] = []

  async def _instant_sleep(seconds: float) -> None:
    delays.append(seconds)

  monkeypatch.setattr(git_mod, "_git_proc_bytes", _always_locked)
  monkeypatch.setattr(asyncio, "sleep", _instant_sleep)

  with pytest.raises(RuntimeError) as excinfo:
    await git_create_worktree(main_checkout, "feature", "charliebot/task-locked", wt_path)

  assert calls["n"] == 4  # the first invocation plus 3 retries
  assert delays == [0.1, 0.2, 0.4]
  assert "could not lock config file .git/config: File exists" in str(excinfo.value)


@pytest.mark.asyncio
async def test_worktree_add_non_lock_failure_raises_immediately(
    repo_setup: dict[str, Path], monkeypatch: pytest.MonkeyPatch) -> None:
  """A failure outside the lock class is not retried: it raises on the first
  attempt carrying git's real error."""
  from src.core import git as git_mod

  main_checkout = repo_setup["main_checkout"]
  wt_path = repo_setup["tmp_path"] / "wt-bad-object"
  real_proc_bytes = git_mod._git_proc_bytes
  calls = {"n": 0}

  async def _bad_object(repo_path, *args, timeout):
    if args[:2] == ("worktree", "add"):
      calls["n"] += 1
      return SimpleNamespace(returncode=128), b"", b"fatal: invalid reference: nope"
    return await real_proc_bytes(repo_path, *args, timeout=timeout)

  async def _no_sleep(seconds: float) -> None:
    raise AssertionError("a non-lock failure must not back off")

  monkeypatch.setattr(git_mod, "_git_proc_bytes", _bad_object)
  monkeypatch.setattr(asyncio, "sleep", _no_sleep)

  with pytest.raises(RuntimeError) as excinfo:
    await git_create_worktree(main_checkout, "feature", "charliebot/task-bad-object", wt_path)

  assert calls["n"] == 1
  assert "fatal: invalid reference: nope" in str(excinfo.value)


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

  run_git(tmp_path, "init", "--bare", str(origin))
  run_git(origin, "symbolic-ref", "HEAD", "refs/heads/main")

  run_git(tmp_path, "clone", str(origin), str(seed))
  run_git(seed, "config", "user.email", "test@example.com")
  run_git(seed, "config", "user.name", "Test")
  run_git(seed, "symbolic-ref", "HEAD", "refs/heads/main")
  _commit(seed, "README.md", "seed\n", "seed main")
  run_git(seed, "push", "-u", "origin", "main")

  run_git(tmp_path, "clone", str(origin), str(clone))
  run_git(clone, "config", "user.email", "test@example.com")
  run_git(clone, "config", "user.name", "Test")

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
  run_git(seed, "push", "origin", "main")
  assert run_git(clone, "rev-parse", "main") != origin_tip  # sanity: local main is behind

  run_git(clone, "checkout", "-b", "local-only")

  base = "origin/main"
  wt_path = tmp_path / "wt-baseless"
  resolution = await git_create_worktree(clone, base, "charliebot/task-baseless", wt_path)

  assert run_git(clone, "rev-parse", resolution.start_point) == origin_tip
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
