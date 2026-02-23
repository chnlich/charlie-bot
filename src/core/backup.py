"""Core backup/restore logic for ~/.charliebot data."""

import tarfile
from datetime import datetime
from pathlib import Path
from typing import Optional

import structlog

log = structlog.get_logger()

CHARLIEBOT_DIR = Path.home() / '.charliebot'
BACKUP_DIR = Path.home() / '.charliebot_backup'

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
    if part in ('.git', 'credentials', '__pycache__') or part.endswith('.pyc'):
      return True
  # Exclude sessions/*/threads and everything under it
  if len(parts) >= 3 and parts[0] == 'sessions' and parts[2] == 'threads':
    return True
  return False


def _parse_backup_date(name: str) -> Optional[datetime]:
  """Parse datetime from a backup filename like charliebot-20260101-120000.tar.gz."""
  try:
    ts_part = name.removeprefix(_BACKUP_PREFIX).removesuffix(_BACKUP_SUFFIX)
    return datetime.strptime(ts_part, _TIMESTAMP_FMT)
  except (ValueError, AttributeError) as e:
    log.debug('backup_parse_date_failed', name=name, error=str(e))
    return None


def create_backup() -> Path:
  """Create a compressed backup of CHARLIEBOT_DIR.

  Excludes: .git, credentials, sessions/*/threads, *.pyc, __pycache__.

  Returns:
    Path to the created archive.
  """
  BACKUP_DIR.mkdir(parents=True, exist_ok=True)
  ts = datetime.now().strftime(_TIMESTAMP_FMT)
  archive_path = BACKUP_DIR / f'{_BACKUP_PREFIX}{ts}{_BACKUP_SUFFIX}'

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
      children = sorted(CHARLIEBOT_DIR.iterdir())
    except Exception as e:
      log.warning('backup_root_iter_failed', error=str(e))
      children = []
    for child in children:
      _add_recursive(tar, child, child.name)

  size_mb = archive_path.stat().st_size / (1024 * 1024)
  log.info('backup_created', path=str(archive_path), size_mb=round(size_mb, 2))
  return archive_path


def apply_retention(backup_dir: Path = BACKUP_DIR) -> None:
  """Apply tiered retention policy to backups in backup_dir.

  - Keep all backups from the last 7 days.
  - Keep Sunday-only backups from 7-30 days ago.
  - Keep 1st-of-month backups from 30-90 days ago.
  - Delete everything older than 90 days.
  """
  if not backup_dir.exists():
    return
  now = datetime.now()
  for backup_file in backup_dir.glob(f'{_BACKUP_PREFIX}*{_BACKUP_SUFFIX}'):
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


def restore_backup(archive_path: Path, target: Path = None) -> None:
  """Extract a backup archive to target directory.

  Args:
    archive_path: Path to the .tar.gz backup file.
    target: Destination directory. Defaults to CHARLIEBOT_DIR.
  """
  if target is None:
    target = CHARLIEBOT_DIR
  if target.exists():
    answer = input(f'Warning: {target} already exists. Overwrite? [y/N] ').strip().lower()
    if answer != 'y':
      print('Restore cancelled.')
      log.info('backup_restore_cancelled', archive=str(archive_path), target=str(target))
      return
  target.mkdir(parents=True, exist_ok=True)
  with tarfile.open(archive_path, 'r:gz') as tar:
    tar.extractall(path=target)
  log.info('backup_restored', archive=str(archive_path), target=str(target))


def list_backups(backup_dir: Path = None) -> list:
  """Return sorted list of backup files with size and date info.

  Args:
    backup_dir: Directory to scan. Defaults to BACKUP_DIR.

  Returns:
    List of dicts with keys: path, name, size, date.
  """
  if backup_dir is None:
    backup_dir = BACKUP_DIR
  if not backup_dir.exists():
    return []
  results = []
  for f in sorted(backup_dir.glob(f'{_BACKUP_PREFIX}*{_BACKUP_SUFFIX}')):
    try:
      stat = f.stat()
      results.append({
          'path': f,
          'name': f.name,
          'size': stat.st_size,
          'date': _parse_backup_date(f.name),
      })
    except Exception as e:
      log.warning('backup_list_stat_failed', path=str(f), error=str(e))
  return results
