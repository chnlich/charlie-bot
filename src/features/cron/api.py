"""CRUD API for scheduled cron task configs (config.d/cron.d/<name>.yaml)."""

import asyncio
import copy
import uuid

from fastapi import APIRouter, Depends, HTTPException
from pydantic import BaseModel, ConfigDict
from starlette.requests import Request
from starlette.responses import JSONResponse, Response

from src.features.cron import event_types as ET
from src.features.cron.config import ScheduledTaskConfig, ScheduledTaskFields
from src.features.cron.loader import (
    _load_cron_file,
    _valid_cron_name,
    _validate_cron_body,
    cron_dir,
    cron_path,
    get_scheduled_task_errors,
    get_scheduled_tasks,
)
from src.features.cron.scheduler import effective_scheduled_task_backend
from src.infra.compression import gzip_level1
from src.infra.config import CharlieBotConfig, require_backend_option
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import SessionMetadata
from src.infra.responses import GZIP_RESPONSE_HEADERS, PreencodedJSONResponse, fast_json_bytes, request_wants_gzip
from src.infra.yaml_utils import load_yaml, save_yaml
from src.runtime.api.deps import bad_request, get_config_on_loop, get_session_manager
from src.runtime.scheduled_sessions import ScheduledSessionBusyError
from src.runtime.sessions import SessionManager
from src.runtime.thinking_state import busy_since

log = LazyStructlogLogger()
router = APIRouter()

# Wire sentence of the cron editor's 404 for a missing task file. Both raisers
# (apply_task_yaml_update's pre-flight, which the cron PUT route runs, and
# delete_cron_task) must carry the same bytes: tests/test_cron_delete.py pins
# each one.
_TASK_NOT_FOUND_DETAIL = 'Task "{}" not found'


def _read_cron_yaml(name: str) -> dict:
  return load_yaml(cron_path(name), default={})


def _write_cron_yaml(name: str, data: dict) -> None:
  save_yaml(cron_path(name), data)


def _validate_cron_name(name: str) -> None:
  """Raise the routes' 400 unless *name* passes :func:`src.features.cron.loader._valid_cron_name`, the rule's one home."""
  if not _valid_cron_name(name):
    raise HTTPException(status_code=400, detail=f'invalid cron name: {name!r}')


def _validate_backend_id(backend: str | None, cfg: CharlieBotConfig) -> None:
  if not backend:
    return
  try:
    require_backend_option(cfg, backend, subject="")
  except ValueError as e:
    raise bad_request(e) from e


def _apply_task_update(task: dict, req: TaskUpdate) -> dict:
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
  if 'session_id' in req.model_fields_set:
    if req.session_id:
      updated['session_id'] = req.session_id
    else:
      updated.pop('session_id', None)
  return updated


async def _ensure_backend_update_session(
    name: str,
    cand_model: ScheduledTaskConfig,
    req: TaskUpdate,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
) -> SessionMetadata | None:
  """The backend change's session effect: the bound node switches in place.

  A bound task's node follows the task config: the editor switches the node's
  backend in place — never by creating a session — and keeps the
  409-before-write contract when the switch cannot happen now (the node's own
  work is in flight). An unbound task has no session to switch: the write
  alone lands, and the next tick's auto-bind creates the node on the new
  backend.
  """
  if 'backend' not in req.model_fields_set:
    return None
  backend = effective_scheduled_task_backend(cand_model, cfg)
  if not cand_model.session_id:
    return None
  node = await session_mgr.get_session(cand_model.session_id)
  if node is None or node.backend == backend:
    return None
  if await _scheduled_node_busy(node):
    raise ScheduledSessionBusyError(
        f"scheduled task '{name}' backend switch from '{node.backend}' to '{backend}' is blocked "
        f"because node '{node.id}' has running work; retry when it is idle")
  return await session_mgr.switch_backend(node.id, backend)


async def _restore_enabled_task_node(name: str, req: TaskUpdate, cand_model: ScheduledTaskConfig) -> None:
  """The enable's session effect on a bound archived task node: the node restores.

  Enabling a task re-arms its node, and an archived node fires nothing, so the
  editor's enable restores the bound node's archived chain (the named source
  is the cron enable itself). A disabled or unbound task restores nothing; an
  open node restores nothing (the chain walk finds no archived member).
  """
  if req.enabled is not True or not cand_model.session_id:
    return
  from src.runtime.api.deps import task_manager

  tree = task_manager()
  meta = await tree.load_meta(cand_model.session_id)
  if meta is None:
    return
  if tree.task_state(meta.id) == "open":
    return
  restored = await tree.completion.restore_chain(meta.id, request_id=str(uuid.uuid4()), reason="cron enable")
  log.info("cron_enable_restored_node", task=name, session=meta.id, restored=restored)


