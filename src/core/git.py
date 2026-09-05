"""Shared git helpers — subprocess operations with timeouts and error handling."""

import asyncio
import os
import re
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path

import structlog

from src.core.timeouts import (
    SUBPROCESS_GIT_READ_TIMEOUT_ASYNC,
    SUBPROCESS_GIT_WRITE_TIMEOUT,
)

log = structlog.get_logger()

_WORKTREE_LOCAL_ARTIFACT_NAMES = frozenset({".pixi", ".pixi-cache", ".uv-cache", ".venv", ".local", "build"})


def git_worktree_dir_name(branch_name: str) -> str:
  """Return the directory name CharlieBot uses for a git worktree branch."""
  return branch_name.replace("/", "-")


async def git_current_branch(repo_path: Path) -> str:
  """Get the current branch of the repo."""
  proc = await asyncio.create_subprocess_exec(
      "git",
      "rev-parse",
      "--abbrev-ref",
      "HEAD",
      cwd=str(repo_path),
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
  )
  try:
    stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=SUBPROCESS_GIT_READ_TIMEOUT_ASYNC)
  except TimeoutError as e:
    proc.kill()
    raise RuntimeError(f'git rev-parse timed out after {SUBPROCESS_GIT_READ_TIMEOUT_ASYNC}s in {repo_path}') from e
  if proc.returncode != 0:
    err_msg = stderr.decode().strip()
    if 'unknown revision' in err_msg:
      log.warning('git_empty_repo_fallback', repo=str(repo_path), detail='no commits yet, defaulting to main')
      return 'main'
    raise RuntimeError(f'git rev-parse failed: {err_msg}')
  return stdout.decode().strip()


_FULL_SHA_RE = re.compile(r"[0-9a-f]{40}")


class BaseBranchResolutionError(RuntimeError):
  """Raised when a --base-branch value is ambiguous or unresolvable.

  The strict resolution matrix (resolve_base_branch) deliberately fails loudly
  instead of silently picking a base: a stale local ref as a worktree base is a
  correctness hazard that used to pass unnoticed.
  """


@dataclass(frozen=True)
class BaseResolution:
  """Result of resolving a --base-branch value into a worktree start point."""

  canonical: str  # bare branch name, or the 40-hex SHA itself
  start_point: str  # ref handed to `git worktree add` (origin/<b>, <b>, or the SHA)
  detail: str  # human-readable resolution summary for prompt/log audit


async def _git_stdout(
    repo_path: Path,
    *args: str,
    timeout: int,
    timeout_label: str,
) -> tuple[bool, str, str]:
  """Run a git command with timeout. Returns (success, stdout, stderr)."""
  proc = await asyncio.create_subprocess_exec(
      "git",
      *args,
      cwd=str(repo_path),
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
  )
  try:
    stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=timeout)
  except TimeoutError:
    proc.kill()
    return False, "", f"{timeout_label} timed out after {timeout}s"
  if proc.returncode != 0:
    return False, stdout.decode().strip(), stderr.decode().strip()
  return True, stdout.decode().strip(), ""


async def _git_rev_parse(repo_path: Path, ref: str) -> str | None:
  """Resolve a ref to a commit SHA; return None when it does not resolve."""
  ok, out, _ = await _git_stdout(
      repo_path,
      "rev-parse",
      "--verify",
      "--quiet",
      ref,
      timeout=SUBPROCESS_GIT_READ_TIMEOUT_ASYNC,
      timeout_label="git rev-parse",
  )
  return out if ok and out else None


_SYMREF_PREFIX = "ref: refs/heads/"


def _default_branch_from_ls_remote(out: str, source: str, repo_path: Path) -> str:
  """Parse the symref line of `git ls-remote --symref` output into the default branch name."""
  for line in out.splitlines():
    if line.startswith(_SYMREF_PREFIX):
      branch = line[len(_SYMREF_PREFIX):].split("\t", 1)[0].strip()
      if branch:
        return branch
  raise BaseBranchResolutionError(
      f"{source} in {repo_path} advertised no {_SYMREF_PREFIX}<branch> symref "
      f"line; refusing to infer a default from local refs. Output: {out!r}")


