"""Tests for src/core/backup.py (BackupManager)."""

import shutil
from datetime import datetime, timedelta
from pathlib import Path

import pytest

from src.core.backup import BackupManager
from src.core.config import load_config


@pytest.fixture()
def backup_mgr(tmp_home):
    cfg = load_config()
    # Create required directories and seed files
    cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
    cfg.backups_dir.mkdir(parents=True, exist_ok=True)
    cfg.memory_file.write_text("# MEMORY\n", encoding="utf-8")
    cfg.past_tasks_file.write_text("# PAST TASKS\n", encoding="utf-8")
    cfg.progress_file.write_text("# PROGRESS\n", encoding="utf-8")
    return BackupManager(cfg)


@pytest.mark.asyncio
class TestBackupManager:
    async def test_run_backup_creates_directory(self, backup_mgr):
        cfg = load_config()
        await backup_mgr.run_backup()
        dirs = list(cfg.backups_dir.iterdir())
        assert len(dirs) == 1

    async def test_run_backup_copies_memory_files(self, backup_mgr):
        cfg = load_config()
        await backup_mgr.run_backup()
        backup_dir = next(cfg.backups_dir.iterdir())
        assert (backup_dir / "MEMORY.md").exists()
        assert (backup_dir / "PAST_TASKS.md").exists()
        assert (backup_dir / "PROGRESS.md").exists()

    async def test_run_backup_creates_sessions_subdir(self, backup_mgr):
        cfg = load_config()
        # Add a fake session
        sess_dir = cfg.sessions_dir / "sess-abc"
        sess_dir.mkdir()
        (sess_dir / "metadata.json").write_text('{"id":"sess-abc"}')
        (sess_dir / "task_queue.json").write_text('{"tasks":[]}')

        await backup_mgr.run_backup()
        backup_dir = next(cfg.backups_dir.iterdir())
        assert (backup_dir / "sessions").is_dir()
        assert (backup_dir / "sessions" / "sess-abc" / "metadata.json").exists()
        assert (backup_dir / "sessions" / "sess-abc" / "task_queue.json").exists()

    async def test_run_backup_skips_missing_optional_session_files(self, backup_mgr):
        cfg = load_config()
        sess_dir = cfg.sessions_dir / "sess-minimal"
        sess_dir.mkdir()
        # Only metadata.json, no task_queue.json

        await backup_mgr.run_backup()
        backup_dir = next(cfg.backups_dir.iterdir())
        sess_backup = backup_dir / "sessions" / "sess-minimal"
        assert not (sess_backup / "task_queue.json").exists()

    async def test_prune_removes_old_backups(self, backup_mgr):
        cfg = load_config()
        # Create a backup directory older than 7 days
        old_dt = datetime.utcnow() - timedelta(days=8)
        old_dir = cfg.backups_dir / old_dt.strftime("%Y-%m-%dT%H-%M-%S")
        old_dir.mkdir()
        (old_dir / "MEMORY.md").write_text("old")

        await backup_mgr.run_backup()

        assert not old_dir.exists(), "Old backup should have been pruned"

    async def test_prune_keeps_recent_backups(self, backup_mgr):
        cfg = load_config()
        # Create a recent backup dir (1 day old)
        recent_dt = datetime.utcnow() - timedelta(days=1)
        recent_dir = cfg.backups_dir / recent_dt.strftime("%Y-%m-%dT%H-%M-%S")
        recent_dir.mkdir()

        await backup_mgr.run_backup()

        assert recent_dir.exists(), "Recent backup should be kept"

    async def test_prune_ignores_non_timestamp_dirs(self, backup_mgr):
        cfg = load_config()
        # Directory with unexpected name
        weird_dir = cfg.backups_dir / "not-a-timestamp"
        weird_dir.mkdir()

        await backup_mgr.run_backup()

        assert weird_dir.exists(), "Non-timestamp dirs should be ignored by prune"

    async def test_multiple_backups_accumulate(self, backup_mgr):
        import asyncio
        cfg = load_config()
        await backup_mgr.run_backup()
        await asyncio.sleep(1.1)  # Ensure a distinct timestamp (1-second resolution)
        await backup_mgr.run_backup()
        dirs = [d for d in cfg.backups_dir.iterdir() if d.is_dir()]
        assert len(dirs) >= 2
