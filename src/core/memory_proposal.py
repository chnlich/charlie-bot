"""The memory store's PR machinery: the ``proposal`` branch and its worktree.

The daily memory curation drafts into a dedicated git worktree instead of the
live checkout: the store at ``<home>/memory`` stays on its base branch (the
branch HEAD names, read live — never hardcoded), while the sibling worktree
``<home>/memory-proposal`` checks out the ``proposal`` branch whose diff
against base is exactly the content awaiting approval. Only an approved
version — one commit SHA — fast-forwards into the live checkout
(:func:`land`), so what sessions read changes only through that gate.

Every path derives from the live store root the caller passes; the branch name
``proposal`` and the ``-proposal`` worktree suffix are the only fixed names.
All git operations are local subprocesses; a refusal raises
:class:`ProposalRefusalError` with its reason and leaves every ref, the worktree,
and the live checkout exactly as they were.
"""

import subprocess
import tarfile
import tempfile
from collections import Counter
from pathlib import Path, PurePosixPath

from src.core import memory

PROPOSAL_BRANCH = "proposal"
_WORKTREE_SUFFIX = "-proposal"


class ProposalRefusalError(Exception):
  """A refused operation; the reason is the message and nothing was changed."""


def proposal_worktree(memory_dir: Path) -> Path:
  """The proposal worktree path: the live store root's sibling."""
  return memory_dir.parent / f"{memory_dir.name}{_WORKTREE_SUFFIX}"


def _run(cwd: Path, *args: str) -> subprocess.CompletedProcess:
  return subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, encoding="utf-8", check=False)


def _git(cwd: Path, *args: str) -> str:
  """Run ``git <args>`` in *cwd* and return stdout; a failure is a refusal."""
  result = _run(cwd, *args)
  if result.returncode != 0:
    raise ProposalRefusalError(f"git {' '.join(args)} failed in {cwd}: {result.stderr.strip()}")
  return result.stdout


def _base_branch(memory_dir: Path) -> str:
  """The branch the live checkout is on — the PR's base, read live each time."""
  result = _run(memory_dir, "symbolic-ref", "--short", "HEAD")
  if result.returncode != 0:
    raise ProposalRefusalError(
        f"the live memory checkout is not on a branch ({result.stderr.strip()}); "
        "check out the base branch first")
  return result.stdout.strip()


def _ensure_branch(memory_dir: Path, base: str) -> None:
  """Create ``proposal`` at the base head when absent."""
  result = _run(memory_dir, "rev-parse", "--verify", "--quiet", f"refs/heads/{PROPOSAL_BRANCH}")
  if result.returncode != 0:
    base_sha = _git(memory_dir, "rev-parse", base).strip()
    _git(memory_dir, "branch", PROPOSAL_BRANCH, base_sha)


def _validate_worktree(memory_dir: Path, worktree: Path) -> None:
  """A present worktree must be this repo's, checked out on ``proposal``."""
  head_ref = _git(worktree, "symbolic-ref", "--short", "HEAD").strip()
  if head_ref != PROPOSAL_BRANCH:
    raise ProposalRefusalError(f"{worktree} is a worktree on '{head_ref}', not '{PROPOSAL_BRANCH}'")
  worktree_common = (worktree / _git(worktree, "rev-parse", "--git-common-dir").strip()).resolve()
  live_common = (memory_dir / _git(memory_dir, "rev-parse", "--git-common-dir").strip()).resolve()
  if worktree_common != live_common:
    raise ProposalRefusalError(f"{worktree} exists but is not a worktree of the memory repo at {memory_dir}")


def _ensure_worktree(memory_dir: Path, worktree: Path) -> None:
  """Create the worktree when absent; a present one is validated first."""
  if not worktree.exists():
    _git(memory_dir, "worktree", "add", str(worktree), PROPOSAL_BRANCH)


def _ahead_count(memory_dir: Path, base: str) -> int:
  return int(_git(memory_dir, "rev-list", "--count", f"{base}..{PROPOSAL_BRANCH}").strip())


