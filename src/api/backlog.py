"""Backlog API routes — read/write project backlog.yaml and history.yaml."""

import asyncio
from datetime import datetime, timezone
from pathlib import Path

import structlog
from fastapi import APIRouter
from fastapi.responses import JSONResponse
from pydantic import BaseModel

from src.core.git import git_add_commit_push
from src.core.tasks import create_logged_task
from src.core.yaml_utils import load_yaml, save_yaml

log = structlog.get_logger()

router = APIRouter()


def _repo_path(repo: str | None) -> Path:
  if repo:
    return Path(repo).expanduser()
  from src.core.config import get_config
  cfg = get_config()
  if cfg.backlog_repos:
    return Path(cfg.backlog_repos[0].path)
  if cfg.backlog_repo:
    return Path(cfg.backlog_repo)
  raise ValueError('backlog_repos not configured in config.yaml')


def _load_all_items(repo_path: Path) -> list[dict]:
  """Load items from backlog/backlogs/*.yaml (with _source), or fall back to backlog/backlog.yaml."""
  backlogs_dir = repo_path / 'backlog' / 'backlogs'
  if backlogs_dir.is_dir():
    items = []
    for yaml_file in sorted(backlogs_dir.glob('*.yaml')):
      source = yaml_file.stem
      file_items = load_yaml(yaml_file, default=[])
      for item in file_items:
        item['_source'] = source
      items.extend(file_items)
    return items

  path = repo_path / 'backlog' / 'backlog.yaml'
  if not path.exists():
    return []
  items = load_yaml(path, default=[])
  for item in items:
    item.setdefault('_source', 'backlog')
  return items


def _find_item_file(repo_path: Path, item_id: str, source: str | None = None) -> tuple[Path | None, list | None]:
  """Return (yaml_path, items) for the file containing item_id, or (None, None).

  If *source* is given (e.g. 'alpha-lab-backtest'), only search that file —
  this disambiguates duplicate IDs across per-module backlogs.
  """
  backlogs_dir = repo_path / 'backlog' / 'backlogs'
  if backlogs_dir.is_dir():
    files = sorted(backlogs_dir.glob('*.yaml'))
    if source:
      files = [f for f in files if f.stem == source]
    for yaml_file in files:
      items = load_yaml(yaml_file, default=[])
      if any(str(i.get('id')) == item_id for i in items):
        return yaml_file, items
    return None, None

  path = repo_path / 'backlog' / 'backlog.yaml'
  if not path.exists():
    return None, None
  items = load_yaml(path, default=[])
  if any(str(i.get('id')) == item_id for i in items):
    return path, items
  return None, None


@router.get('/repos')
async def get_repos():
  """Return configured backlog repos [{label, path}]."""
  from src.core.config import get_config
  cfg = get_config()
  return JSONResponse(content=[{"label": r.label, "path": r.path} for r in cfg.backlog_repos])


@router.get('')
async def get_backlog(repo: str | None = None):
  """Return backlog items from backlog/backlogs/*.yaml or fallback backlog/backlog.yaml."""
  repo_path = _repo_path(repo)
  items = await asyncio.to_thread(_load_all_items, repo_path)
  return JSONResponse(content=items)


@router.get('/history')
async def get_history(repo: str | None = None):
  """Return history entries from {repo}/backlog/history-*.yaml files, sorted by timestamp descending."""
  loop_dir = _repo_path(repo) / 'backlog'
  files = sorted(loop_dir.glob('history-*.yaml'))
  if not files:
    return JSONResponse(content=[], status_code=200)

  def _load() -> list[dict]:
    items: list[dict] = []
    for f in files:
      items.extend(load_yaml(f, default=[]))
    items.sort(key=lambda e: e.get('timestamp', ''), reverse=True)
    return items

  return JSONResponse(content=await asyncio.to_thread(_load))


class BacklogPatch(BaseModel):
  status: str | None = None
  priority: str | None = None
  rejected_reason: str | None = None
  failed_reason: str | None = None
  revision_feedback: str | None = None


def _apply_status_transition(item: dict, patch: BacklogPatch) -> None:
  """Mutate *item* to reflect transition to *patch.status* (timestamps, reasons, counters)."""
  item['status'] = patch.status
  now = datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%S')
  if patch.status == 'rejected':
    item['rejected_at'] = now
    if patch.rejected_reason:
      item['rejected_reason'] = patch.rejected_reason
    else:
      item.pop('rejected_reason', None)
    item.pop('revision_feedback', None)
    item.pop('revision_requested_at', None)
  elif patch.status == 'failed':
    item['failed_at'] = now
    if patch.failed_reason:
      item['failed_reason'] = patch.failed_reason
    else:
      item.pop('failed_reason', None)
    item['failed_count'] = item.get('failed_count', 0) + 1
  elif patch.status == 'revision_requested':
    item['revision_requested_at'] = now
    if patch.revision_feedback:
      item['revision_feedback'] = patch.revision_feedback
    else:
      item.pop('revision_feedback', None)
  elif patch.status == 'approved':
    item.pop('failed_at', None)
    item.pop('failed_reason', None)
    item.pop('revision_feedback', None)
    item.pop('revision_requested_at', None)
  elif patch.status == 'pending':
    item.pop('rejected_reason', None)
    item.pop('rejected_at', None)
    item.pop('revision_feedback', None)
    item.pop('revision_requested_at', None)


@router.patch('/{item_id}')
async def patch_backlog(item_id: str, patch: BacklogPatch, repo: str | None = None, source: str | None = None):
  """Update status/priority of a backlog item, then git commit+push."""
  repo_path = _repo_path(repo)
  yaml_path, items = await asyncio.to_thread(_find_item_file, repo_path, item_id, source)
  if yaml_path is None:
    return JSONResponse(content={'error': f'Item {item_id} not found'}, status_code=404)

  updated = None
  for item in items:
    if str(item.get('id')) == item_id:
      if patch.status is not None:
        _apply_status_transition(item, patch)
      if patch.priority is not None:
        item['priority'] = patch.priority
      updated = item
      break

  if updated is None:
    return JSONResponse(content={'error': f'Item {item_id} not found'}, status_code=404)

  await asyncio.to_thread(save_yaml, yaml_path, items)
  log.info('backlog_updated', item_id=item_id, file=str(yaml_path), **patch.model_dump(exclude_none=True))

  git_rel = str(yaml_path.relative_to(repo_path))
  status_label = patch.status or 'updated'
  create_logged_task(git_add_commit_push(repo_path, [git_rel], f'backlog: update {item_id} status to {status_label}'))

  resp = {k: v for k, v in updated.items() if k != '_source'}
  return JSONResponse(content=resp)
