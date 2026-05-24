"""CRUD API for scheduled cron task configs (~/.charliebot/config.d/cron.yaml)."""

import asyncio
from pathlib import Path
from typing import Optional

import structlog
from fastapi import APIRouter, Depends, HTTPException
from pydantic import BaseModel

from src.core.config import CharlieBotConfig, get_config, get_scheduled_tasks
from src.core.yaml_utils import load_yaml, save_yaml

log = structlog.get_logger()
router = APIRouter()

CRON_PATH = Path.home() / '.charliebot' / 'config.d' / 'cron.yaml'


def _read_cron_yaml() -> dict:
  return load_yaml(CRON_PATH, default={'scheduled_tasks': []})


def _write_cron_yaml(data: dict):
  save_yaml(CRON_PATH, data)


def _validate_backend_id(backend: Optional[str], cfg: CharlieBotConfig) -> None:
  if backend and cfg.get_backend_option(backend) is None:
    raise HTTPException(status_code=400, detail=f"backend '{backend}' is not in backend_options")


class TaskUpdate(BaseModel):
  cron: Optional[str] = None
  prompt: Optional[str] = None
  repo: Optional[str] = None
  backend: Optional[str] = None
  timezone: Optional[str] = None
  enabled: Optional[bool] = None
  project: Optional[str] = None
  allow_failure: Optional[bool] = None


class TaskCreate(BaseModel):
  name: str
  cron: str
  prompt: str
  repo: Optional[str] = None
  backend: Optional[str] = None
  timezone: str = 'America/Los_Angeles'
  enabled: bool = True
  project: Optional[str] = None
  allow_failure: bool = False


@router.get('/tasks')
async def list_cron_tasks():
  """Return all scheduled tasks as JSON."""
  return [t.model_dump() for t in get_scheduled_tasks()]


@router.put('/tasks/{name}')
async def update_cron_task(name: str, req: TaskUpdate, cfg: CharlieBotConfig = Depends(get_config)):
  """Update an existing task by name (name is immutable)."""
  if 'backend' in req.model_fields_set:
    _validate_backend_id(req.backend, cfg)
  data = await asyncio.to_thread(_read_cron_yaml)
  tasks = data.get('scheduled_tasks', [])
  for task in tasks:
    if task.get('name') == name:
      if req.cron is not None:
        task['cron'] = req.cron
      if req.prompt is not None:
        task['prompt'] = req.prompt
      if req.repo is not None:
        task['repo'] = req.repo or None
      if 'backend' in req.model_fields_set:
        if req.backend:
          task['backend'] = req.backend
        else:
          task.pop('backend', None)
      if req.timezone is not None:
        task['timezone'] = req.timezone
      if req.enabled is not None:
        task['enabled'] = req.enabled
      if req.project is not None:
        task['project'] = req.project or None
      if req.allow_failure is not None:
        task['allow_failure'] = req.allow_failure
      data['scheduled_tasks'] = tasks
      await asyncio.to_thread(_write_cron_yaml, data)
      log.debug('cron_task_updated', name=name)
      return task
  raise HTTPException(status_code=404, detail=f'Task "{name}" not found')


@router.post('/tasks')
async def create_cron_task(req: TaskCreate, cfg: CharlieBotConfig = Depends(get_config)):
  """Add a new scheduled task."""
  _validate_backend_id(req.backend, cfg)
  data = await asyncio.to_thread(_read_cron_yaml)
  tasks = data.get('scheduled_tasks', [])
  if any(t.get('name') == req.name for t in tasks):
    raise HTTPException(status_code=409, detail=f'Task "{req.name}" already exists')
  payload = req.model_dump()
  if not payload.get('backend'):
    payload.pop('backend', None)
  new_task = {k: v for k, v in payload.items() if v is not None or k in ('name', 'cron', 'prompt', 'enabled')}
  tasks.append(new_task)
  data['scheduled_tasks'] = tasks
  await asyncio.to_thread(_write_cron_yaml, data)
  log.debug('cron_task_created', name=req.name)
  return new_task


@router.delete('/tasks/{name}')
async def delete_cron_task(name: str):
  """Remove a task by name."""
  data = await asyncio.to_thread(_read_cron_yaml)
  tasks = data.get('scheduled_tasks', [])
  filtered = [t for t in tasks if t.get('name') != name]
  if len(filtered) == len(tasks):
    raise HTTPException(status_code=404, detail=f'Task "{name}" not found')
  data['scheduled_tasks'] = filtered
  await asyncio.to_thread(_write_cron_yaml, data)
  log.debug('cron_task_deleted', name=name)
  return {'ok': True}
