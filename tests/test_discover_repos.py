"""Tests for CharlieBotConfig.discover_repos, the single workspace_dirs repo scan.

Both repo-listing endpoints (/api/sessions/projects, /api/git/repos) serve this
one scan; the contract pinned here is resolved absolute paths, deduplication by
path, and a global name sort.
"""
from pathlib import Path

from conftest import OPUS_BACKEND_OPTION

from src.core.config import CharlieBotConfig


def _cfg(tmp_path: Path, workspace_dirs: list[str]) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      paths={"workspace_dirs": workspace_dirs},
      backends={"options": [OPUS_BACKEND_OPTION]},
  )


def _make_repo(workspace: Path, name: str) -> Path:
  repo = workspace / name
  (repo / ".git").mkdir(parents=True)
  return repo


def test_scan_returns_resolved_deduped_sorted_entries(tmp_path: Path) -> None:
  workspace = tmp_path / "workspace"
  workspace.mkdir()
  beta = _make_repo(workspace, "beta")
  alpha = _make_repo(workspace, "alpha")
  (workspace / "not-a-repo").mkdir()
  (workspace / "notes.txt").write_text("x", encoding="utf-8")

  repos = _cfg(tmp_path, [str(workspace), str(workspace)]).discover_repos()

  assert repos == [
      {
          "name": "alpha",
          "path": str(alpha.resolve())
      },
      {
          "name": "beta",
          "path": str(beta.resolve())
      },
  ]


def test_symlinked_workspace_dir_yields_one_entry_per_repo(tmp_path: Path) -> None:
  workspace = tmp_path / "workspace"
  workspace.mkdir()
  repo = _make_repo(workspace, "alpha")
  alias = tmp_path / "workspace-alias"
  alias.symlink_to(workspace)

  repos = _cfg(tmp_path, [str(workspace), str(alias)]).discover_repos()

  assert repos == [{"name": "alpha", "path": str(repo.resolve())}]


def test_missing_workspace_dir_is_skipped(tmp_path: Path) -> None:
  repos = _cfg(tmp_path, [str(tmp_path / "nope")]).discover_repos()

  assert repos == []
