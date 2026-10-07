"""Tests for the file-level lazy-load diff API (src/features/diff_view/api.py)."""

import pathlib
import subprocess

import conftest
import fastapi
import httpx
import pytest
from fastapi import testclient

from src.features.diff_view import api as git_api
from src.infra import config


def _build_repo(workspace: pathlib.Path) -> pathlib.Path:
  """Create a repo with a `main` and `feature` branch covering add/modify/delete/rename."""
  repo = workspace / "repo"
  repo.mkdir(parents=True)
  conftest.run_git(repo, "init", "-q", "-b", "main")
  conftest.run_git(repo, "config", "user.email", "t@t.t")
  conftest.run_git(repo, "config", "user.name", "t")
  (repo / "keep.txt").write_text("line1\nline2\nline3\n")
  (repo / "torename.txt").write_text("old content\nsecond\n")
  (repo / "todelete.txt").write_text("to be deleted\n")
  conftest.run_git(repo, "add", "-A")
  conftest.run_git(repo, "commit", "-qm", "base")

  conftest.run_git(repo, "checkout", "-q", "-b", "feature")
  (repo / "keep.txt").write_text("line1\nline2 changed\nline3\nline4\n")
  conftest.run_git(repo, "mv", "torename.txt", "renamed.txt")
  (repo / "renamed.txt").write_text("old content\nsecond\nthird\n")
  conftest.run_git(repo, "rm", "-q", "todelete.txt")
  (repo / "added.txt").write_text("brand new\nfile\n")
  conftest.run_git(repo, "add", "-A")
  conftest.run_git(repo, "commit", "-qm", "feature")
  return repo


def _build_app(workspace: pathlib.Path) -> fastapi.FastAPI:
  cfg = config.CharlieBotConfig(
      charliebot_home=workspace / "charliebot-home",
      paths={"workspace_dirs": [str(workspace)]},
  )
  app = fastapi.FastAPI()
  app.include_router(git_api.router, prefix="/api/git")
  conftest.apply_config_overrides(app, cfg)
  return app


def _build_client(workspace: pathlib.Path) -> testclient.TestClient:
  return testclient.TestClient(_build_app(workspace))


def _get_diff(
    client: testclient.TestClient, endpoint: str, repo: pathlib.Path, base: str, head: str,
    **params: str) -> httpx.Response:
  return client.get(f"/api/git/diff/{endpoint}", params={"repo": str(repo), "base": base, "head": head, **params})


def test_diff_files_manifest(tmp_path: pathlib.Path) -> None:
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)

  resp = _get_diff(client, "files", repo, "main", "feature", mode="three-dot")
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
  expected_head_sha = subprocess.run(
      ["git", "rev-parse", "feature"], cwd=repo, check=True, capture_output=True, text=True).stdout.strip()
  assert data["head_sha"] == expected_head_sha


def test_diff_file_returns_unified_diff(tmp_path: pathlib.Path) -> None:
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)

  resp = _get_diff(client, "file", repo, "main", "feature", path="keep.txt")
  assert resp.status_code == 200
  data = resp.json()
  assert data["path"] == "keep.txt"
  assert "line2 changed" in data["diff"]
  assert data["size_bytes"] == len(data["diff"].encode("utf-8"))


def test_diff_file_rename_renders_as_rename(tmp_path: pathlib.Path) -> None:
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)

  # Passing old_path alongside path keeps git's rename pairing intact: the diff shows the
  # rename and only the one added line, not the whole file re-added.
  diff = _get_diff(client, "file", repo, "main", "feature", path="renamed.txt", old_path="torename.txt").json()["diff"]
  assert "rename from torename.txt" in diff
  assert "rename to renamed.txt" in diff
  assert "new file" not in diff

  # Without old_path git drops the pairing and the same file looks like a wholesale add.
  readd = _get_diff(client, "file", repo, "main", "feature", path="renamed.txt").json()["diff"]
  assert "new file" in readd