def _ls_remote_ref_sha(out: str, ref: str) -> str | None:
  """The SHA ls-remote reported for exactly *ref*, or None when it listed no such line."""
  for line in out.splitlines():
    sha, _, listed = line.partition("\t")
    if listed.strip() == ref and sha:
      return sha
  return None


async def git_remote_default_branch(repo_path: Path) -> str:
  """Read the remote's published default branch via `git ls-remote --symref origin HEAD`.

  Deliberately never consults refs/remotes/origin/HEAD or any local branch: the
  local symref is clone-time metadata that goes stale silently when upstream
  changes its default branch. Raises BaseBranchResolutionError when origin is
  not configured, unreachable, or advertises no symref -- a caller that cannot
  read the remote's default must fail loudly, never substitute a local ref.
  """
  ok, out, err = await _git_stdout(
      repo_path,
      "ls-remote",
      "--symref",
      "origin",
      "HEAD",
      timeout=SUBPROCESS_GIT_WRITE_TIMEOUT,
      timeout_label="git ls-remote",
  )
  if not ok:
    raise BaseBranchResolutionError(f"cannot read origin's default branch in {repo_path} via git ls-remote: {err}")
  return _default_branch_from_ls_remote(out, "git ls-remote --symref origin HEAD", repo_path)


async def git_remote_default_branch_and_tip(repo_path: Path) -> tuple[str, str | None]:
  """The remote's default branch and its tip SHA from ONE `git ls-remote --symref origin`.

  The unfiltered listing carries the HEAD symref and every branch ref, so a
  caller that needs both the default branch and whether that branch is
  published (plus its tip) pays one network round-trip instead of two. Same
  fail-loud contract as git_remote_default_branch. The tip is None when the
  listing names a default branch without listing its ref — an inconsistent
  remote the caller's base resolution then reports through its own probe.
  """
  ok, out, err = await _git_stdout(
      repo_path,
      "ls-remote",
      "--symref",
      "origin",
      timeout=SUBPROCESS_GIT_WRITE_TIMEOUT,
      timeout_label="git ls-remote",
  )
  if not ok:
    raise BaseBranchResolutionError(f"cannot read origin's default branch in {repo_path} via git ls-remote: {err}")
  branch = _default_branch_from_ls_remote(out, "git ls-remote --symref origin", repo_path)
  return branch, _ls_remote_ref_sha(out, f"refs/heads/{branch}")


