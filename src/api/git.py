"""Git-related API routes."""

import asyncio
import subprocess
from pathlib import Path
from typing import Literal

from fastapi import APIRouter, Depends, HTTPException, Query

from src.core.config import CharlieBotConfig, get_config
from src.core.memo import BoundedMemo
from src.core.timeouts import SUBPROCESS_GIT_DIFF_TIMEOUT, SUBPROCESS_GIT_READ_TIMEOUT

router = APIRouter()

# Per-file diff cap (bytes). A single file whose diff exceeds this is returned as
# a content-free stub so one giant generated/lockfile/binary file can't wedge the page.
_DIFF_MAX_BYTES = 5 * 1024 * 1024

# Bound on _diff_files_memo in manifest entries: a /diff page watches one
# freshly-resolved range at a time, so the cap covers every branch pair open
# across tabs.
_DIFF_FILES_MEMO_LIMIT = 64

# Memo key for one diff/files manifest: repo, both resolved SHAs, range mode,
# and the .gitattributes signature (git reads diff drivers from the worktree
# attributes file, and they feed --numstat's line counts). The remaining pieces
# of repo state (refs) enter the key through the resolved SHAs; host git config
# (rename detection) is static on this host.
_ManifestKey = tuple[str, str, str, str, tuple[int, int] | None]

# (repo, base_sha, head_sha, mode, attributes signature) -> (rows,
# total_additions, total_deletions). A diff between two commits is immutable —
# SHAs are content-addressed — so a repeat view of the same resolved range
# (every /diff refresh until a branch moves) re-runs zero git diff
# subprocesses. Served rows are shared across responses, the no-defensive-copy
# idiom of the sibling memos.
_diff_files_memo: BoundedMemo[_ManifestKey, tuple[list[dict], int, int]] = BoundedMemo(_DIFF_FILES_MEMO_LIMIT)

# Bound on _diff_file_memo in per-file bodies: a review pass expands files one
# at a time across the branch pairs open in tabs, and an evicted entry simply
# re-runs the diff.
_DIFF_FILE_MEMO_LIMIT = 64

# Memo key for one diff/file body: the manifest key (see _ManifestKey) plus the
# pathspec the handler passes git. Same immutability argument as the manifest:
# repeat expands — expand/collapse re-fetches, tab refreshes — of one resolved
# ref pair re-run zero git diff subprocesses.
_FileDiffKey = tuple[str, str, str, str, tuple[int, int] | None, tuple[str, ...]]

_diff_file_memo: BoundedMemo[_FileDiffKey, str] = BoundedMemo(_DIFF_FILE_MEMO_LIMIT)


def _attributes_signature(repo_path: Path) -> tuple[int, int] | None:
  """Return (mtime_ns, size) of the repo's .gitattributes, or None when it has none."""
  try:
    st = (repo_path / ".gitattributes").stat()
  except FileNotFoundError:
    return None
  return (st.st_mtime_ns, st.st_size)


def _resolve_repo_under_workspace(repo: str, cfg: CharlieBotConfig) -> Path:
  """Validate repo path and return resolved Path; raise HTTPException(400) otherwise."""
  repo_path = Path(repo).expanduser().resolve()
  if not (repo_path / ".git").exists():
    raise HTTPException(status_code=400, detail=f"Not a git repo: {repo}")
  workspace_roots = [Path(d).expanduser().resolve() for d in cfg.workspace_dirs]
  if not any(repo_path.is_relative_to(root) for root in workspace_roots):
    raise HTTPException(status_code=400, detail="repo must be under configured workspace_dirs")
  return repo_path


def _range_spec(base: str, head: str, mode: str) -> str:
  """Build the `git diff` range expression for the requested mode."""
  return f"{base}...{head}" if mode == "three-dot" else f"{base}..{head}"


def _run_git_sync(repo_path: Path, args: list[str], timeout: float, failure_detail: str) -> str:
  """Run `git <args>` in repo_path and return stdout; raise HTTPException(500) on failure."""
  result = subprocess.run(
      ["git", *args],
      cwd=repo_path,
      capture_output=True,
      text=True,
      check=False,
      timeout=timeout,
  )
  if result.returncode != 0:
    raise HTTPException(status_code=500, detail=result.stderr.strip() or failure_detail)
  return result.stdout


def _run_git_diff_sync(repo_path: Path, args: list[str]) -> str:
  """Run `git diff <args>` in repo_path and return stdout; raise HTTPException(500) on failure."""
  return _run_git_sync(repo_path, ["diff", *args], SUBPROCESS_GIT_DIFF_TIMEOUT, "git diff failed")


