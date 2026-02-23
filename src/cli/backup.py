"""CLI entry point for CharlieBot backup operations.

Usage:
  python3 -m src.cli.backup create
  python3 -m src.cli.backup list
  python3 -m src.cli.backup restore <file> [--target DIR]
"""

import argparse
import sys
from pathlib import Path

from src.core.backup import BACKUP_DIR, apply_retention, create_backup, list_backups, restore_backup


def _cmd_create(args: argparse.Namespace) -> None:
  archive = create_backup()
  apply_retention(BACKUP_DIR)
  print(f'Created: {archive}')


def _cmd_list(args: argparse.Namespace) -> None:
  backups = list_backups()
  if not backups:
    print('No backups found.')
    return
  print(f'{"Name":<40} {"Size":>10}  Date')
  print('-' * 70)
  for b in backups:
    size = b['size']
    size_str = f'{size / (1024 * 1024):.1f} MB' if size >= 1024 * 1024 else f'{size / 1024:.1f} KB'
    date_str = b['date'].strftime('%Y-%m-%d %H:%M:%S') if b['date'] else 'unknown'
    print(f'{b["name"]:<40} {size_str:>10}  {date_str}')


def _cmd_restore(args: argparse.Namespace) -> None:
  archive_path = Path(args.file)
  if not archive_path.is_absolute():
    archive_path = BACKUP_DIR / archive_path
  if not archive_path.exists():
    print(f'Error: {archive_path} not found.', file=sys.stderr)
    sys.exit(1)
  target = Path(args.target) if args.target else None
  restore_backup(archive_path, target)


def main() -> None:
  parser = argparse.ArgumentParser(description='CharlieBot backup management')
  sub = parser.add_subparsers(dest='command')
  sub.required = True

  sub.add_parser('create', help='Create a backup and apply retention policy')
  sub.add_parser('list', help='List available backups')
  restore_parser = sub.add_parser('restore', help='Restore from a backup file')
  restore_parser.add_argument('file', help='Backup filename or path')
  restore_parser.add_argument('--target', help='Target directory (default: ~/.charliebot)')

  args = parser.parse_args()
  {'create': _cmd_create, 'list': _cmd_list, 'restore': _cmd_restore}[args.command](args)


if __name__ == '__main__':
  main()