async def _scheduled_node_busy(node: SessionMetadata) -> bool:
  """Whether the bound node's own work is in flight.

  The rotation busy check's successor: the node's busy interval (the master
  queue's or a worker Run's, per thinking_state). An in-flight round resolved its backend at launch, so the
  editor's switch waits for it instead of splitting the round's identity.
  """
  return bool(busy_since(node.id))


class TaskUpdate(BaseModel):
  cron: str | None = None
  prompt_file: str | None = None
  repo: str | None = None
  backend: str | None = None
  timezone: str | None = None
  enabled: bool | None = None
  project: str | None = None
  allow_failure: bool | None = None
  session_id: str | None = None


class TaskCreate(ScheduledTaskFields):
  """Create-request body for POST /tasks: the shared task field block, all of it editable."""

  # The loader's task model rejects unknown keys (extra='forbid'); this body keeps
  # its own looser contract of ignoring them, so the pydantic default is pinned.
  model_config = ConfigDict(extra='ignore')


@router.get('/tasks')
async def list_cron_tasks(request: Request) -> Response:
  """Return all scheduled tasks plus one error entry per broken file, never 500.

  Valid jobs are sorted by name, followed by one entry per error record shaped
  ``{"name", "error", "broken": True, "path": str, "enabled": bool | None}`` —
  ``path`` is the failing file's absolute path and ``enabled`` its raw value
  (None when the body could not be parsed). A broken or legacy file must never
  cause this route to fail.
  """
  # prompt is resolved from prompt_file for the in-process scheduler/master
  # reads; no consumer of this route reads it (the UI edits prompt_file), and
  # shipping the resolved bodies was ~90 KB of the 96 KB response. The steps
  # exclusion is the same field one level down on a chain task. The dump feeds
  # the response render (orjson) directly with no encoder pass left to convert
  # types, so it must hand the render plain JSON types: mode="json" is that
  # guarantee should a datetime or enum field join the model (today every field
  # is already a primitive, so the bytes equal the encoder-rendered output).
  # Returning the mapped list instead would pay jsonable_encoder's dict
  # recursion per request for the same bytes.
  body, gz = _cron_tasks_body()
  if not request_wants_gzip(request):
    return PreencodedJSONResponse(body)
  return PreencodedJSONResponse(gz, headers=GZIP_RESPONSE_HEADERS)


# The sidebar's Workspace view fetches this list on every view load, and the
# browser's fetch always accepts gzip. The rendered bytes and
# their level-1 gzip form cache on the snapshot's own generation: the identity
# of the tasks list get_scheduled_tasks returns — stable between config
# changes, rebuilt by any reload, and pinned by the cache's own reference so a
# freed list's address can never be reused for a new one. One config change
# re-renders and re-compresses once; every poll in between serves both bodies
# with zero render and zero deflate, and Content-Encoding set upstream makes
# the middleware skip its own pass (the M72 mechanism).
_CRON_TASKS_BODY_CACHE: tuple[list, bytes, bytes] | None = None


def _cron_tasks_body() -> tuple[bytes, bytes]:
  """Return (plain body, gzip body) for the current cron snapshot, rendering once per generation."""
  global _CRON_TASKS_BODY_CACHE
  tasks = get_scheduled_tasks()
  cache = _CRON_TASKS_BODY_CACHE
  if cache is not None and cache[0] is tasks:
    return cache[1], cache[2]
  valid = [t.model_dump(mode="json", exclude={'prompt': True, 'steps': {'__all__': {'prompt': True}}}) for t in tasks]
  broken = [
      {
          'name': e.name,
          'error': e.error,
          'broken': True,
          'path': e.path,
          'enabled': e.enabled
      } for e in get_scheduled_task_errors()
  ]
  body = fast_json_bytes(valid + broken)
  gz = gzip_level1(body)
  _CRON_TASKS_BODY_CACHE = (tasks, body, gz)
  return body, gz


