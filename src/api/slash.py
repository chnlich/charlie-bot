"""Slash command API routes."""

import asyncio

import structlog
from fastapi import APIRouter, Depends, Request
from fastapi.responses import JSONResponse
from pydantic import BaseModel, Field

from src.api.chat import launch_prompt_dispatch
from src.api.deps import get_session_manager, require_session
from src.api.message_utils import (
    build_user_event,
    serialize_uploaded_files,
)
from src.core import event_types as ET
from src.core.config import CharlieBotConfig, get_config, get_scheduled_tasks
from src.core.models import SessionMetadata, UploadedFileRef
from src.core.sessions import SessionManager
from src.core.slash_commands import dispatch_slash_command, load_slash_commands

log = structlog.get_logger()

router = APIRouter()

# ---------------------------------------------------------------------------
# Built-in command descriptors
# ---------------------------------------------------------------------------

_HELP_ENTRY = {
    'name': 'help',
    'scope': 'builtin',
    'description': 'Show available slash commands',
    'params': [],
}

_RUN_ENTRY = {
    'name':
        'run',
    'scope':
        'builtin',
    'description':
        'Manually trigger a scheduled task',
    'args':
        '<task-name>',
    'params':
        [
            {
                'name': 'task_name',
                'label': 'Task name',
                'type': 'text',
                'required': True,
                'placeholder': 'e.g. daily-report'
            },
        ],
}

_STOP_IMPROVE_ENTRY = {
    'name': 'stop-improve',
    'scope': 'builtin',
    'description': 'Stop an active improve loop after current iteration',
    'params': [],
}


async def _build_command_list() -> list[dict]:
  """Return the full command list: YAML commands + built-ins."""
  cmds = await asyncio.to_thread(load_slash_commands)
  result = [
      {
          'name': c.name,
          'scope': c.scope,
          'description': c.description,
          'args': c.args,
          'params': [p.model_dump(exclude_defaults=True) for p in c.params],
      } for c in cmds
  ]
  result.append(_HELP_ENTRY)
  result.append(_RUN_ENTRY)
  result.append(_STOP_IMPROVE_ENTRY)
  return result


async def _handle_run_command(
    args_text: str,
    request: Request,
    session_id: str,
    session_mgr: SessionManager,
    display_text: str,
    uploaded_files: list[dict],
) -> dict | JSONResponse:
  """Execute the built-in /run scheduled-task trigger command."""
  task_name = args_text
  if not task_name:
    names = [t.name for t in get_scheduled_tasks() if t.enabled]
    if not names:
      return {'error': 'No scheduled tasks configured'}
    return {'error': f'Usage: /run <task-name>. Available: {", ".join(names)}'}
  scheduler = getattr(request.app.state, 'scheduler', None)
  if scheduler is None:
    return {'error': 'Scheduler not available'}
  try:
    result = await scheduler.run_task_now(task_name)
  except ValueError as e:
    log.debug("slash_command_value_error", error=str(e))
    return {'error': str(e)}
  await session_mgr.persist_and_broadcast(session_id, build_user_event(display_text, uploaded_files))
  return JSONResponse(
      status_code=202,
      content={
          'type': ET.TASK_TRIGGERED,
          'task': task_name,
          'session_id': result['session_id'],
          'thread_id': result['thread_id'],
      },
  )


class SlashExecuteRequest(BaseModel):
  command: str
  args: str = ''
  uploaded_files: list[UploadedFileRef] = Field(default_factory=list)


@router.get('/commands')
async def list_commands():
  """Return all available slash commands: the YAML registry plus the built-ins."""
  return await _build_command_list()


@router.post('/{session_id}/execute')
async def execute_command(
    request: Request,
    session_id: str,
    req: SlashExecuteRequest,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Execute a slash command for a session."""
  name = req.command.lstrip('/')
  args_text = req.args.strip()
  uploaded_files = serialize_uploaded_files(req.uploaded_files)
  display_text = f"/{name}" + (f" {args_text}" if args_text else "")

  # Built-in /help
  if name == 'help':
    await session_mgr.persist_and_broadcast(session_id, build_user_event(display_text, uploaded_files))
    return {'type': ET.HELP, 'commands': await _build_command_list()}

  # Built-in /stop-improve
  if name == 'stop-improve':
    from src.core.improve_command import stop_improve_loop
    stopped = await stop_improve_loop(session_id, cfg)
    if stopped:
      await session_mgr.persist_and_broadcast(session_id, build_user_event(display_text, uploaded_files))
      return {'type': ET.IMPROVE_STOPPED, 'message': 'Improve loop will stop after current iteration'}
    return {'error': 'No active improve loop in this session'}

  # Built-in /run <task-name>
  if name == 'run':
    return await _handle_run_command(args_text, request, session_id, session_mgr, display_text, uploaded_files)

  # Look up and dispatch via shared helper
  dispatch = await dispatch_slash_command(name, req.args, session_dir=str(cfg.sessions_dir / session_id))

  if dispatch.kind == 'not_found':
    return {'error': f'Unknown command: /{name}'}

  if dispatch.kind == 'error':
    return {'error': dispatch.error}

  if dispatch.kind == 'shell_result':
    result = dispatch.shell_result
    await session_mgr.persist_and_broadcast(session_id, build_user_event(display_text, uploaded_files))
    return {
        'type': ET.SHELL_RESULT,
        'command': name,
        'stdout': result['stdout'],
        'stderr': result['stderr'],
        'exit_code': result['exit_code'],
    }

  if dispatch.kind == 'prompt':
    await session_mgr.persist_and_broadcast(session_id, build_user_event(display_text, uploaded_files))
    launch_prompt_dispatch(cfg, meta, dispatch, session_mgr, display_text, uploaded_files)
    return JSONResponse(status_code=202, content={'type': ET.PROMPT_DISPATCHED, 'command': name})

  return {'error': f'Unexpected dispatch result for /{name}'}
