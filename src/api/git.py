"""Git-related API routes."""

import asyncio
import subprocess
from pathlib import Path
from typing import Literal

from fastapi import APIRouter, Depends, HTTPException, Query

from src.core.config import CharlieBotConfig, get_config
from src.core.timeouts import SUBPROCESS_GIT_DIFF_TIMEOUT, SUBPROCESS_GIT_READ_TIMEOUT

router = APIRouter()

# Per-file diff cap (bytes). A single file whose diff exceeds this is returned as
# a content-free stub so one giant generated/lockfile/binary file can't wedge the page.
_DIFF_MAX_BYTES = 5 * 1024 * 1024


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


def _run_git_diff_sync(repo_path: Path, args: list[str]) -> str:
  """Run `git diff <args>` in repo_path and return stdout; raise HTTPException(500) on failure."""
  result = subprocess.run(
      ["git", "diff", *args],
      cwd=repo_path,
      capture_output=True,
      text=True,
      check=False,
      timeout=SUBPROCESS_GIT_DIFF_TIMEOUT,
  )
  if result.returncode != 0:
    raise HTTPException(status_code=500, detail=result.stderr.strip() or "git diff failed")
  return result.stdout


async def _run_git_diff(repo_path: Path, args: list[str]) -> str:
  """Async front for the blocking diff; a subprocess that may hold for
  SUBPROCESS_GIT_DIFF_TIMEOUT seconds stays off the event loop."""
  return await asyncio.to_thread(_run_git_diff_sync, repo_path, args)


def _resolve_commit_sync(repo_path: Path, ref: str) -> str:
  """Resolve a git ref to its full commit SHA."""
  result = subprocess.run(
      ["git", "rev-parse", "--verify", f"{ref}^{{commit}}"],
      cwd=repo_path,
      capture_output=True,
      text=True,
      check=False,
      timeout=SUBPROCESS_GIT_READ_TIMEOUT,
  )
  if result.returncode != 0:
    raise HTTPException(status_code=500, detail=result.stderr.strip() or "git rev-parse failed")
  return result.stdout.strip()


async def _resolve_commit(repo_path: Path, ref: str) -> str:
  """Async front for the blocking rev-parse; keeps the lookup off the event loop."""
  return await asyncio.to_thread(_resolve_commit_sync, repo_path, ref)


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
  result = subprocess.run(
      ["git", "branch", "-a", "--sort=-committerdate", "--format=%(refname:short)"],
      cwd=repo_path,
      capture_output=True,
      text=True,
      check=False,
      timeout=SUBPROCESS_GIT_READ_TIMEOUT,
  )
  if result.returncode != 0:
    raise HTTPException(status_code=500, detail=result.stderr.strip())
  return result.stdout


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
  head_sha = await _resolve_commit(repo_path, head)
  status_by_path = _parse_name_status_z(await _run_git_diff(repo_path, ["--name-status", "-z", range_spec]))

  files: list[dict] = []
  total_additions = 0
  total_deletions = 0
  numstat = await _run_git_diff(repo_path, ["--numstat", "-z", range_spec])
  for additions, deletions, path, old_path in _parse_numstat_z(numstat):
    entry: dict = {
        "path": path,
        "status": status_by_path[path],
        "additions": additions,
        "deletions": deletions,
    }
    if old_path is not None:
      entry["old_path"] = old_path
    files.append(entry)
    total_additions += additions
    total_deletions += deletions

  return {
      "files": files,
      "base": base,
      "head": head,
      "head_sha": head_sha,
      "mode": mode,
      "total_files": len(files),
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
  range_spec = _range_spec(base, head, mode)
  # Restricting to a single side of a rename makes git drop the pairing and emit a
  # wholesale add/delete; passing both endpoints keeps it a rename diff.
  pathspec = [old_path, path] if old_path else [path]
  diff_text = await _run_git_diff(repo_path, [range_spec, "--", *pathspec])
  size_bytes = len(diff_text.encode("utf-8"))
  if not force and size_bytes > _DIFF_MAX_BYTES:
    return {"too_large": True, "size_bytes": size_bytes, "path": path}
  return {"diff": diff_text, "path": path, "size_bytes": size_bytes}
