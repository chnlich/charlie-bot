"""CRUD API for scheduled cron task configs (config.d/cron.d/<name>.yaml)."""

import asyncio
import copy
import re
from pathlib import Path
from typing import Literal

import structlog
from fastapi import APIRouter, Depends, HTTPException
from pydantic import BaseModel

from src.api.deps import get_session_manager
from src.core.config import (
  DEFAULT_TIMEZONE,
  CharlieBotConfig,
  ScheduledTaskConfig,
  _load_cron_file,
  _validate_cron_body,
  get_config,
  get_scheduled_task_errors,
  get_scheduled_tasks,
)
from src.core.config import (
  cron_dir as _core_cron_dir,
)
from src.core.models import PROJECT_ROLE, SessionMetadata
from src.core.scheduler import effective_scheduled_task_backend
from src.core.sessions import ScheduledSessionBusyError, SessionManager
from src.core.yaml_utils import load_yaml, save_yaml

log = structlog.get_logger()
router = APIRouter()

_CRON_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")


def cron_dir() -> Path:
  """Path of this profile's per-job cron config directory, from the canonical core helper."""
  return _core_cron_dir()


def cron_path(name: str) -> Path:
  """Path of one job's cron config file. Resolved per call, never at import."""
  return cron_dir() / f'{name}.yaml'


def _read_cron_yaml(name: str) -> dict:
  return load_yaml(cron_path(name), default={})


def _write_cron_yaml(name: str, data: dict):
  save_yaml(cron_path(name), data)


def _validate_backend_id(backend: str | None, cfg: CharlieBotConfig) -> None:
  if backend and cfg.get_backend_option(backend) is None:
    raise HTTPException(status_code=400, detail=f"backend '{backend}' is not in backend_options")


def _apply_task_update(task: dict, req: "TaskUpdate") -> dict:
  updated = dict(task)
  if req.cron is not None:
    updated['cron'] = req.cron
  if req.prompt_file is not None:
    updated['prompt_file'] = req.prompt_file
  if req.repo is not None:
    updated['repo'] = req.repo or None
  if 'backend' in req.model_fields_set:
    if req.backend:
      updated['backend'] = req.backend
    else:
      updated.pop('backend', None)
  if req.timezone is not None:
    updated['timezone'] = req.timezone
  if req.enabled is not None:
    updated['enabled'] = req.enabled
  if req.project is not None:
    updated['project'] = req.project or None
  if req.allow_failure is not None:
    updated['allow_failure'] = req.allow_failure
  return updated


async def _ensure_backend_update_session(
    name: str,
    cand_model: ScheduledTaskConfig,
    req: "TaskUpdate",
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
) -> SessionMetadata | None:
  if 'backend' not in req.model_fields_set:
    return None
  backend = effective_scheduled_task_backend(cand_model, cfg)
  role: str | None = None
  group: str | None = None
  if cand_model.mode == 'master':
    role = PROJECT_ROLE
    group = cand_model.project
  try:
    return await session_mgr.ensure_scheduled_session_backend(name, backend, role=role, group=group)
  except ScheduledSessionBusyError as e:
    raise HTTPException(status_code=409, detail=str(e)) from e


def _check_master_project_unique(name: str, mode: str | None, project: str | None) -> None:
  """Reject when another mode: master task already carries the same project (group).

  At most one active mode: master task per group, so at most one live
  role=project session per group. The check names the conflicting task.
  """
  if mode != 'master':
    return
  for other in get_scheduled_tasks():
    if other.name != name and other.mode == 'master' and other.project == project:
      raise HTTPException(
          status_code=409,
          detail=(
              f"mode 'master' task for project '{project}' already exists: '{other.name}' "
              "(at most one Project Manager task per group)"))


class TaskUpdate(BaseModel):
  cron: str | None = None
  prompt_file: str | None = None
  repo: str | None = None
  backend: str | None = None
  timezone: str | None = None
  enabled: bool | None = None
  project: str | None = None
  allow_failure: bool | None = None


class TaskCreate(BaseModel):
  name: str
  cron: str
  prompt_file: str | None = None
  repo: str | None = None
  backend: str | None = None
  timezone: str = DEFAULT_TIMEZONE
  enabled: bool = True
  project: str | None = None
  mode: Literal['worker', 'master'] | None = None
  allow_failure: bool = False


@router.get('/tasks')
async def list_cron_tasks():
  """Return all scheduled tasks plus one error entry per broken file, never 500.

  Valid jobs are sorted by name, followed by one entry per error record shaped
  ``{"name", "error", "broken": True, "path": str, "enabled": bool | None}`` —
  ``path`` is the failing file's absolute path and ``enabled`` its raw value
  (None when the body could not be parsed). A broken or legacy file must never
  cause this route to fail.
  """
  valid = [t.model_dump() for t in get_scheduled_tasks()]
  broken = [
      {'name': e.name, 'error': e.error, 'broken': True, 'path': e.path, 'enabled': e.enabled}
      for e in get_scheduled_task_errors()]
  return valid + broken