async def resolve_base_branch(repo_path: Path, base_branch: str, *, remote_tip: str | None = None) -> BaseResolution:
  """Resolve a --base-branch value to a worktree start point, failing loudly on ambiguity.

  Resolution matrix (the only accepted forms; everything else raises
  BaseBranchResolutionError):
    - 40-hex SHA      → pinned base, verified present; no freshness semantics, no network.
    - origin/<b>      → explicit remote: must exist on origin; start at origin/<b>'s freshly
                        resolved tip (see *remote_tip*) regardless of any local state.
    - <b> (bare)      → if the remote branch exists, the local branch must be absent or
                        point at exactly the same commit (else hard error);
                        start at the freshly resolved origin/<b>. If the remote branch does
                        not exist (or no origin remote is configured), require the branch to
                        exist locally and start there (unpublished-branch case).
  A reachable-check that fails (network/auth) is a hard error, never a silent local
  fallback; use a full SHA to pin a base while offline.

  *remote_tip* is a caller's own fresh `git ls-remote` answer for
  ``refs/heads/<branch>`` (None = the caller did not probe). Supplying it skips
  this function's duplicate ls-remote; the fetch still runs unless the probe
  showed the local remote-tracking ref already holds exactly the tip origin
  advertises — the probe is read straight from the remote, so a fetch could not
  change that ref and the start point keeps the freshly-resolved guarantee.
  """
  raw = (base_branch or "").strip()
  if not raw:
    raise BaseBranchResolutionError("base branch is empty")

  # Full commit SHA: pinned base. Verified by cat-file; no fetch, no remote probe.
  if _FULL_SHA_RE.fullmatch(raw):
    ok, _, err = await _git_stdout(
        repo_path,
        "cat-file",
        "-e",
        f"{raw}^{{commit}}",
        timeout=SUBPROCESS_GIT_READ_TIMEOUT_ASYNC,
        timeout_label="git cat-file",
    )
    if not ok:
      raise BaseBranchResolutionError(f"base commit {raw} not found in repo: {err}")
    return BaseResolution(canonical=raw, start_point=raw, detail=f"pinned commit {raw[:12]}")

  explicit_remote = raw.startswith("origin/")
  branch = raw[len("origin/"):] if explicit_remote else raw
  if not branch or ".." in branch or any(c.isspace() for c in branch):
    raise BaseBranchResolutionError(f"invalid branch name in --base-branch: {raw!r}")

  # Probe origin state: configured? reachable? branch published?
  ok, _, _ = await _git_stdout(
      repo_path,
      "remote",
      "get-url",
      "origin",
      timeout=SUBPROCESS_GIT_READ_TIMEOUT_ASYNC,
      timeout_label="git remote get-url",
  )
  origin_configured = ok
  if origin_configured and remote_tip is None:
    ok, out, ls_err = await _git_stdout(
        repo_path,
        "ls-remote",
        "origin",
        f"refs/heads/{branch}",
        timeout=SUBPROCESS_GIT_WRITE_TIMEOUT,
        timeout_label="git ls-remote",
    )
    if not ok:
      raise BaseBranchResolutionError(
          f"cannot reach origin to resolve base branch {branch!r}: {ls_err}. "
          "Retry when the remote is reachable, or pass a full commit SHA to pin the base.")
    remote_tip = _ls_remote_ref_sha(out, f"refs/heads/{branch}")

  remote_exists = remote_tip is not None
  if remote_exists:
    local_remote_sha = await _git_rev_parse(repo_path, f"refs/remotes/origin/{branch}")
    if remote_tip != local_remote_sha:
      fetched, fetch_err = await git_fetch(repo_path, "origin", branch)
      if not fetched:
        raise BaseBranchResolutionError(f"git fetch origin {branch} failed: {fetch_err}")
    remote_sha = await _git_rev_parse(repo_path, f"refs/remotes/origin/{branch}")
    if remote_sha is None:
      raise BaseBranchResolutionError(f"origin/{branch} listed by ls-remote but missing after fetch in {repo_path}")
    local_sha = await _git_rev_parse(repo_path, f"refs/heads/{branch}")
    if not explicit_remote and local_sha is not None and local_sha != remote_sha:
      raise BaseBranchResolutionError(
          f"local {branch} ({local_sha[:12]}) differs from origin/{branch} ({remote_sha[:12]}). "
          "A local base must match its remote exactly: push the local commits, fast-forward the "
          f"local branch, pass origin/{branch} to use the remote explicitly, "
          "or pass a full commit SHA to pin the base.")
    return BaseResolution(
        canonical=branch, start_point=f"origin/{branch}", detail=f"branch {branch} at origin tip {remote_sha[:12]}")

  # Remote branch absent (or no origin remote configured): unpublished-branch path.
  if explicit_remote:
    raise BaseBranchResolutionError(
        f"origin/{branch} was requested explicitly but that remote branch does not exist on origin.")
  local_sha = await _git_rev_parse(repo_path, f"refs/heads/{branch}")
  if local_sha is None:
    raise BaseBranchResolutionError(
        f"base branch {branch!r} exists neither locally nor on origin "
        f"(origin {'unreachable or not configured' if not origin_configured else 'has no such branch'}).")
  return BaseResolution(
      canonical=branch,
      start_point=branch,
      detail=f"local branch {branch} at {local_sha[:12]} (no matching origin branch)")