def test_diff_file_too_large_returns_stub_and_force_loads(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)
  # Shrink the per-file cap so keep.txt's small diff trips it.
  monkeypatch.setattr(git_api, "_DIFF_MAX_BYTES", 10)

  stub = _get_diff(client, "file", repo, "main", "feature", path="keep.txt").json()
  assert stub["too_large"] is True
  assert stub["path"] == "keep.txt"
  assert stub["size_bytes"] > 10
  assert "diff" not in stub

  forced = _get_diff(client, "file", repo, "main", "feature", path="keep.txt", force="true").json()
  assert "too_large" not in forced
  assert "line2 changed" in forced["diff"]


_HEAD_MOVE_CASES = [
    pytest.param("files", {"mode": "three-dot"}, id="files"),
    pytest.param("file", {"path": "added.txt"}, id="file"),
]


@pytest.mark.parametrize(("endpoint", "extra_params"), _HEAD_MOVE_CASES)
def test_diff_head_move_busts_memo(tmp_path: pathlib.Path, endpoint: str, extra_params: dict[str, str]) -> None:
  """A moved ref resolves to a new SHA key, so its view re-computes."""
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)
  first = _get_diff(client, endpoint, repo, "main", "feature", **extra_params).json()

  (repo / "added.txt").write_text("brand new\nfile\nthird\n")
  conftest.run_git(repo, "add", "-A")
  conftest.run_git(repo, "commit", "-qm", "advance feature")

  second = _get_diff(client, endpoint, repo, "main", "feature", **extra_params).json()
  # The files response carries the moved SHA; the file response carries only the
  # body, so its re-computation shows as the new hunk line.
  if endpoint == "files":
    assert second["head_sha"] != first["head_sha"]
    by_path = {f["path"]: f for f in second["files"]}
    assert by_path["added.txt"]["additions"] == 3
  else:
    assert "+third" in second["diff"]


def test_memory_store_repo_accepted_outside_workspace_dirs(tmp_path: pathlib.Path) -> None:
  """The memory store (cfg.memory_dir) is a diff-able repo even though it sits
  outside paths.workspace_dirs: the PR flow serves its proposal diff here."""
  workspace = tmp_path / "workspace"
  workspace.mkdir()
  cfg = config.CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      paths={"workspace_dirs": [str(workspace)]},
  )
  repo = _build_repo(cfg.memory_dir)
  app = fastapi.FastAPI()
  app.include_router(git_api.router, prefix="/api/git")
  conftest.apply_config_overrides(app, cfg)
  client = testclient.TestClient(app)

  resp = _get_diff(client, "files", repo, "main", "feature")
  assert resp.status_code == 200
  assert {f["path"] for f in resp.json()["files"]} >= {"added.txt", "renamed.txt"}


def test_other_repo_outside_workspace_and_memory_rejected(tmp_path: pathlib.Path) -> None:
  """A repo outside both the workspace roots and the memory store stays refused."""
  repo = _build_repo(tmp_path / "stray-repo")
  cfg = config.CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      paths={"workspace_dirs": [str(tmp_path / "elsewhere")]},
  )
  app = fastapi.FastAPI()
  app.include_router(git_api.router, prefix="/api/git")
  conftest.apply_config_overrides(app, cfg)
  client = testclient.TestClient(app)

  resp = _get_diff(client, "files", repo, "main", "feature")
  assert resp.status_code == 400


def test_refs_signature_tracks_ref_state(tmp_path: pathlib.Path) -> None:
  """The signature moves exactly when ref state moves, and holds still otherwise."""
  repo = _build_repo(tmp_path)
  before = git_api._refs_signature(repo)

  # Idle repo: unchanged.
  assert git_api._refs_signature(repo) == before

  # A commit rewrites the branch ref.
  conftest.run_git(repo, "commit", "-q", "--allow-empty", "-m", "advance")
  after_commit = git_api._refs_signature(repo)
  assert after_commit != before

  # A checkout rewrites HEAD.
  conftest.run_git(repo, "checkout", "-q", "main")
  assert git_api._refs_signature(repo) != after_commit

  # Packing rewrites packed-refs and removes the loose ref files, then holds still.
  before_pack = git_api._refs_signature(repo)
  conftest.run_git(repo, "pack-refs", "--all")
  after_pack = git_api._refs_signature(repo)
  assert after_pack != before_pack
  assert git_api._refs_signature(repo) == after_pack
