"""Tests for the `charliebot gc-trash` CLI: dry-run lists; --yes hard-deletes."""

import pathlib
import sys

import conftest
import pytest

from src.infra import config
from src.runtime.cli import gc_trash


def _cfg_with_trash(tmp_path: pathlib.Path) -> config.CharlieBotConfig:
  cfg = conftest.build_worktree_cfg(tmp_path)
  trash = pathlib.Path(cfg.paths.worktree_dir) / ".trash"
  for name in ("charliebot-task-a", "charliebot-task-b"):
    entry = trash / name
    entry.mkdir(parents=True)
    (entry / "diff.txt").write_text("content", encoding="utf-8")
  return cfg


def test_gc_trash_dry_run_lists_but_deletes_nothing(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _cfg_with_trash(tmp_path)
  trash = pathlib.Path(cfg.paths.worktree_dir) / ".trash"
  monkeypatch.setattr(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr(sys, "argv", ["charliebot gc-trash"])

  gc_trash.main()

  out = capsys.readouterr().out
  assert "charliebot-task-a" in out
  assert "charliebot-task-b" in out
  assert "Dry run" in out
  # Nothing deleted.
  assert (trash / "charliebot-task-a").exists()
  assert (trash / "charliebot-task-b").exists()


def test_gc_trash_yes_hard_deletes(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _cfg_with_trash(tmp_path)
  trash = pathlib.Path(cfg.paths.worktree_dir) / ".trash"
  monkeypatch.setattr(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr(sys, "argv", ["charliebot gc-trash", "--yes"])

  gc_trash.main()

  out = capsys.readouterr().out
  assert "deleted" in out
  assert not (trash / "charliebot-task-a").exists()
  assert not (trash / "charliebot-task-b").exists()
  # Trash dir itself remains; it is just empty now.
  assert not list(trash.iterdir())


def test_gc_trash_empty_reports_and_returns(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = conftest.build_worktree_cfg(tmp_path)
  monkeypatch.setattr(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, lambda: cfg)
  monkeypatch.setattr(sys, "argv", ["charliebot gc-trash", "--yes"])

  gc_trash.main()

  out = capsys.readouterr().out
  assert "empty" in out.lower()