async def git_create_worktree(
    repo_path: Path, base_branch: str, branch_name: str, wt_path: Path, *, remote_tip: str | None = None
) -> BaseResolution:
  """Create a git worktree and fail loudly if git reports an error.

  Resolves the start-point via `resolve_base_branch` so that a stale local copy
  of <base_branch> can never silently become a worker's base: ambiguous input
  raises BaseBranchResolutionError before any worktree is created. Returns the
  BaseResolution so callers can persist the canonical (bare) branch name and
  audit the chosen start point. *remote_tip* forwards to resolve_base_branch
  (see there); it must be the caller's own fresh ls-remote answer for
  ``refs/heads/<branch>``.
  """
  resolution = await resolve_base_branch(repo_path, base_branch, remote_tip=remote_tip)
  start_point = resolution.start_point
  proc = await asyncio.create_subprocess_exec(
      "git",
      "worktree",
      "add",
      "-b",
      branch_name,
      str(wt_path),
      start_point,
      cwd=str(repo_path),
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
  )
  try:
    stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=SUBPROCESS_GIT_WRITE_TIMEOUT)
  except TimeoutError as e:
    proc.kill()
    raise RuntimeError(f'git worktree add timed out after {SUBPROCESS_GIT_WRITE_TIMEOUT}s for {branch_name}') from e
  if proc.returncode != 0:
    out = stdout.decode().strip()
    err = stderr.decode().strip()
    log.error(
        "spawn_worker_worktree_create_failed",
        repo=str(repo_path),
        branch=branch_name,
        worktree=str(wt_path),
        base_branch=base_branch,
        start_point=start_point,
        stdout=out,
        stderr=err,
        returncode=proc.returncode,
    )
    raise RuntimeError(f"git worktree add failed for {branch_name}: {err or out or 'unknown error'}")
  log.info(
      "git_worktree_created",
      repo=str(repo_path),
      base=base_branch,
      canonical=resolution.canonical,
      start_point=start_point,
      resolution=resolution.detail,
      worktree=str(wt_path),
  )
  return resolution


def _is_relative_to(path: Path, parent: Path) -> bool:
  try:
    path.relative_to(parent)
  except ValueError:
    return False
  return True


def _assert_safe_worktree_cleanup_target(
    repo_path: str,
    wt_path: Path,
    *,
    allowed_parent: Path,
    expected_residue_name: str,
) -> Path:
  """Validate the worktree path before removing anything inside it."""
  if wt_path.name != expected_residue_name:
    raise RuntimeError(
        f"refusing to clean worktree at {wt_path}: "
        f"directory name does not match expected {expected_residue_name}")
  if wt_path.is_symlink():
    raise RuntimeError(f"refusing to clean worktree symlink: {wt_path}")

  resolved_wt = wt_path.resolve()
  resolved_allowed_parent = allowed_parent.resolve()
  resolved_repo = Path(repo_path).resolve()
  home = Path.home().resolve()
  if resolved_wt in {Path("/"), home}:
    raise RuntimeError(f"refusing to clean unsafe worktree path: {wt_path}")
  if resolved_wt == resolved_repo:
    raise RuntimeError(f"refusing to clean repo root as worktree: {wt_path}")
  if resolved_wt == resolved_allowed_parent or not _is_relative_to(resolved_wt, resolved_allowed_parent):
    raise RuntimeError(f"refusing to clean worktree outside allowed parent {resolved_allowed_parent}: {wt_path}")
  return resolved_wt


def _contains_git_marker(path: Path) -> bool:
  """Return True if path contains any .git marker without following symlinks."""
  for _dirpath, dirnames, filenames in os.walk(path, followlinks=False):
    if ".git" in filenames or ".git" in dirnames:
      return True
  return False


