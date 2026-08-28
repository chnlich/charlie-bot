"""Tests for the `charliebot gc-trash` CLI: dry-run lists; --yes hard-deletes."""

import sys
from pathlib import Path

import pytest

from src.cli import gc_trash
from src.core.config import CharlieBotConfig


def _cfg_with_trash(tmp_path: Path) -> CharlieBotConfig:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", worktree_dir=str(tmp_path / "worktrees"))
  trash = Path(cfg.worktree_dir) / ".trash"
  for name in ("charliebot-task-a", "charliebot-task-b"):
    entry = trash / name
    entry.mkdir(parents=True)
    (entry / "diff.txt").write_text("content", encoding="utf-8")
  return cfg


def test_gc_trash_dry_run_lists_but_deletes_nothing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _cfg_with_trash(tmp_path)
  trash = Path(cfg.worktree_dir) / ".trash"
  monkeypatch.setattr(gc_trash, "get_config", lambda: cfg)
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
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _cfg_with_trash(tmp_path)
  trash = Path(cfg.worktree_dir) / ".trash"
  monkeypatch.setattr(gc_trash, "get_config", lambda: cfg)
  monkeypatch.setattr(sys, "argv", ["charliebot gc-trash", "--yes"])

  gc_trash.main()

  out = capsys.readouterr().out
  assert "deleted" in out
  assert not (trash / "charliebot-task-a").exists()
  assert not (trash / "charliebot-task-b").exists()
  # Trash dir itself remains; it is just empty now.
  assert not list(trash.iterdir())


def test_gc_trash_empty_reports_and_returns(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", worktree_dir=str(tmp_path / "worktrees"))
  monkeypatch.setattr(gc_trash, "get_config", lambda: cfg)
  monkeypatch.setattr(sys, "argv", ["charliebot gc-trash", "--yes"])

  gc_trash.main()

  out = capsys.readouterr().out
  assert "empty" in out.lower()
