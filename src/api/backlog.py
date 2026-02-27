"""Backlog API routes — read/write project backlog.yaml and history.yaml."""

import asyncio
from datetime import datetime, timezone
from pathlib import Path

import structlog
import yaml
from fastapi import APIRouter
from fastapi.responses import JSONResponse
from pydantic import BaseModel

log = structlog.get_logger()

router = APIRouter()


def _repo_path(repo: str | None) -> Path:
  if repo:
    return Path(repo).expanduser()
  from src.core.config import get_config
  cfg = get_config()
  if not cfg.backlog_repo:
    raise ValueError('backlog_repo not configured in config.yaml')
  return Path(cfg.backlog_repo)


def _load_all_items(repo_path: Path) -> list[dict]:
  """Load items from loop/backlogs/*.yaml (with _source), or fall back to loop/backlog.yaml."""
  backlogs_dir = repo_path / 'loop' / 'backlogs'
  if backlogs_dir.is_dir():
    items = []
    for yaml_file in sorted(backlogs_dir.glob('*.yaml')):
      source = yaml_file.stem
      file_items = yaml.safe_load(yaml_file.read_text(encoding='utf-8')) or []
      for item in file_items:
        item['_source'] = source
      items.extend(file_items)
    return items

  path = repo_path / 'loop' / 'backlog.yaml'
  if not path.exists():
    return []
  items = yaml.safe_load(path.read_text(encoding='utf-8')) or []
  for item in items:
    item.setdefault('_source', 'backlog')
  return items


def _find_item_file(repo_path: Path, item_id: str, source: str | None = None) -> tuple[Path | None, list | None]:
  """Return (yaml_path, items) for the file containing item_id, or (None, None).

  If *source* is given (e.g. 'alpha-lab-backtest'), only search that file —
  this disambiguates duplicate IDs across per-module backlogs.
  """
  backlogs_dir = repo_path / 'loop' / 'backlogs'
  if backlogs_dir.is_dir():
    files = sorted(backlogs_dir.glob('*.yaml'))
    if source:
      files = [f for f in files if f.stem == source]
    for yaml_file in files:
      items = yaml.safe_load(yaml_file.read_text(encoding='utf-8')) or []
      if any(str(i.get('id')) == item_id for i in items):
        return yaml_file, items
    return None, None

  path = repo_path / 'loop' / 'backlog.yaml'
  if not path.exists():
    return None, None
  items = yaml.safe_load(path.read_text(encoding='utf-8')) or []
  if any(str(i.get('id')) == item_id for i in items):
    return path, items
  return None, None


@router.get('')
async def get_backlog(repo: str | None = None):
  """Return backlog items from loop/backlogs/*.yaml or fallback loop/backlog.yaml."""
  repo_path = _repo_path(repo)
  items = await asyncio.to_thread(_load_all_items, repo_path)
  return JSONResponse(content=items)


@router.get('/history')
async def get_history(repo: str | None = None):
  """Return history entries from {repo}/loop/history.yaml."""
  path = _repo_path(repo) / 'loop' / 'history.yaml'
  if not path.exists():
    return JSONResponse(content=[], status_code=200)
  items = await asyncio.to_thread(lambda: yaml.safe_load(path.read_text(encoding='utf-8')) or [])
  return JSONResponse(content=items)


class BacklogPatch(BaseModel):
  status: str | None = None
  priority: str | None = None
  rejected_reason: str | None = None


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
        item['status'] = patch.status
        if patch.status == 'rejected':
          item['rejected_at'] = datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%S')
          if patch.rejected_reason:
            item['rejected_reason'] = patch.rejected_reason
          else:
            item.pop('rejected_reason', None)
        elif patch.status == 'pending':
          item.pop('rejected_reason', None)
          item.pop('rejected_at', None)
      if patch.priority is not None:
        item['priority'] = patch.priority
      updated = item
      break

  if updated is None:
    return JSONResponse(content={'error': f'Item {item_id} not found'}, status_code=404)

  await asyncio.to_thread(
      yaml_path.write_text, yaml.safe_dump(items, allow_unicode=True, sort_keys=False), encoding='utf-8')
  log.info('backlog_updated', item_id=item_id, file=str(yaml_path), **patch.model_dump(exclude_none=True))

  git_rel = str(yaml_path.relative_to(repo_path))
  status_label = patch.status or 'updated'
  asyncio.create_task(_git_commit_push(repo_path, git_rel, item_id, status_label))

  resp = {k: v for k, v in updated.items() if k != '_source'}
  return JSONResponse(content=resp)


async def _git_commit_push(repo_path: Path, git_rel: str, item_id: str, status: str):
  """Fire-and-forget: git add + commit + push the modified backlog file."""
  try:
    add = await asyncio.create_subprocess_exec(
        'git',
        '-C',
        str(repo_path),
        'add',
        git_rel,
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.DEVNULL,
    )
    await add.wait()
    commit = await asyncio.create_subprocess_exec(
        'git',
        '-C',
        str(repo_path),
        'commit',
        '-m',
        f'backlog: update {item_id} status to {status}',
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.DEVNULL,
    )
    await commit.wait()
    push = await asyncio.create_subprocess_exec(
        'git',
        '-C',
        str(repo_path),
        'push',
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.DEVNULL,
    )
    await push.wait()
  except Exception as e:
    log.warning('backlog_git_push_failed', item_id=item_id, error=str(e))
