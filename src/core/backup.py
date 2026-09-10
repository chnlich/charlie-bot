"""Core backup logic for a CharlieBot profile's state directory."""

import tarfile
from datetime import datetime
from pathlib import Path

import structlog

from src.core.config import charliebot_home_dir
from src.core.threads import THREADS_DIR_NAME

log = structlog.get_logger()


def charliebot_dir() -> Path:
  """The state directory being backed up: this profile's home."""
  return charliebot_home_dir()


def backup_dir() -> Path:
  """Where this profile's archives are written, a sibling of its home.

  The default home yields ``~/.charliebot_backup``.
  """
  home = charliebot_home_dir()
  return home.with_name(home.name + '_backup')


_TIMESTAMP_FMT = '%Y%m%d-%H%M%S'
_BACKUP_PREFIX = 'charliebot-'
_BACKUP_SUFFIX = '.tar.gz'

# Age thresholds in days
_DAILY_THRESHOLD = 7
_WEEKLY_THRESHOLD = 30
_MONTHLY_THRESHOLD = 90


def _should_exclude(arcname: str) -> bool:
  """Return True if the archive member (relative path) should be excluded."""
  parts = Path(arcname).parts
  for part in parts:
    if part in ('.git', '.claude', 'credentials', '__pycache__') or part.endswith('.pyc'):
      return True
  # Exclude sessions/*/threads and everything under it
  return len(parts) >= 3 and parts[0] == 'sessions' and parts[2] == THREADS_DIR_NAME


def _parse_backup_date(name: str) -> datetime | None:
  """Parse datetime from a backup filename like charliebot-20260101-120000.tar.gz."""
  try:
    ts_part = name.removeprefix(_BACKUP_PREFIX).removesuffix(_BACKUP_SUFFIX)
    return datetime.strptime(ts_part, _TIMESTAMP_FMT)
  except (ValueError, AttributeError) as e:
    log.debug('backup_parse_date_failed', name=name, error=str(e))
    return None


def create_backup() -> Path:
  """Create a compressed backup of this profile's state directory.

  Excludes: .git, credentials, sessions/*/threads, *.pyc, __pycache__.

  Returns:
    Path to the created archive.
  """
  target_dir = backup_dir()
  target_dir.mkdir(parents=True, exist_ok=True)
  ts = datetime.now().strftime(_TIMESTAMP_FMT)
  archive_path = target_dir / f'{_BACKUP_PREFIX}{ts}{_BACKUP_SUFFIX}'

  def _add_recursive(tar: tarfile.TarFile, path: Path, arcname: str) -> None:
    if _should_exclude(arcname):
      return
    try:
      tar.add(path, arcname=arcname, recursive=False)
    except Exception as e:
      log.warning('backup_skip_file', path=str(path), error=str(e))
      return
    if path.is_dir():
      try:
        children = sorted(path.iterdir())
      except Exception as e:
        log.warning('backup_skip_dir', path=str(path), error=str(e))
        return
      for child in children:
        _add_recursive(tar, child, str(Path(arcname) / child.name))

  with tarfile.open(archive_path, 'w:gz') as tar:
    try:
      children = sorted(charliebot_dir().iterdir())
    except Exception as e:
      log.warning('backup_root_iter_failed', error=str(e))
      children = []
    for child in children:
      _add_recursive(tar, child, child.name)

  size_mb = archive_path.stat().st_size / (1024 * 1024)
  log.info('backup_created', path=str(archive_path), size_mb=round(size_mb, 2))
  return archive_path


def apply_retention(target_dir: Path | None = None) -> None:
  """Apply tiered retention policy to backups in target_dir (default: this profile's).

  - Keep all backups from the last 7 days.
  - Keep Sunday-only backups from 7-30 days ago.
  - Keep 1st-of-month backups from 30-90 days ago.
  - Delete everything older than 90 days.
  """
  if target_dir is None:
    target_dir = backup_dir()
  if not target_dir.exists():
    return
  now = datetime.now()
  for backup_file in target_dir.glob(f'{_BACKUP_PREFIX}*{_BACKUP_SUFFIX}'):
    backup_date = _parse_backup_date(backup_file.name)
    if backup_date is None:
      log.debug('backup_retention_skip_unparseable', name=backup_file.name)
      continue
    age = (now - backup_date).days
    if age < _DAILY_THRESHOLD:
      keep = True
    elif age < _WEEKLY_THRESHOLD:
      keep = backup_date.weekday() == 6  # Sunday
    elif age < _MONTHLY_THRESHOLD:
      keep = backup_date.day == 1  # 1st of month
    else:
      keep = False
    if not keep:
      try:
        backup_file.unlink()
        log.info('backup_deleted', name=backup_file.name, age_days=age)
      except Exception as e:
        log.warning('backup_delete_failed', name=backup_file.name, error=str(e))
