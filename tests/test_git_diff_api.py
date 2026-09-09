"""Tests for the file-level lazy-load diff API (src/api/git.py)."""

import asyncio
import subprocess
from pathlib import Path

import httpx
import pytest
from conftest import loop_stall_gaps, stall_before_call
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


def _build_app(workspace: Path) -> FastAPI:
  cfg = CharlieBotConfig(
      charliebot_home=workspace / "charliebot-home",
      workspace_dirs=[str(workspace)],
  )
  app = FastAPI()
  app.include_router(git_api.router, prefix="/api/git")
  app.dependency_overrides[get_config] = lambda: cfg
  return app


def _build_client(workspace: Path) -> TestClient:
  return TestClient(_build_app(workspace))


def _get_diff(client: TestClient, endpoint: str, repo: Path, base: str, head: str, **params: str) -> httpx.Response:
  return client.get(f"/api/git/diff/{endpoint}", params={"repo": str(repo), "base": base, "head": head, **params})


def _counting_run(monkeypatch: pytest.MonkeyPatch) -> list[list[str]]:
  """Patch the git API's subprocess.run to record each call's argv and delegate to the real run.

  Returns the list the argvs land in; a test clears it between rounds to count
  only the calls after that point.
  """
  calls: list[list[str]] = []
  real_run = subprocess.run

  def counting_run(*args, **kwargs):  # type: ignore[no-untyped-def]
    calls.append(args[0])
    return real_run(*args, **kwargs)

  monkeypatch.setattr(git_api.subprocess, "run", counting_run)
  return calls


def test_diff_files_manifest(tmp_path: Path) -> None:
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


def test_diff_file_returns_unified_diff(tmp_path: Path) -> None:
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)

  resp = _get_diff(client, "file", repo, "main", "feature", path="keep.txt")
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
  readd = _get_diff(client, "file", repo, "main", "feature", path="renamed.txt").json()["diff"]
  assert "new file" in readd


def test_diff_file_too_large_returns_stub_and_force_loads(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
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


def test_empty_diff_has_no_files(tmp_path: Path) -> None:
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)

  resp = _get_diff(client, "files", repo, "main", "main")
  assert resp.status_code == 200
  data = resp.json()
  assert data["files"] == []
  assert data["total_files"] == 0
  assert data["total_additions"] == 0
  assert data["total_deletions"] == 0


def test_two_dot_mode(tmp_path: Path) -> None:
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)

  resp = _get_diff(client, "files", repo, "main", "feature", mode="two-dot")
  assert resp.status_code == 200
  data = resp.json()
  assert data["mode"] == "two-dot"
  assert {f["path"] for f in data["files"]} == {"added.txt", "keep.txt", "renamed.txt", "todelete.txt"}


