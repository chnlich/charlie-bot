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
from typing import Any

import pytest
from conftest import build_codex_worktree_cfg

from src.core import spawner, spawner_launch
from src.core.git import (
    BaseBranchResolutionError,
    BaseResolution,
    git_create_worktree,
    git_current_branch,
    git_remote_default_branch,
)
from src.core.models import SessionMetadata, SpawnRequest, ThreadMetadata


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
    await git_create_worktree(main_checkout, "feature", "charliebot/task-behind", repo_setup["tmp_path"] / "wt-behind")
  _expect_hard_error(excinfo, "differs from origin/feature", local_tip[:12], origin_tip[:12], "origin/feature")


@pytest.mark.asyncio
async def test_local_ahead_of_origin_raises(repo_setup: dict[str, Path]) -> None:
  """Unpushed local commits must never silently become the base: local ahead is a hard error."""
  main_checkout = repo_setup["main_checkout"]

  origin_tip = _git(main_checkout, "rev-parse", "origin/feature")
  local_tip = _commit(main_checkout, "local_only.txt", "ahead\n", "unpushed local commit")
  assert local_tip != origin_tip

  with pytest.raises(BaseBranchResolutionError) as excinfo:
    await git_create_worktree(main_checkout, "feature", "charliebot/task-ahead", repo_setup["tmp_path"] / "wt-ahead")
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
    await git_create_worktree(main_checkout, "0" * 40, "charliebot/task-badsha", repo_setup["tmp_path"] / "wt-badsha")
  _expect_hard_error(excinfo, "not found")


@pytest.mark.asyncio
@pytest.mark.parametrize("raw", ["", "   ", "origin/", "has space", "has..dots"])
async def test_garbage_input_raises(repo_setup: dict[str, Path], raw: str) -> None:
  main_checkout = repo_setup["main_checkout"]
  with pytest.raises(BaseBranchResolutionError):
    await git_create_worktree(main_checkout, raw, "charliebot/task-garbage", repo_setup["tmp_path"] / "wt-garbage")


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
async def test_remote_default_branch_read_from_remote(remote_default_repo: dict[str, Path]) -> None:
  """The default branch comes from the remote, so clone-time metadata cannot
  make a task silently build on the wrong branch."""
  origin = remote_default_repo["origin"]
  seed = remote_default_repo["seed"]
  clone = remote_default_repo["clone"]

  assert await git_remote_default_branch(clone) == "main"

  # Publish a second branch and repoint the remote's HEAD at it.
  _git(seed, "checkout", "-b", "develop")
  _git(seed, "push", "-u", "origin", "develop")
  _git(origin, "symbolic-ref", "HEAD", "refs/heads/develop")

  # The clone's own symref still points at the old default (it is written at
  # clone time only); the helper must follow the remote's new value regardless.
  assert _git(clone, "symbolic-ref", "refs/remotes/origin/HEAD") == "refs/remotes/origin/main"
  assert await git_remote_default_branch(clone) == "develop"


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

  base = f"origin/{await git_remote_default_branch(clone)}"
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


class _SpawnSessionManager:
  """Minimal session manager for driving spawner._create_worktree_and_process."""

  async def get_session(self, session_id: str) -> SessionMetadata:
    return SessionMetadata(id=session_id, name="Test Session")


class _SpawnThreadManager:
  """Minimal thread manager for driving spawner._create_worktree_and_process."""

  def __init__(self, events_log: Path) -> None:
    self._events_log = events_log

  async def save_metadata(self, meta: ThreadMetadata) -> None:
    return None

  async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
    return self._events_log


async def _run_spawn_request(
    clone: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    request: SpawnRequest,
    *,
    thread_id: str,
) -> ThreadMetadata:
  """Drive _create_worktree_and_process end-to-end with real git (no git mocking).

  The local-branch read is tripwired: if the launch fallback ever consults
  git_current_branch again, the AssertionError fails the test.
  """

  async def _forbidden_current_branch(repo_path: Path) -> str:
    raise AssertionError("launch fallback consulted git_current_branch (the local checkout)")

  # raising=False: the launch path does not import the symbol at all today, and a
  # regression that re-adds the call re-adds the binding this tripwire then catches.
  monkeypatch.setattr(spawner_launch, "git_current_branch", _forbidden_current_branch, raising=False)

  thread = ThreadMetadata(id=thread_id, session_id="session-id", description="Do work")
  await spawner._create_worktree_and_process(
      "session-id",
      thread,
      "Do work",
      build_codex_worktree_cfg(tmp_path),
      _SpawnSessionManager(),
      _SpawnThreadManager(tmp_path / f"events-{thread_id}.jsonl"),
      clone,
      request,
  )
  return thread