def _iter_local_worktree_artifacts(wt_path: Path):
  """Yield paths of known cache/env/build artifact dirs inside the worktree.

  Walks top-down without following symlinks and never descends into a nested git
  worktree (a non-root dir holding a .git marker), so only this worktree's own
  artifacts are surfaced. Artifact dirs are not descended into.
  """
  for dirpath, dirnames, filenames in os.walk(wt_path, topdown=True, followlinks=False):
    if Path(dirpath) != wt_path and (".git" in filenames or ".git" in dirnames):
      dirnames[:] = []
      continue
    dirnames[:] = [name for name in dirnames if name != ".git"]
    for name in [name for name in dirnames if name in _WORKTREE_LOCAL_ARTIFACT_NAMES]:
      yield Path(dirpath) / name
    dirnames[:] = [name for name in dirnames if name not in _WORKTREE_LOCAL_ARTIFACT_NAMES]


async def _remove_local_worktree_artifacts(wt_path: Path, thread_id: str) -> None:
  """Pre-clean known local cache/env/build artifacts inside the worktree.

  A symlinked artifact (e.g. `.venv -> <main checkout>/.venv`) is skipped rather
  than treated as an error: the pre-clean exists to avoid an expensive recursive
  delete inside the worktree, while a symlink is one cheap directory entry that
  `git worktree remove --force` deletes on its own, and deleting a symlink never
  touches its target.
  """
  for artifact_path in _iter_local_worktree_artifacts(wt_path):
    if artifact_path.is_symlink():
      log.warning("worktree_artifact_symlink_skipped", thread_id=thread_id, path=str(artifact_path))
      continue
    await asyncio.to_thread(shutil.rmtree, artifact_path)
    log.info("worktree_local_artifact_removed", thread_id=thread_id, path=str(artifact_path))


async def _strip_local_worktree_artifacts_best_effort(wt_path: Path, thread_id: str) -> None:
  """Best-effort removal of regenerable artifacts before quarantine.

  Unlike _remove_local_worktree_artifacts, this never raises: artifacts that can't be
  removed (e.g. root-owned files) are logged and left in place for the move to carry.
  Errors are surfaced via log.warning, never swallowed silently.
  """
  for artifact_path in _iter_local_worktree_artifacts(wt_path):
    if artifact_path.is_symlink():
      log.warning("quarantine_artifact_symlink_skipped", thread_id=thread_id, path=str(artifact_path))
      continue
    try:
      await asyncio.to_thread(shutil.rmtree, artifact_path)
      log.info("worktree_local_artifact_removed", thread_id=thread_id, path=str(artifact_path))
    except OSError as e:
      log.warning("quarantine_artifact_remove_failed", thread_id=thread_id, path=str(artifact_path), error=str(e))


async def _remove_worktree_residue(
    wt_path: Path,
    thread_id: str,
    *,
    repo_path: str,
    allowed_parent: Path,
    expected_residue_name: str,
) -> None:
  """Delete a git-detached residual worktree directory after explicit safety checks."""
  _assert_safe_worktree_cleanup_target(
      repo_path,
      wt_path,
      allowed_parent=allowed_parent,
      expected_residue_name=expected_residue_name,
  )
  if not wt_path.is_dir():
    raise RuntimeError(f"refusing to remove non-directory worktree residue: {wt_path}")
  if _contains_git_marker(wt_path):
    raise RuntimeError(f"refusing to remove worktree residue with .git marker: {wt_path}")

  await asyncio.to_thread(shutil.rmtree, wt_path)
  if os.path.lexists(wt_path):
    raise RuntimeError(f"failed to remove worktree residue: {wt_path}")
  log.info("worktree_residue_removed", thread_id=thread_id, path=str(wt_path))