def test_diff_files_keeps_event_loop_responsive(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A slow git subprocess runs off the event loop; concurrent loop work keeps ticking."""
  repo = _build_repo(tmp_path)
  app = _build_app(tmp_path)

  monkeypatch.setattr(git_api.subprocess, "run", stall_before_call(0.25, subprocess.run))

  async def scenario() -> tuple[httpx.Response, list[float]]:
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://test") as client:
      async with loop_stall_gaps() as gaps:
        resp = await client.get(
            "/api/git/diff/files",
            params={
                "repo": str(repo),
                "base": "main",
                "head": "feature",
                "mode": "three-dot"
            },
        )
    return resp, gaps

  resp, gaps = asyncio.run(scenario())
  assert resp.status_code == 200
  assert resp.json()["total_files"] == 4
  # diff_files fires three subprocess.run calls (rev-parse + two diffs); an inline run
  # pins one gap per call near the 0.25 s stall.
  assert max(gaps) < 0.15


# First view's subprocess sequence: one rev-parse resolving both refs, then the two
# manifest diffs (files endpoint) or the single per-file diff (file endpoint).
_REPEAT_VIEW_CASES = [
    pytest.param("files", {"mode": "three-dot"}, ["rev-parse", "diff", "diff"], id="files"),
    pytest.param("file", {"path": "keep.txt"}, ["rev-parse", "diff"], id="file"),
]


@pytest.mark.parametrize(("endpoint", "extra_params", "first_calls"), _REPEAT_VIEW_CASES)
def test_diff_repeat_view_uses_memo(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    endpoint: str,
    extra_params: dict[str, str],
    first_calls: list[str],
) -> None:
  """A repeat view of the same resolved range re-runs zero git diff subprocesses."""
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)
  calls = _counting_run(monkeypatch)

  first = _get_diff(client, endpoint, repo, "main", "feature", **extra_params)
  assert first.status_code == 200
  assert [c[1] for c in calls] == first_calls

  calls.clear()
  second = _get_diff(client, endpoint, repo, "main", "feature", **extra_params)
  assert second.status_code == 200
  assert second.json() == first.json()
  # The repeat view pays zero subprocesses: the ref-state signature is unchanged,
  # so the memoized resolution serves the SHAs and the manifest/body memo serves
  # the rest.
  assert [c[1] for c in calls] == []


_HEAD_MOVE_CASES = [
    pytest.param("files", {"mode": "three-dot"}, id="files"),
    pytest.param("file", {"path": "added.txt"}, id="file"),
]


@pytest.mark.parametrize(("endpoint", "extra_params"), _HEAD_MOVE_CASES)
def test_diff_head_move_busts_memo(tmp_path: Path, endpoint: str, extra_params: dict[str, str]) -> None:
  """A moved ref resolves to a new SHA key, so its view re-computes."""
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)
  first = _get_diff(client, endpoint, repo, "main", "feature", **extra_params).json()

  (repo / "added.txt").write_text("brand new\nfile\nthird\n")
  _git(repo, "add", "-A")
  _git(repo, "commit", "-qm", "advance feature")

  second = _get_diff(client, endpoint, repo, "main", "feature", **extra_params).json()
  # The files response carries the moved SHA; the file response carries only the
  # body, so its re-computation shows as the new hunk line.
  if endpoint == "files":
    assert second["head_sha"] != first["head_sha"]
    by_path = {f["path"]: f for f in second["files"]}
    assert by_path["added.txt"]["additions"] == 3
  else:
    assert "+third" in second["diff"]


def test_branch_repeat_view_uses_memo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A repeat branch listing re-runs zero git subprocesses and serves the same list."""
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)
  calls = _counting_run(monkeypatch)

  first = client.get("/api/git/branches", params={"repo": str(repo)})
  assert first.status_code == 200
  assert "feature" in first.json() and "main" in first.json()

  calls.clear()
  second = client.get("/api/git/branches", params={"repo": str(repo)})
  assert second.status_code == 200
  assert second.json() == first.json()
  # The repeat listing pays zero subprocesses: the ref-state signature is
  # unchanged, so the memoized branch lines serve the response.
  assert [c[1] for c in calls] == []