async def apply_task_yaml_update(
    name: str,
    req: TaskUpdate,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
) -> tuple[dict, SessionMetadata | None]:
  """Apply a ``TaskUpdate`` to one job's yaml: load, validate, rotate, write.

  The cron PUT route's implementation. Returns
  the candidate task body (the PUT response) and the rotated/ensured
  ``SessionMetadata`` (None when the request carries no backend change or the
  session already matches). Raises HTTPException with the cron editor's exact
  contract: 404 when the task file is missing, 400 on a non-recognized
  backend, and 409 when the loader fails or the dedicated session is busy
  (busy 409 surfaces before any yaml write).
  """
  path = cron_path(name)
  if not path.exists():
    raise HTTPException(status_code=404, detail=_TASK_NOT_FOUND_DETAIL.format(name))
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
    cand_model, _ = await asyncio.to_thread(_validate_cron_body, copy.deepcopy(candidate), cfg.charlie_bot_repo, name)
  except Exception as e:
    raise HTTPException(status_code=409, detail=str(e)) from e

  rotated: SessionMetadata | None = None
  try:
    rotated = await _ensure_backend_update_session(name, cand_model, req, cfg, session_mgr)
  except ScheduledSessionBusyError as e:
    # The 409 lands before any yaml write: the file keeps its current backend
    # when the node's switch cannot happen now.
    raise HTTPException(status_code=409, detail=str(e)) from e
  await _restore_enabled_task_node(name, req, cand_model)
  await asyncio.to_thread(_write_cron_yaml, name, candidate)
  log.debug('cron_task_updated', name=name)
  return candidate, rotated


@router.put('/tasks/{name}')
async def update_cron_task(
    name: str,
    req: TaskUpdate,
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    session_mgr: SessionManager = Depends(get_session_manager),
) -> dict:
  _validate_cron_name(name)
  candidate, _ = await apply_task_yaml_update(name, req, cfg, session_mgr)
  return candidate


@router.post('/tasks')
async def create_cron_task(req: TaskCreate, cfg: CharlieBotConfig = Depends(get_config_on_loop)) -> dict:
  """Add a new scheduled job as its own config.d/cron.d/<name>.yaml file."""
  _validate_cron_name(req.name)
  _validate_backend_id(req.backend, cfg)
  path = cron_path(req.name)
  if path.exists():
    raise HTTPException(status_code=409, detail=f'Task "{req.name}" already exists')
  payload = req.model_dump()
  if not payload.get('backend'):
    payload.pop('backend', None)
  body = {k: v for k, v in payload.items() if k != 'name' and (v is not None or k in ('cron', 'enabled'))}
  # Validate the assembled body through the exact same body-processing code
  # the production loader uses, on a deep copy (that code mutates its argument
  # in place — see _validate_cron_body), exactly as the update route does, so
  # the persisted file keeps the submitted prompt_file pointer: the pointed
  # file owns the prompt body and this file carries only the path to it. An
  # unreadable prompt_file or a task whose sources or mode fail the loader's
  # validation becomes a 409 with the loader's error text, and nothing is
  # written to disk.
  try:
    await asyncio.to_thread(_validate_cron_body, copy.deepcopy(body), cfg.charlie_bot_repo, req.name)
  except Exception as e:
    raise HTTPException(status_code=409, detail=str(e)) from e
  cron_dir().mkdir(parents=True, exist_ok=True)
  await asyncio.to_thread(_write_cron_yaml, req.name, body)
  log.debug('cron_task_created', name=req.name)
  return {'name': req.name, **body}


@router.post('/tasks/{name}/run')
async def run_cron_task_now(name: str, request: Request) -> JSONResponse:
  """Run a scheduled task once now, through the same path a cron fire takes.

  The call lands on ``scheduler.run_task_now`` and writes no chat event of its
  own: a handler task leaves only its handler result (never a pending input),
  and an agent task delivers its completion report to its node like a
  scheduled fire does. The response type is the one the manual-run trigger has
  always carried.
  """
  scheduler = getattr(request.app.state, 'scheduler', None)
  if scheduler is None:
    raise HTTPException(status_code=503, detail='Scheduler not available')
  try:
    result = await scheduler.run_task_now(name)
  except ValueError as e:
    raise HTTPException(status_code=404, detail=str(e)) from e
  return JSONResponse(
      status_code=202,
      content={
          'type': ET.TASK_TRIGGERED,
          'task': name,
          'session_id': result['session_id'],
          'thread_id': result.get('thread_id'),
      },
  )


@router.delete('/tasks/{name}')
async def delete_cron_task(name: str) -> dict:
  """Remove a job by unlinking its config.d/cron.d/<name>.yaml; it archives nothing.

  The task's bound node is the user's task-tree node, not the deletion's
  product: it stays exactly as it is. (A stale active legacy cron session, if
  one ever re-activates, is archived by the next tick's sweep.)
  """
  _validate_cron_name(name)
  path = cron_path(name)
  if not path.exists():
    raise HTTPException(status_code=404, detail=_TASK_NOT_FOUND_DETAIL.format(name))
  path.unlink()
  log.debug('cron_task_deleted', name=name)
  return {'ok': True}
