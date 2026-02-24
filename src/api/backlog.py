"""Backlog API routes — read/write project backlog.yaml and history.yaml."""

import asyncio
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


@router.get('')
async def get_backlog(repo: str | None = None):
  """Return backlog items from {repo}/loop/backlog.yaml."""
  path = _repo_path(repo) / 'loop' / 'backlog.yaml'
  if not path.exists():
    return JSONResponse(content=[], status_code=200)
  items = yaml.safe_load(path.read_text(encoding='utf-8')) or []
  return JSONResponse(content=items)


@router.get('/history')
async def get_history(repo: str | None = None):
  """Return history entries from {repo}/loop/history.yaml."""
  path = _repo_path(repo) / 'loop' / 'history.yaml'
  if not path.exists():
    return JSONResponse(content=[], status_code=200)
  items = yaml.safe_load(path.read_text(encoding='utf-8')) or []
  return JSONResponse(content=items)


class BacklogPatch(BaseModel):
  status: str | None = None
  priority: str | None = None


@router.patch('/{item_id}')
async def patch_backlog(item_id: str, patch: BacklogPatch, repo: str | None = None):
  """Update status/priority of a backlog item, then git commit+push."""
  repo_path = _repo_path(repo)
  yaml_path = repo_path / 'loop' / 'backlog.yaml'
  if not yaml_path.exists():
    return JSONResponse(content={'error': 'backlog.yaml not found'}, status_code=404)

  items = yaml.safe_load(yaml_path.read_text(encoding='utf-8')) or []
  updated = None
  for item in items:
    if str(item.get('id')) == item_id:
      if patch.status is not None:
        item['status'] = patch.status
      if patch.priority is not None:
        item['priority'] = patch.priority
      updated = item
      break

  if updated is None:
    return JSONResponse(content={'error': f'Item {item_id} not found'}, status_code=404)

  yaml_path.write_text(yaml.safe_dump(items, allow_unicode=True, sort_keys=False), encoding='utf-8')
  log.info('backlog_updated', item_id=item_id, **patch.model_dump(exclude_none=True))

  status_label = patch.status or 'updated'
  asyncio.create_task(_git_commit_push(repo_path, item_id, status_label))

  return JSONResponse(content=updated)


async def _git_commit_push(repo_path: Path, item_id: str, status: str):
  """Fire-and-forget: git add + commit + push backlog.yaml."""
  try:
    add = await asyncio.create_subprocess_exec(
      'git', '-C', str(repo_path), 'add', 'loop/backlog.yaml',
      stdout=asyncio.subprocess.DEVNULL,
      stderr=asyncio.subprocess.DEVNULL,
    )
    await add.wait()
    commit = await asyncio.create_subprocess_exec(
      'git', '-C', str(repo_path), 'commit', '-m',
      f'backlog: update {item_id} status to {status}',
      stdout=asyncio.subprocess.DEVNULL,
      stderr=asyncio.subprocess.DEVNULL,
    )
    await commit.wait()
    push = await asyncio.create_subprocess_exec(
      'git', '-C', str(repo_path), 'push',
      stdout=asyncio.subprocess.DEVNULL,
      stderr=asyncio.subprocess.DEVNULL,
    )
    await push.wait()
  except Exception as e:
    log.warning('backlog_git_push_failed', item_id=item_id, error=str(e))
