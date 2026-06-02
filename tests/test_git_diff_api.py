"""Tests for the file-level lazy-load diff API (src/api/git.py)."""

import subprocess
from pathlib import Path

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api import git as git_api
from src.core.config import CharlieBotConfig, get_config


def _git(repo: Path, *args: str) -> None:
  subprocess.run(["git", *args], cwd=repo, check=True, capture_output=True, text=True)


def _build_repo(workspace: Path) -> Path:
  """Create a repo with a `main` and `feature` branch covering add/modify/delete/rename."""
  repo = workspace / "repo"
  repo.mkdir(parents=True)
  _git(repo, "init", "-q", "-b", "main")
  _git(repo, "config", "user.email", "t@t.t")
  _git(repo, "config", "user.name", "t")
  (repo / "keep.txt").write_text("line1\nline2\nline3\n")
  (repo / "torename.txt").write_text("old content\nsecond\n")
  (repo / "todelete.txt").write_text("to be deleted\n")
  _git(repo, "add", "-A")
  _git(repo, "commit", "-qm", "base")

  _git(repo, "checkout", "-q", "-b", "feature")
  (repo / "keep.txt").write_text("line1\nline2 changed\nline3\nline4\n")
  _git(repo, "mv", "torename.txt", "renamed.txt")
  (repo / "renamed.txt").write_text("old content\nsecond\nthird\n")
  _git(repo, "rm", "-q", "todelete.txt")
  (repo / "added.txt").write_text("brand new\nfile\n")
  _git(repo, "add", "-A")
  _git(repo, "commit", "-qm", "feature")
  return repo


def _build_client(workspace: Path) -> TestClient:
  cfg = CharlieBotConfig(
      charliebot_home=workspace / "charliebot-home",
      workspace_dirs=[str(workspace)],
  )
  app = FastAPI()
  app.include_router(git_api.router, prefix="/api/git")
  app.dependency_overrides[get_config] = lambda: cfg
  return TestClient(app)


def test_diff_files_manifest(tmp_path: Path) -> None:
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)

  resp = client.get(
      "/api/git/diff/files",
      params={"repo": str(repo), "base": "main", "head": "feature", "mode": "three-dot"},
  )
  assert resp.status_code == 200
  data = resp.json()
  by_path = {f["path"]: f for f in data["files"]}

  assert by_path["added.txt"]["status"] == "A"
  assert (by_path["added.txt"]["additions"], by_path["added.txt"]["deletions"]) == (2, 0)
  assert by_path["keep.txt"]["status"] == "M"
  assert (by_path["keep.txt"]["additions"], by_path["keep.txt"]["deletions"]) == (2, 1)
  assert by_path["todelete.txt"]["status"] == "D"
  # Rename is keyed by the new path with an 'R' status and carries its pre-rename path.
  assert by_path["renamed.txt"]["status"] == "R"
  assert by_path["renamed.txt"]["old_path"] == "torename.txt"
  assert "torename.txt" not in by_path
  # Non-renames carry no old_path key.
  assert "old_path" not in by_path["keep.txt"]

  assert data["total_files"] == len(data["files"]) == 4
  assert data["total_additions"] == sum(f["additions"] for f in data["files"])
  assert data["total_deletions"] == sum(f["deletions"] for f in data["files"])


def test_diff_file_returns_unified_diff(tmp_path: Path) -> None:
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)

  resp = client.get(
      "/api/git/diff/file",
      params={"repo": str(repo), "base": "main", "head": "feature", "path": "keep.txt"},
  )
  assert resp.status_code == 200
  data = resp.json()
  assert data["path"] == "keep.txt"
  assert "line2 changed" in data["diff"]
  assert data["size_bytes"] == len(data["diff"].encode("utf-8"))


def test_diff_file_rename_renders_as_rename(tmp_path: Path) -> None:
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)

  # Passing old_path alongside path keeps git's rename pairing intact: the diff shows the
  # rename and only the one added line, not the whole file re-added.
  params = {"repo": str(repo), "base": "main", "head": "feature", "path": "renamed.txt", "old_path": "torename.txt"}
  diff = client.get("/api/git/diff/file", params=params).json()["diff"]
  assert "rename from torename.txt" in diff
  assert "rename to renamed.txt" in diff
  assert "new file" not in diff

  # Without old_path git drops the pairing and the same file looks like a wholesale add.
  readd = client.get(
      "/api/git/diff/file",
      params={"repo": str(repo), "base": "main", "head": "feature", "path": "renamed.txt"},
  ).json()["diff"]
  assert "new file" in readd


def test_diff_file_too_large_returns_stub_and_force_loads(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)
  # Shrink the per-file cap so keep.txt's small diff trips it.
  monkeypatch.setattr(git_api, "_DIFF_MAX_BYTES", 10)

  params = {"repo": str(repo), "base": "main", "head": "feature", "path": "keep.txt"}
  stub = client.get("/api/git/diff/file", params=params).json()
  assert stub["too_large"] is True
  assert stub["path"] == "keep.txt"
  assert stub["size_bytes"] > 10
  assert "diff" not in stub

  forced = client.get("/api/git/diff/file", params={**params, "force": "true"}).json()
  assert "too_large" not in forced
  assert "line2 changed" in forced["diff"]


def test_empty_diff_has_no_files(tmp_path: Path) -> None:
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)

  resp = client.get(
      "/api/git/diff/files",
      params={"repo": str(repo), "base": "main", "head": "main"},
  )
  assert resp.status_code == 200
  data = resp.json()
  assert data["files"] == []
  assert data["total_files"] == 0
  assert data["total_additions"] == 0
  assert data["total_deletions"] == 0


def test_two_dot_mode(tmp_path: Path) -> None:
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)

  resp = client.get(
      "/api/git/diff/files",
      params={"repo": str(repo), "base": "main", "head": "feature", "mode": "two-dot"},
  )
  assert resp.status_code == 200
  data = resp.json()
  assert data["mode"] == "two-dot"
  assert {f["path"] for f in data["files"]} == {"added.txt", "keep.txt", "renamed.txt", "todelete.txt"}


def test_repo_outside_workspace_rejected(tmp_path: Path) -> None:
  repo = _build_repo(tmp_path)
  # Point the workspace somewhere else so the repo fails the under-workspace check.
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      workspace_dirs=[str(tmp_path / "elsewhere")],
  )
  app = FastAPI()
  app.include_router(git_api.router, prefix="/api/git")
  app.dependency_overrides[get_config] = lambda: cfg
  client = TestClient(app)

  resp = client.get(
      "/api/git/diff/files",
      params={"repo": str(repo), "base": "main", "head": "feature"},
  )
  assert resp.status_code == 400