async def git_worktree_remove(
    repo_path: str,
    wt_path: Path,
    thread_id: str,
    *,
    allowed_parent: Path,
    expected_residue_name: str,
) -> bool:
  """Remove a git worktree and any safe git-detached residual directory."""
  _assert_safe_worktree_cleanup_target(
      repo_path,
      wt_path,
      allowed_parent=allowed_parent,
      expected_residue_name=expected_residue_name,
  )
  if os.path.lexists(wt_path):
    if wt_path.is_symlink():
      raise RuntimeError(f"refusing to remove worktree symlink: {wt_path}")
    if not wt_path.is_dir():
      raise RuntimeError(f"refusing to remove non-directory worktree: {wt_path}")
    await _remove_local_worktree_artifacts(wt_path, thread_id)

  proc = await asyncio.create_subprocess_exec(
      "git",
      "worktree",
      "remove",
      "--force",
      str(wt_path),
      cwd=repo_path,
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
  )
  try:
    _, stderr = await asyncio.wait_for(proc.communicate(), timeout=SUBPROCESS_GIT_READ_TIMEOUT_ASYNC)
  except TimeoutError:
    proc.kill()
    log.warning("worktree_remove_timeout", thread_id=thread_id, path=str(wt_path))
    return False
  if proc.returncode != 0:
    log.warning("worktree_remove_failed", thread_id=thread_id, stderr=stderr.decode().strip())
    return False
  if os.path.lexists(wt_path):
    await _remove_worktree_residue(
        wt_path,
        thread_id,
        repo_path=repo_path,
        allowed_parent=allowed_parent,
        expected_residue_name=expected_residue_name,
    )
  log.info("worktree_removed", thread_id=thread_id, path=str(wt_path))
  return True


async def git_worktree_remove_reporting(
    repo_path: str | None,
    worktree_path: str | None,
    branch_name: str | None,
    owner_id: str,
    worktree_parent: Path,
    *,
    log_fields: dict,
    label: str,
    fail_event: str,
    remove_failed_event: str,
) -> str | None:
  """Remove a finished flow's worktree, reporting failure as a message.

  Returns an error message string when the remove fails so the caller can surface
  it in its own flow, or None on success and when the flow has no worktree to
  clean. ``label``, ``log_fields``, and the two structlog event names carry the
  caller's flow identity (worker finalize, review chain, improve loop);
  ``owner_id`` feeds the remove/prune primitives' thread_id log field. A
  worktree without a branch raises instead of reporting: the residue name and
  the prune both derive from the branch, so a missing one is corrupt state, not
  a cleanup miss.
  """
  if not repo_path or not worktree_path:
    return None
  wt = Path(worktree_path)
  if not wt.exists():
    return None
  if not branch_name:
    raise RuntimeError(f"owner {owner_id} has worktree_path but no branch_name")
  try:
    removed = await git_worktree_remove(
        repo_path,
        wt,
        owner_id,
        allowed_parent=worktree_parent,
        expected_residue_name=git_worktree_dir_name(branch_name),
    )
  except Exception as e:
    log.exception(fail_event, worktree=str(wt), error=str(e), **log_fields)
    return f"{label} cleanup failed for {wt}: {e}"
  if not removed:
    log.error(remove_failed_event, worktree=str(wt), **log_fields)
    return f"{label} cleanup failed for {wt}: git worktree remove reported failure"
  await git_worktree_prune(repo_path, owner_id)
  return None


async def git_quarantine_worktree(
    repo_path: str,
    wt_path: Path,
    thread_id: str,
    *,
    allowed_parent: Path,
    expected_residue_name: str,
    trash_dir: Path,
) -> Path:
  """Quarantine a stale worktree by moving it into trash_dir, then prune git's refs.

  Unlike git_worktree_remove (rm-based, which fails on root-owned files), this is
  move-based: it best-effort strips regenerable caches, then renames the remaining
  directory into trash_dir on the same filesystem. The rename only needs write
  permission on the parent dirs, so it succeeds even when the worktree still holds
  root-owned files, and it keeps the inode so files held open by another process are
  unaffected -- this permission/runtime robustness is the whole point of quarantine.
  All non-cache contents (code, diff, logs, outputs) are preserved. Returns the final
  trash path.
  """
  _assert_safe_worktree_cleanup_target(
      repo_path,
      wt_path,
      allowed_parent=allowed_parent,
      expected_residue_name=expected_residue_name,
  )
  await _strip_local_worktree_artifacts_best_effort(wt_path, thread_id)

  await asyncio.to_thread(trash_dir.mkdir, parents=True, exist_ok=True)
  dest = trash_dir / wt_path.name
  if os.path.lexists(dest):
    # Collision with a previously quarantined worktree of the same branch name.
    dest = trash_dir / f"{wt_path.name}-{thread_id}"
    if os.path.lexists(dest):
      raise RuntimeError(f"refusing to quarantine over existing trash entry: {dest}")

  await asyncio.to_thread(shutil.move, str(wt_path), str(dest))
  await git_worktree_prune(repo_path, thread_id)
  log.info("worktree_quarantined", thread_id=thread_id, src=str(wt_path), dest=str(dest))
  return dest