async def _run_git_diff(repo_path: Path, args: list[str]) -> str:
  """Async front for the blocking diff; a subprocess that may hold for
  SUBPROCESS_GIT_DIFF_TIMEOUT seconds stays off the event loop."""
  return await asyncio.to_thread(_run_git_diff_sync, repo_path, args)


def _resolve_commits_sync(repo_path: Path, refs: list[str]) -> list[str]:
  """Resolve each git ref to its full commit SHA, in order, in one rev-parse call."""
  stdout = _run_git_sync(
      repo_path, ["rev-parse", *[f"{ref}^{{commit}}" for ref in refs]], SUBPROCESS_GIT_READ_TIMEOUT,
      "git rev-parse failed")
  return stdout.split()


async def _resolve_commits(repo_path: Path, refs: list[str]) -> list[str]:
  """Async front for the blocking rev-parse; keeps the lookup off the event loop."""
  return await asyncio.to_thread(_resolve_commits_sync, repo_path, refs)


def _parse_numstat_z(output: str) -> list[tuple[int, int, str, str | None]]:
  """Parse `git diff --numstat -z` into (additions, deletions, new_path, old_path) tuples.

  A normal entry is one NUL-terminated `added\\tdeleted\\tpath` record (old_path None). A
  rename/copy is `added\\tdeleted\\t` (empty path) followed by two NUL tokens (old, new);
  both paths are reported so the per-file diff can pass both pathspecs and render the
  rename as a rename rather than a wholesale add. Binary files emit `-` for the counts,
  reported here as 0.
  """
  tokens = output.split("\0")
  entries: list[tuple[int, int, str, str | None]] = []
  i = 0
  while i < len(tokens):
    token = tokens[i]
    if token == "":
      i += 1
      continue
    added_s, deleted_s, path = token.split("\t", 2)
    old_path: str | None = None
    if path == "":
      # Rename/copy: the following two tokens are the old and new paths.
      old_path = tokens[i + 1]
      path = tokens[i + 2]
      i += 3
    else:
      i += 1
    additions = 0 if added_s == "-" else int(added_s)
    deletions = 0 if deleted_s == "-" else int(deleted_s)
    entries.append((additions, deletions, path, old_path))
  return entries


def _parse_name_status_z(output: str) -> dict[str, str]:
  """Parse `git diff --name-status -z` into {path: status_letter}.

  For renames/copies the status is `Rxxx`/`Cxxx` followed by old and new path tokens;
  the new path is keyed with the leading status letter ('R'/'C').
  """
  tokens = output.split("\0")
  status_by_path: dict[str, str] = {}
  i = 0
  while i < len(tokens):
    status = tokens[i]
    if status == "":
      i += 1
      continue
    if status[0] in ("R", "C"):
      status_by_path[tokens[i + 2]] = status[0]
      i += 3
    else:
      status_by_path[tokens[i + 1]] = status[0]
      i += 2
  return status_by_path


def _list_branches_sync(repo_path: Path) -> str:
  """Run `git branch -a` in repo_path and return stdout; raise HTTPException(500) on failure."""
  return _run_git_sync(
      repo_path,
      ["branch", "-a", "--sort=-committerdate", "--format=%(refname:short)"],
      SUBPROCESS_GIT_READ_TIMEOUT,
      "git branch failed",
  )


@router.get("/branches")
async def list_branches(repo: str = Query(..., description="Full path to git repo")):
  """Return branch names for a repo, most recent first, up to 50."""
  repo_path = Path(repo).expanduser()
  if not (repo_path / ".git").exists() and not repo_path.name == ".git":
    raise HTTPException(status_code=400, detail=f"Not a git repo: {repo}")
  stdout = await asyncio.to_thread(_list_branches_sync, repo_path)
  seen: set[str] = {"HEAD"}
  branches: list[str] = ["HEAD"]
  for line in stdout.splitlines():
    name = line.strip()
    if not name or name in {"origin", "HEAD"}:
      continue
    if name not in seen:
      seen.add(name)
      branches.append(name)
    if len(branches) >= 50:
      break
  return branches