def test_branch_new_ref_busts_memo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A ref the listing reads (a new branch) moves the signature, so the list re-computes."""
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)
  calls = _counting_run(monkeypatch)

  first = client.get("/api/git/branches", params={"repo": str(repo)}).json()
  assert calls, "the first listing runs the branch subprocess"

  calls.clear()
  _git(repo, "branch", "newly-created")
  second = client.get("/api/git/branches", params={"repo": str(repo)}).json()
  assert "newly-created" in second and "newly-created" not in first
  assert len([c for c in calls if c[1] == "branch" and "-a" in c]) == 1, \
      "the moved ref state re-runs the branch subprocess exactly once"

  calls.clear()
  third = client.get("/api/git/branches", params={"repo": str(repo)}).json()
  assert third == second
  assert [c[1] for c in calls] == []


def test_branch_list_accepts_git_dir_path_form(tmp_path: Path) -> None:
  """The endpoint's own validation admits a path that IS a .git dir; the signature
  walk must not 500 on it where the subprocess-only form returned 200."""
  repo = _build_repo(tmp_path)
  client = _build_client(tmp_path)
  root = client.get("/api/git/branches", params={"repo": str(repo)}).json()
  via_git_dir = client.get("/api/git/branches", params={"repo": str(repo / ".git")})
  assert via_git_dir.status_code == 200
  assert via_git_dir.json() == root


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

  resp = _get_diff(client, "files", repo, "main", "feature")
  assert resp.status_code == 400


def test_refs_signature_tracks_ref_state(tmp_path: Path) -> None:
  """The signature moves exactly when ref state moves, and holds still otherwise."""
  repo = _build_repo(tmp_path)
  before = git_api._refs_signature(repo)

  # Idle repo: unchanged.
  assert git_api._refs_signature(repo) == before

  # A commit rewrites the branch ref.
  _git(repo, "commit", "-q", "--allow-empty", "-m", "advance")
  after_commit = git_api._refs_signature(repo)
  assert after_commit != before

  # A checkout rewrites HEAD.
  _git(repo, "checkout", "-q", "main")
  assert git_api._refs_signature(repo) != after_commit

  # Packing rewrites packed-refs and removes the loose ref files, then holds still.
  before_pack = git_api._refs_signature(repo)
  _git(repo, "pack-refs", "--all")
  after_pack = git_api._refs_signature(repo)
  assert after_pack != before_pack
  assert git_api._refs_signature(repo) == after_pack


def test_refs_signature_entries_bounded_by_dirs(tmp_path: Path) -> None:
  """Branch accumulation adds signature entries only for new namespace dirs.

  The refs trees contribute their directories (each ref mutation renames into
  the containing directory and moves its mtime_ns), so fifty new branches move
  the signature without growing its entry count.
  """
  repo = _build_repo(tmp_path)
  base_len = len(git_api._refs_signature(repo))
  for i in range(50):
    _git(repo, "branch", f"bulk{i}")
  grown = git_api._refs_signature(repo)
  assert len(grown) - base_len <= 2


def test_refs_signature_moves_on_unrelated_branch_growth(tmp_path: Path) -> None:
  """A new branch renames into refs/heads, moving the directory's mtime_ns."""
  repo = _build_repo(tmp_path)
  before = git_api._refs_signature(repo)
  _git(repo, "branch", "unrelated")
  assert git_api._refs_signature(repo) != before


def test_ref_resolution_memo_skips_rev_parse_until_refs_move(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The memoized resolver re-runs rev-parse only when the ref state has moved."""
  repo = _build_repo(tmp_path)
  calls = _counting_run(monkeypatch)

  first = git_api._resolve_commits_memoized_sync(repo, ["main", "feature"])
  second = git_api._resolve_commits_memoized_sync(repo, ["main", "feature"])
  assert first == second
  assert [c[1] for c in calls if c[1] == "rev-parse"] == ["rev-parse"]

  calls.clear()
  _git(repo, "commit", "-q", "--allow-empty", "-m", "move feature")
  third = git_api._resolve_commits_memoized_sync(repo, ["main", "feature"])
  assert third != first
  assert [c[1] for c in calls if c[1] == "rev-parse"] == ["rev-parse"]


def test_ref_resolution_follows_linked_worktree(tmp_path: Path) -> None:
  """A linked worktree's git dir and common dir are found through the .git file."""
  repo = _build_repo(tmp_path)
  worktree = tmp_path / "wt"
  _git(repo, "worktree", "add", "-q", str(worktree), "-b", "wt-branch")

  git_dir, common_dir = git_api._git_dirs(worktree)
  assert git_dir != repo / ".git"
  assert (common_dir / "refs" / "heads" / "main").exists()
  # The worktree's HEAD is its own file inside the per-worktree git dir.
  assert (git_dir / "HEAD").exists()

  resolved = git_api._resolve_commits_memoized_sync(worktree, ["wt-branch", "main"])
  assert len(resolved) == 2 and all(len(sha) == 40 for sha in resolved)

  # Per-worktree refs (bisect, worktree, rewritten) live under the linked
  # worktree's own git dir; moving one must move the signature.
  head_sha = resolved[1]
  _git(worktree, "update-ref", "refs/bisect/bad", head_sha)
  with_bisect = git_api._refs_signature(worktree)
  assert git_api._refs_signature(worktree) == with_bisect
  _git(worktree, "update-ref", "refs/bisect/bad", resolved[0])
  assert git_api._refs_signature(worktree) != with_bisect
  moved = git_api._resolve_commits_memoized_sync(worktree, ["refs/bisect/bad"])
  assert moved == [resolved[0]]