async def git_worktree_prune(repo_path: str, thread_id: str) -> None:
  """Prune stale worktree refs."""
  prune_proc = await asyncio.create_subprocess_exec(
      "git",
      "worktree",
      "prune",
      cwd=repo_path,
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
  )
  try:
    await asyncio.wait_for(prune_proc.communicate(), timeout=SUBPROCESS_GIT_READ_TIMEOUT_ASYNC)
  except TimeoutError:
    prune_proc.kill()
    log.warning("worktree_prune_timeout", thread_id=thread_id)


async def _run_git_cmd(
    repo_path: Path,
    *args: str,
    timeout: int,
    timeout_label: str,
) -> tuple[bool, str]:
  """Run a git command with timeout. Returns (success, stderr)."""
  ok, _, stderr = await _git_stdout(repo_path, *args, timeout=timeout, timeout_label=timeout_label)
  return ok, stderr


async def git_fetch(repo_path: Path, remote: str, branch: str) -> tuple[bool, str]:
  """Run git fetch <remote> <branch>. Returns (success, stderr)."""
  return await _run_git_cmd(
      repo_path,
      "fetch",
      remote,
      branch,
      timeout=SUBPROCESS_GIT_WRITE_TIMEOUT,
      timeout_label="git fetch",
  )


async def git_push_branch(repo_path: Path, branch: str) -> tuple[bool, str]:
  """Run git push origin <branch>. Returns (success, stderr)."""
  return await _run_git_cmd(
      repo_path,
      "push",
      "origin",
      branch,
      timeout=SUBPROCESS_GIT_WRITE_TIMEOUT,
      timeout_label="git push",
  )


async def git_push_refspec(repo_path: Path, local_branch: str, remote_branch: str) -> tuple[bool, str]:
  """Run git push origin <local>:refs/heads/<remote>. Returns (success, stderr).

  Git rejects non-fast-forward pushes by default, so this is implicitly an FF-only push.
  """
  return await _run_git_cmd(
      repo_path,
      "push",
      "origin",
      f"{local_branch}:refs/heads/{remote_branch}",
      timeout=SUBPROCESS_GIT_WRITE_TIMEOUT,
      timeout_label="git push refspec",
  )


async def git_add_commit_push(
    repo_path: Path,
    files: list[str],
    message: str,
    timeout: int = SUBPROCESS_GIT_WRITE_TIMEOUT,
) -> None:
  """Git add, commit, and push with subprocess timeouts.

  Runs each git command in a thread-pool worker with a per-command timeout.
  Catches CalledProcessError and TimeoutExpired consistently, logging warnings
  rather than propagating.

  Args:
    repo_path: Working directory for git commands.
    files: Paths (relative to repo_path) to stage.
    message: Commit message.
    timeout: Per-command timeout in seconds.
  """

  def _run() -> None:
    subprocess.run(['git', 'add', *files], cwd=repo_path, check=True, capture_output=True, timeout=timeout)
    subprocess.run(['git', 'commit', '-m', message], cwd=repo_path, check=True, capture_output=True, timeout=timeout)
    subprocess.run(['git', 'push'], cwd=repo_path, check=True, capture_output=True, timeout=timeout)

  try:
    await asyncio.to_thread(_run)
  except subprocess.CalledProcessError as e:
    log.warning('git_commit_push_failed', cmd=e.cmd, stderr=e.stderr.decode(errors='replace'))
  except subprocess.TimeoutExpired as e:
    log.warning('git_commit_push_timeout', cmd=e.cmd, timeout=e.timeout)
  except Exception as e:
    log.warning('git_commit_push_error', error=str(e))