@router.get("/repos")
async def list_repos(cfg: CharlieBotConfig = Depends(get_config)):
  """Scan workspace_dirs (one level deep) and return repos containing a .git folder."""
  seen: set[str] = set()
  repos: list[dict[str, str]] = []
  for dir_str in cfg.workspace_dirs:
    parent = Path(dir_str).expanduser()
    if not parent.is_dir():
      continue
    for child in parent.iterdir():
      if not child.is_dir() or not (child / ".git").exists():
        continue
      resolved = str(child.resolve())
      if resolved in seen:
        continue
      seen.add(resolved)
      repos.append({"label": child.name, "path": resolved})
  repos.sort(key=lambda r: r["label"])
  return repos


@router.get("/diff/files")
async def diff_files(
    repo: str = Query(..., description="Full path to git repo"),
    base: str = Query(..., description="Base ref"),
    head: str = Query(..., description="Head ref"),
    mode: Literal["three-dot", "two-dot"] = Query("three-dot", description="Diff range mode"),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Return a cheap per-file manifest (status + line counts) for the diff range.

  Carries no hunk content, so it never hits any size limit even for thousands of files.
  """
  repo_path = _resolve_repo_under_workspace(repo, cfg)
  range_spec = _range_spec(base, head, mode)
  base_sha, head_sha = await _resolve_commits(repo_path, [base, head])
  key = (str(repo_path), base_sha, head_sha, mode, _attributes_signature(repo_path))
  memoized = _diff_files_memo.get(key)
  if memoized is None:
    # Two independent manifest subprocesses over the same frozen range; they run
    # concurrently and a failure surfaces in the same order the sequential form
    # raised it (name-status, then numstat).
    name_status, numstat = await asyncio.gather(
        _run_git_diff(repo_path, ["--name-status", "-z", range_spec]),
        _run_git_diff(repo_path, ["--numstat", "-z", range_spec]),
        return_exceptions=True,
    )
    for outcome in (name_status, numstat):
      if isinstance(outcome, BaseException):
        raise outcome
    status_by_path = _parse_name_status_z(name_status)
    rows: list[dict] = []
    total_additions = 0
    total_deletions = 0
    for additions, deletions, path, old_path in _parse_numstat_z(numstat):
      entry: dict = {
          "path": path,
          "status": status_by_path[path],
          "additions": additions,
          "deletions": deletions,
      }
      if old_path is not None:
        entry["old_path"] = old_path
      rows.append(entry)
      total_additions += additions
      total_deletions += deletions
    memoized = (rows, total_additions, total_deletions)
    _diff_files_memo.store(key, memoized)
  rows, total_additions, total_deletions = memoized

  return {
      "files": rows,
      "base": base,
      "head": head,
      "head_sha": head_sha,
      "mode": mode,
      "total_files": len(rows),
      "total_additions": total_additions,
      "total_deletions": total_deletions,
  }


@router.get("/diff/file")
async def diff_file(
    repo: str = Query(..., description="Full path to git repo"),
    base: str = Query(..., description="Base ref"),
    head: str = Query(..., description="Head ref"),
    mode: Literal["three-dot", "two-dot"] = Query("three-dot", description="Diff range mode"),
    path: str = Query(..., description="Repo-relative path of the file to diff"),
    old_path: str | None = Query(
        None, description="Pre-rename path; pass alongside path so a rename/copy renders as a rename, not a re-add"),
    force: bool = Query(False, description="Render even if the diff exceeds the per-file cap"),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Return the unified diff for a single file.

  When the file's diff exceeds the per-file cap and force is false, return a
  {too_large, size_bytes, path} stub (HTTP 200) instead of the content so one giant
  file can't wedge the page; the caller re-requests with force=true to load it anyway.
  """
  repo_path = _resolve_repo_under_workspace(repo, cfg)
  # Restricting to a single side of a rename makes git drop the pairing and emit a
  # wholesale add/delete; passing both endpoints keeps it a rename diff.
  pathspec = tuple([old_path, path] if old_path else [path])
  base_sha, head_sha = await _resolve_commits(repo_path, [base, head])
  key = (str(repo_path), base_sha, head_sha, mode, _attributes_signature(repo_path), pathspec)
  diff_text = _diff_file_memo.get(key)
  if diff_text is None:
    # The range over resolved SHAs, so a ref that moves before the subprocess
    # starts cannot key one pair's body under another pair.
    diff_text = await _run_git_diff(repo_path, [_range_spec(base_sha, head_sha, mode), "--", *pathspec])
    _diff_file_memo.store(key, diff_text)
  size_bytes = len(diff_text.encode("utf-8"))
  if not force and size_bytes > _DIFF_MAX_BYTES:
    return {"too_large": True, "size_bytes": size_bytes, "path": path}
  return {"diff": diff_text, "path": path, "size_bytes": size_bytes}
