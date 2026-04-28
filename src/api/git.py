"""Git-related API routes."""

import subprocess
from pathlib import Path
from typing import Literal

from fastapi import APIRouter, Depends, HTTPException, Query

from src.core.config import CharlieBotConfig, get_config
from src.core.timeouts import SUBPROCESS_GIT_DIFF_TIMEOUT, SUBPROCESS_GIT_READ_TIMEOUT

router = APIRouter()

# Maximum diff payload size (bytes). Larger diffs are rejected with HTTP 413.
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


@router.get("/branches")
async def list_branches(repo: str = Query(..., description="Full path to git repo")):
  """Return branch names for a repo, most recent first, up to 50."""
  repo_path = Path(repo).expanduser()
  if not (repo_path / ".git").exists() and not repo_path.name == ".git":
    raise HTTPException(status_code=400, detail=f"Not a git repo: {repo}")
  result = subprocess.run(
      ["git", "branch", "-a", "--sort=-committerdate", "--format=%(refname:short)"],
      cwd=repo_path,
      capture_output=True,
      text=True,
      timeout=SUBPROCESS_GIT_READ_TIMEOUT,
  )
  if result.returncode != 0:
    raise HTTPException(status_code=500, detail=result.stderr.strip())
  seen: set[str] = set()
  branches: list[str] = []
  for line in result.stdout.splitlines():
    name = line.strip()
    if not name or name == "origin":
      continue
    # Strip origin/ prefix from remote branches
    if name.startswith("origin/"):
      name = name[len("origin/"):]
    if name == "HEAD":
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


@router.get("/diff")
async def diff(
    repo: str = Query(..., description="Full path to git repo"),
    base: str = Query(..., description="Base ref"),
    head: str = Query(..., description="Head ref"),
    mode: Literal["three-dot", "two-dot"] = Query("three-dot", description="Diff range mode"),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Run `git diff base...head` (or `base..head`) and return the unified diff."""
  repo_path = _resolve_repo_under_workspace(repo, cfg)
  range_spec = f"{base}...{head}" if mode == "three-dot" else f"{base}..{head}"
  result = subprocess.run(
      ["git", "diff", range_spec],
      cwd=repo_path,
      capture_output=True,
      text=True,
      check=False,
      timeout=SUBPROCESS_GIT_DIFF_TIMEOUT,
  )
  if result.returncode != 0:
    raise HTTPException(status_code=500, detail=result.stderr.strip() or "git diff failed")

  diff_text = result.stdout
  size_bytes = len(diff_text.encode("utf-8"))
  if size_bytes > _DIFF_MAX_BYTES:
    raise HTTPException(
        status_code=413,
        detail={
            "message": "diff exceeds size limit",
            "size_bytes": size_bytes,
            "limit_bytes": _DIFF_MAX_BYTES,
        },
    )

  return {
      "diff": diff_text,
      "base": base,
      "head": head,
      "mode": mode,
      "size_bytes": size_bytes,
  }