def open_proposal(memory_dir: Path) -> dict[str, str]:
  """Ensure the branch and worktree exist, align them with base, and print status.

  Refuses — leaving everything as it was — when the live checkout carries
  uncommitted tracked changes or is not on a branch, when the worktree belongs
  to another repo or another branch, or when the needed rebase conflicts (the
  abort keeps the branch at its old SHA).
  """
  memory_dir = memory_dir.resolve()
  worktree = proposal_worktree(memory_dir)
  uncommitted = _run(memory_dir, "status", "--porcelain", "--untracked-files=no")
  if uncommitted.stdout.strip():
    raise ProposalRefusalError(
        f"the live memory checkout has uncommitted changes; commit or discard them first:\n"
        f"{uncommitted.stdout.rstrip()}")
  base = _base_branch(memory_dir)
  if worktree.exists():
    # Validate before anything is created, so a refusal leaves the repo as it was.
    _validate_worktree(memory_dir, worktree)
  _ensure_branch(memory_dir, base)
  _ensure_worktree(memory_dir, worktree)
  ahead = _ahead_count(memory_dir, base)
  if ahead == 0:
    # A fresh PR: the branch carries nothing of its own, so align it with base.
    _git(worktree, "merge", "--ff-only", base)
  elif _run(memory_dir, "merge-base", "--is-ancestor", base, PROPOSAL_BRANCH).returncode != 0:
    worktree_status = _run(worktree, "status", "--porcelain")
    if worktree_status.stdout.strip():
      raise ProposalRefusalError(
          f"{base} moved and the proposal worktree has uncommitted changes; commit or "
          "discard them so the branch can rebase")
    rebase = _run(worktree, "rebase", base)
    if rebase.returncode != 0:
      abort = _run(worktree, "rebase", "--abort")
      detail = rebase.stderr.strip()
      if abort.returncode != 0:
        detail += f"; rebase --abort also failed: {abort.stderr.strip()}"
      raise ProposalRefusalError(
          f"rebasing '{PROPOSAL_BRANCH}' onto {base} conflicted; the branch keeps its "
          f"commits: {detail}")
  return status(memory_dir)


def status(memory_dir: Path) -> dict[str, str]:
  """The PR's read-only state as ``key: value`` fields."""
  memory_dir = memory_dir.resolve()
  base = _base_branch(memory_dir)
  missing = _run(memory_dir, "rev-parse", "--verify", "--quiet", f"refs/heads/{PROPOSAL_BRANCH}")
  if missing.returncode != 0:
    raise ProposalRefusalError(
        f"no '{PROPOSAL_BRANCH}' branch in {memory_dir}; run 'charliebot memory proposal open' first")
  head = _git(memory_dir, "rev-parse", PROPOSAL_BRANCH).strip()
  ahead = _ahead_count(memory_dir, base)
  if ahead:
    oldest = _git(memory_dir, "rev-list", f"{base}..{PROPOSAL_BRANCH}").splitlines()[-1]
    opened = _git(memory_dir, "show", "--no-patch", "--date=short", "--format=%cd", oldest).strip()
  else:
    opened = "-"
  worktree = proposal_worktree(memory_dir)
  dirty = ("yes" if _run(worktree, "status", "--porcelain").stdout.strip() else "no") if worktree.exists() else "-"
  return {
      "worktree": str(worktree),
      "base": base,
      "head": head,
      "ahead": str(ahead),
      "opened": opened,
      "dirty": dirty,
      "diff_path": f"/diff?repo={memory_dir}&base={base}&head={head}",
  }


def _rev_lines(memory_dir: Path, rev: str, path: str) -> Counter:
  """The non-blank lines of ``<rev>:<path>``; an absent file counts as empty."""
  listing = _run(memory_dir, "ls-tree", rev, "--", path)
  if listing.returncode != 0:
    raise ProposalRefusalError(f"git ls-tree {rev} -- {path} failed in {memory_dir}: {listing.stderr.strip()}")
  if not listing.stdout.strip():
    return Counter()
  text = _git(memory_dir, "show", f"{rev}:{path}")
  return Counter(line for line in text.split("\n") if line.strip())


def _store_path(path: str) -> PurePosixPath:
  """Validate a store-relative path argument; anything escaping the store is a refusal."""
  rel = PurePosixPath(path)
  if rel.is_absolute() or ".." in rel.parts or not rel.parts:
    raise ProposalRefusalError(f"path must be store-relative (e.g. entries/<topic>/<slug>.md or topics): {path!r}")
  return rel