async def apply_task_yaml_update(
    name: str,
    req: TaskUpdate,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
) -> tuple[dict, SessionMetadata | None]:
  """Apply a ``TaskUpdate`` to one job's yaml: load, validate, rotate, write.

  Shared by the cron PUT route and the sessions write-through switch. Returns
  the candidate task body (the PUT response) and the rotated/ensured
  ``SessionMetadata`` (None when the request carries no backend change or the
  session already matches). Raises HTTPException with the cron editor's exact
  contract: 404 when the task file is missing, 400 on a non-recognized
  backend, and 409 when the loader fails or the dedicated session is busy
  (busy 409 surfaces before any yaml write).
  """
  path = cron_path(name)
  if not path.exists():
    raise HTTPException(status_code=404, detail=f'Task "{name}" not found')
  if 'backend' in req.model_fields_set:
    _validate_backend_id(req.backend, cfg)
  # A syntax-error or otherwise unparseable file body must surface as a 409 with
  # the loader's error text, never a 500. Validate via the loader on the real
  # file so the error matches what the list route reports for the job. Mirrors
  # the loader's own catch-all: any failure in _load_cron_file becomes this
  # job's error, never an unhandled exception.
  try:
    await asyncio.to_thread(_load_cron_file, cron_path(name), cfg.charlie_bot_repo, name)
    raw = await asyncio.to_thread(_read_cron_yaml, name)
  except Exception as e:
    raise HTTPException(status_code=409, detail=str(e)) from e

  candidate = _apply_task_update(raw, req)
  # Validate the candidate through the exact same body-processing code the
  # production loader uses, on a deep copy (that code mutates the body in
  # place — see _validate_cron_body) so the persisted file is always
  # reloadable and file-format-only keys like `prompt_file` can never surface
  # as an unhandled ValidationError.
  try:
    cand_model, _ = await asyncio.to_thread(
        _validate_cron_body, copy.deepcopy(candidate), cfg.charlie_bot_repo, name)
  except Exception as e:
    raise HTTPException(status_code=409, detail=str(e)) from e

  rotated: SessionMetadata | None = None
  if cand_model.enabled:
    _check_master_project_unique(name, cand_model.mode, cand_model.project)
  rotated = await _ensure_backend_update_session(name, cand_model, req, cfg, session_mgr)
  await asyncio.to_thread(_write_cron_yaml, name, candidate)
  log.debug('cron_task_updated', name=name)
  return candidate, rotated


@router.put('/tasks/{name}')
async def update_cron_task(
    name: str,
    req: TaskUpdate,
    cfg: CharlieBotConfig = Depends(get_config),
    session_mgr: SessionManager = Depends(get_session_manager),
):
  if not _CRON_NAME_RE.fullmatch(name):
    raise HTTPException(status_code=400, detail=f'invalid cron task name: {name!r}')
  candidate, _ = await apply_task_yaml_update(name, req, cfg, session_mgr)
  return candidate


@router.post('/tasks')
async def create_cron_task(req: TaskCreate, cfg: CharlieBotConfig = Depends(get_config)):
  """Add a new scheduled job as its own config.d/cron.d/<name>.yaml file."""
  if not _CRON_NAME_RE.fullmatch(req.name):
    raise HTTPException(status_code=400, detail=f'invalid cron name: {req.name!r}')
  _validate_backend_id(req.backend, cfg)
  if req.mode == 'master' and not req.project:
    raise HTTPException(status_code=400, detail="mode 'master' requires 'project' (the group the PM session is bound to)")
  _check_master_project_unique(req.name, req.mode, req.project)
  path = cron_path(req.name)
  if path.exists():
    raise HTTPException(status_code=409, detail=f'Task "{req.name}" already exists')
  payload = req.model_dump()
  if not payload.get('backend'):
    payload.pop('backend', None)
  body = {k: v for k, v in payload.items()
          if k != 'name' and (v is not None or k in ('cron', 'enabled'))}
  # Validate the assembled body through the exact same body-processing code
  # the production loader uses, on a deep copy (that code mutates its argument
  # in place — see _validate_cron_body), exactly as the update route does, so
  # the persisted file keeps the submitted prompt_file pointer: the pointed
  # file owns the prompt body and this file carries only the path to it. An
  # unreadable prompt_file or a master task without a prompt source becomes a
  # 409 with the loader's error text, and nothing is written to disk.
  try:
    await asyncio.to_thread(
        _validate_cron_body, copy.deepcopy(body), cfg.charlie_bot_repo, req.name)
  except Exception as e:
    raise HTTPException(status_code=409, detail=str(e)) from e
  cron_dir().mkdir(parents=True, exist_ok=True)
  await asyncio.to_thread(_write_cron_yaml, req.name, body)
  log.debug('cron_task_created', name=req.name)
  return {**{'name': req.name}, **body}


@router.delete('/tasks/{name}')
async def delete_cron_task(name: str):
  """Remove a job by unlinking its config.d/cron.d/<name>.yaml file."""
  if not _CRON_NAME_RE.fullmatch(name):
    raise HTTPException(status_code=400, detail=f'invalid cron name: {name!r}')
  path = cron_path(name)
  if not path.exists():
    raise HTTPException(status_code=404, detail=f'Task "{name}" not found')
  path.unlink()
  log.debug('cron_task_deleted', name=name)
  return {'ok': True}
