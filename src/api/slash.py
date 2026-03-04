"""Slash command API routes."""

import asyncio

import structlog
from fastapi import APIRouter, Depends, Request
from fastapi.responses import JSONResponse
from pydantic import BaseModel

from src.api.chat import run_and_finalize
from src.api.deps import get_session_manager, require_session
from src.core.config import CharlieBotConfig, get_config, get_scheduled_tasks
from src.core.models import SessionMetadata
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
}

_RUN_ENTRY = {
    'name': 'run',
    'scope': 'builtin',
    'description': 'Manually trigger a scheduled task',
    'args': '<task-name>',
}


async def _build_command_list() -> list[dict]:
  """Return the full command list: YAML commands + built-ins."""
  cmds = await asyncio.to_thread(load_slash_commands)
  result = [{'name': c.name, 'scope': c.scope, 'description': c.description, 'args': c.args} for c in cmds]
  result.append(_HELP_ENTRY)
  result.append(_RUN_ENTRY)
  return result


class SlashExecuteRequest(BaseModel):
  command: str
  args: str = ''


@router.get('/commands')
async def list_commands():
  """Return all available slash commands including built-in /help."""
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

  # Built-in /help
  if name == 'help':
    return {'type': 'help', 'commands': await _build_command_list()}

  # Built-in /run <task-name>
  if name == 'run':
    task_name = req.args.strip()
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
    return JSONResponse(
        status_code=202,
        content={
            'type': 'task_triggered',
            'task': task_name,
            'session_id': result['session_id'],
            'thread_id': result['thread_id'],
        },
    )

  # Look up and dispatch via shared helper
  dispatch = await dispatch_slash_command(name, req.args, session_dir=str(cfg.sessions_dir / session_id))

  if dispatch.kind == 'not_found':
    return {'error': f'Unknown command: /{name}'}

  if dispatch.kind == 'error':
    return {'error': dispatch.error}

  if dispatch.kind == 'shell_result':
    result = dispatch.shell_result
    return {
        'type': 'shell_result',
        'command': name,
        'stdout': result['stdout'],
        'stderr': result['stderr'],
        'exit_code': result['exit_code'],
    }

  if dispatch.kind == 'prompt':
    asyncio.create_task(
        run_and_finalize(
            cfg, meta, dispatch.substituted_prompt, session_mgr, extra_claude_flags=dispatch.claude_code_flags))
    return JSONResponse(status_code=202, content={'type': 'prompt_dispatched', 'command': name})

  return {'error': f'Unexpected dispatch result for /{name}'}
