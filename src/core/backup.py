"""Hourly backup system for critical CharlieBot state."""

import shutil
from datetime import datetime, timedelta

import structlog

from src.core.config import CharliBotConfig

log = structlog.get_logger()


class BackupManager:
  """Creates timestamped backups and prunes old ones."""

  def __init__(self, cfg: CharliBotConfig):
    self._cfg = cfg
    self._retention_days = 7

  async def run_backup(self) -> None:
    """Create a snapshot of all critical state files."""
    timestamp = datetime.utcnow().strftime("%Y-%m-%dT%H-%M-%S")
    backup_dir = self._cfg.backups_dir / timestamp
    backup_dir.mkdir(parents=True, exist_ok=True)

    # Global knowledge files
    for src_file in [self._cfg.memory_file, self._cfg.past_tasks_file, self._cfg.progress_file]:
      if src_file.exists():
        shutil.copy2(src_file, backup_dir / src_file.name)

    # Session state
    sessions_backup = backup_dir / "sessions"
    sessions_backup.mkdir()
    if self._cfg.sessions_dir.exists():
      for session_dir in self._cfg.sessions_dir.iterdir():
        if not session_dir.is_dir():
          continue
        s_backup = sessions_backup / session_dir.name
        s_backup.mkdir()
        for fname in ["metadata.json", "task_queue.json"]:
          src = session_dir / fname
          if src.exists():
            shutil.copy2(src, s_backup / fname)

    log.info("backup_created", path=str(backup_dir))
    await self._prune_old_backups()

  async def _prune_old_backups(self) -> None:
    """Delete backup directories older than retention_days."""
    cutoff = datetime.utcnow() - timedelta(days=self._retention_days)
    if not self._cfg.backups_dir.exists():
      return

    for d in self._cfg.backups_dir.iterdir():
      if not d.is_dir():
        continue
      try:
        dt = datetime.strptime(d.name, "%Y-%m-%dT%H-%M-%S")
        if dt < cutoff:
          shutil.rmtree(d)
          log.info("backup_pruned", path=str(d))
      except ValueError:
        pass  # Skip directories with unexpected names