def commit(memory_dir: Path, path: str, message_file: Path) -> str:
  """Commit exactly one store-relative path on the proposal branch; return the new SHA.

  The guard: every non-blank line the PR already added to this file (the
  multiset ``proposal:<path> - <base>:<path>``) must still be present in the
  worktree's file, so an approved line never silently moves under the
  reviewer's feet. Additions, edits and deletions of the named path commit
  alike; anything else in the worktree stays uncommitted.

  *message_file* resolves against the caller's working directory here: the
  git subprocess runs inside the worktree, so a relative path would otherwise
  read a different file than the caller named.
  """
  memory_dir = memory_dir.resolve()
  rel = _store_path(path)
  message_file = message_file.expanduser().resolve()
  worktree = proposal_worktree(memory_dir)
  if not worktree.exists():
    raise ProposalRefusalError(f"no proposal worktree at {worktree}; run 'charliebot memory proposal open' first")
  base = _base_branch(memory_dir)
  added = _rev_lines(memory_dir, PROPOSAL_BRANCH, path) - _rev_lines(memory_dir, base, path)
  current_file = worktree / Path(*rel.parts)
  current = Counter()
  if current_file.is_file():
    current = Counter(line for line in current_file.read_text(encoding="utf-8").split("\n") if line.strip())
  missing = added - current
  if missing:
    listed = "\n".join(f"  {line}" for line, count in sorted(missing.items()) for _ in range(count))
    raise ProposalRefusalError(
        f"{path}: lines the PR already added are gone from the worktree; restore them "
        f"or resolve the conflict in the report:\n{listed}")
  _git(worktree, "add", "--", path)
  result = _run(worktree, "commit", "--only", "-F", str(message_file), "--", path)
  if result.returncode != 0:
    raise ProposalRefusalError(f"committing {path} failed: {result.stderr.strip()}")
  return _git(worktree, "rev-parse", "HEAD").strip()


def land(memory_dir: Path, sha: str) -> dict[str, str]:
  """Fast-forward the live checkout to one approved proposal commit.

  Every precondition is a refusal: the live checkout must be clean and on a
  branch, *sha* must sit on ``proposal`` with base behind it, and the tree of
  *sha* must pass the store lint (extracted with ``git archive``). The merge
  is ``--ff-only``, so the live checkout lands exactly on the approved tree.
  """
  memory_dir = memory_dir.resolve()
  base = _base_branch(memory_dir)
  uncommitted = _run(memory_dir, "status", "--porcelain")
  if uncommitted.stdout.strip():
    raise ProposalRefusalError(
        f"the live memory checkout is not clean; land needs it clean:\n{uncommitted.stdout.rstrip()}")
  resolved = _run(memory_dir, "rev-parse", "--verify", "--quiet", f"{sha}^{{commit}}")
  if resolved.returncode != 0:
    raise ProposalRefusalError(f"'{sha}' does not resolve to a commit in {memory_dir}")
  commit_sha = resolved.stdout.strip()
  if _run(memory_dir, "merge-base", "--is-ancestor", commit_sha, PROPOSAL_BRANCH).returncode != 0:
    raise ProposalRefusalError(f"'{sha}' is not on the '{PROPOSAL_BRANCH}' branch")
  if _run(memory_dir, "merge-base", "--is-ancestor", base, commit_sha).returncode != 0:
    raise ProposalRefusalError(f"{base} is not an ancestor of '{sha}'; the land would move the branch backwards")
  _lint_tree(memory_dir, commit_sha)
  old_base = _git(memory_dir, "rev-parse", base).strip()
  _git(memory_dir, "merge", "--ff-only", commit_sha)
  commits = _git(memory_dir, "rev-list", "--count", f"{old_base}..{commit_sha}").strip()
  remaining = _git(memory_dir, "rev-list", "--count", f"{commit_sha}..{PROPOSAL_BRANCH}").strip()

  def _short(value: str) -> str:
    return _git(memory_dir, "rev-parse", "--short", value).strip()

  return {
      "landed": f"{_short(old_base)}..{_short(commit_sha)}",
      "commits": commits,
      "remaining": remaining,
  }


def _lint_tree(memory_dir: Path, commit_sha: str) -> None:
  """Lint the tree of *commit_sha*, extracted with git archive; a violation is a refusal."""
  with tempfile.TemporaryDirectory() as tmp:
    tree = Path(tmp) / "tree"
    tree.mkdir()
    archive = Path(tmp) / "tree.tar"
    _git(memory_dir, "archive", "--format=tar", "--output", str(archive), commit_sha)
    with tarfile.open(archive) as tf:
      tf.extractall(tree, filter="data")
    violations = memory.lint(tree)
  if violations:
    raise ProposalRefusalError("the proposed tree fails the store lint:\n" + "\n".join(violations))