@pytest.mark.asyncio
async def test_baseless_spawn_request_never_reads_local_branch(
    remote_default_repo: dict[str, Path],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """A base-less SpawnRequest records the remote's default branch as the base and
  never consults the local checkout, even when local main is behind origin."""
  seed = remote_default_repo["seed"]
  clone = remote_default_repo["clone"]

  _commit(seed, "advance.txt", "advance\n", "advance origin main")
  _git(seed, "push", "origin", "main")  # local main is now behind origin

  thread = await _run_spawn_request(
      clone,
      remote_default_repo["tmp_path"],
      monkeypatch,
      SpawnRequest(
          repo_path=str(clone),
          base_branch=None,
          resolved_backend="codex-o3",
          resolved_model="o3",
      ),
      thread_id="thread-baseless",
  )

  assert thread.base_branch == await git_remote_default_branch(clone)


@pytest.mark.asyncio
async def test_crash_respawn_reaches_the_same_fallback(
    remote_default_repo: dict[str, Path],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """Crash-respawn of a scheduled task whose worktree never existed: the rebuilt
  request carries no base (init.py's `invocation.get(...) or meta.get(...)` is
  None), so respawn and first launch share the same fallback."""
  clone = remote_default_repo["clone"]

  invocation: dict[str, Any] = {}
  meta: dict[str, Any] = {"base_branch": None}
  request = SpawnRequest(
      repo_path=str(clone),
      base_branch=invocation.get("base_branch") or meta.get("base_branch"),
      resolved_backend="codex-o3",
      resolved_model="o3",
  )
  assert request.base_branch is None

  thread = await _run_spawn_request(
      clone,
      remote_default_repo["tmp_path"],
      monkeypatch,
      request,
      thread_id="thread-respawn",
  )

  assert thread.base_branch == await git_remote_default_branch(clone)


# --- probe-fed resolution: the ls-remote answer replaces the duplicate probe + no-op fetch ---


@pytest.mark.asyncio
async def test_probe_equal_tracking_ref_skips_fetch(repo_setup: dict[str, Path], monkeypatch: pytest.MonkeyPatch) -> None:
  """When the ls-remote probe shows the local remote-tracking ref already holds the
  tip origin advertises, the fetch is provably a no-op and must not run: the probe
  is read straight from the remote, so no fetch could change that ref."""
  from src.core import git as git_mod

  main_checkout = repo_setup["main_checkout"]
  tip = _git(main_checkout, "rev-parse", "origin/feature")

  async def _forbidden_fetch(repo_path: Path, remote: str, branch: str) -> tuple[bool, str]:
    raise AssertionError(f"git fetch ran although the probe showed the tracking ref current ({remote} {branch})")

  monkeypatch.setattr(git_mod, "git_fetch", _forbidden_fetch)

  resolution = await git_mod.resolve_base_branch(main_checkout, "origin/feature")
  assert resolution.start_point == "origin/feature"
  assert _git(main_checkout, "rev-parse", resolution.start_point) == tip


@pytest.mark.asyncio
async def test_probe_mismatch_still_fetches(repo_setup: dict[str, Path], monkeypatch: pytest.MonkeyPatch) -> None:
  """When the probe shows origin ahead of the tracking ref, the fetch runs and the
  start point is the freshly fetched tip."""
  from src.core import git as git_mod

  seed = repo_setup["seed"]
  main_checkout = repo_setup["main_checkout"]

  new_tip = _commit(seed, "advance.txt", "advance\n", "advance origin")
  _git(seed, "push", "origin", "feature")
  assert _git(main_checkout, "rev-parse", "origin/feature") != new_tip  # sanity: tracking ref behind

  real_fetch = git_mod.git_fetch
  calls: list[tuple[str, str]] = []

  async def _counting_fetch(repo_path: Path, remote: str, branch: str) -> tuple[bool, str]:
    calls.append((remote, branch))
    return await real_fetch(repo_path, remote, branch)

  monkeypatch.setattr(git_mod, "git_fetch", _counting_fetch)

  resolution = await git_mod.resolve_base_branch(main_checkout, "origin/feature")
  assert calls == [("origin", "feature")]
  assert _git(main_checkout, "rev-parse", resolution.start_point) == new_tip


@pytest.mark.asyncio
async def test_caller_supplied_tip_skips_the_probe(repo_setup: dict[str, Path], monkeypatch: pytest.MonkeyPatch) -> None:
  """A remote_tip from the caller's own ls-remote replaces this function's probe:
  no ls-remote runs, the equality check still gates the fetch."""
  from src.core import git as git_mod

  main_checkout = repo_setup["main_checkout"]
  tip = _git(main_checkout, "rev-parse", "origin/feature")

  real_stdout = git_mod._git_stdout

  async def _no_ls_remote(repo_path: Path, *args: str, **kwargs: Any) -> tuple[bool, str, str]:
    if "ls-remote" in args:
      raise AssertionError(f"resolve_base_branch probed ls-remote despite a caller-supplied tip: {args}")
    return await real_stdout(repo_path, *args, **kwargs)

  monkeypatch.setattr(git_mod, "_git_stdout", _no_ls_remote)

  resolution = await git_mod.resolve_base_branch(main_checkout, "origin/feature", remote_tip=tip)
  assert resolution.start_point == "origin/feature"
  assert _git(main_checkout, "rev-parse", resolution.start_point) == tip


@pytest.mark.asyncio
async def test_remote_default_branch_and_tip_reads_both_from_one_call(
    remote_default_repo: dict[str, Path],
) -> None:
  """The combined ls-remote returns the remote's default branch and that branch's
  tip, following the remote when its HEAD repoints (never clone-time metadata)."""
  from src.core.git import git_remote_default_branch_and_tip

  origin = remote_default_repo["origin"]
  seed = remote_default_repo["seed"]
  clone = remote_default_repo["clone"]

  branch, tip = await git_remote_default_branch_and_tip(clone)
  assert branch == "main"
  assert tip == _git(origin, "rev-parse", "refs/heads/main")

  _commit(seed, "advance.txt", "advance\n", "advance origin main")
  _git(seed, "push", "origin", "main")

  branch, tip = await git_remote_default_branch_and_tip(clone)
  assert branch == "main"
  assert tip == _git(origin, "rev-parse", "refs/heads/main")

  # Repoint the remote's HEAD at develop; the combined call follows it.
  _git(seed, "checkout", "-b", "develop")
  _git(seed, "push", "-u", "origin", "develop")
  _git(origin, "symbolic-ref", "HEAD", "refs/heads/develop")

  branch, tip = await git_remote_default_branch_and_tip(clone)
  assert branch == "develop"
  assert tip == _git(origin, "rev-parse", "refs/heads/develop")
