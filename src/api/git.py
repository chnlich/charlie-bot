"""Git-related API routes."""

import subprocess
from pathlib import Path

from fastapi import APIRouter, HTTPException, Query

router = APIRouter()


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
    timeout=10,
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
