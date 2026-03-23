"""Slash command API routes."""

import asyncio

import structlog
from fastapi import APIRouter, Depends, Request
from fastapi.responses import JSONResponse
from pydantic import BaseModel

from src.api.chat import run_and_finalize
from src.api.deps import get_session_manager, get_thread_manager, require_session
from src.core.config import CharlieBotConfig, get_config, get_scheduled_tasks
from src.core.models import SessionMetadata
from src.core.sessions import SessionManager
from src.core.slash_commands import dispatch_slash_command, load_slash_commands
from src.core.tasks import create_logged_task
from src.core.threads import ThreadManager

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
    'name': 'run',
    'scope': 'builtin',
    'description': 'Manually trigger a scheduled task',
    'args': '<task-name>',
    'params': [
        {'name': 'task_name', 'label': 'Task name', 'type': 'text', 'required': True, 'placeholder': 'e.g. daily-report'},
    ],
}

_IMPROVE_ENTRY = {
    'name': 'improve',
    'scope': 'builtin',
    'description': 'Run iterative improvement loop',
    'args': '[max_iterations] <goal>',
    'params': [
        {'name': 'max_iterations', 'label': 'Max iterations', 'type': 'number', 'default': '5',
         'placeholder': 'Default: 5'},
        {'name': 'goal', 'label': 'Goal', 'type': 'text', 'required': True, 'placeholder': 'What to improve...'},
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
  result = [{
      'name': c.name, 'scope': c.scope, 'description': c.description, 'args': c.args,
      'params': [p.model_dump(exclude_defaults=True) for p in c.params],
  } for c in cmds]
  result.append(_HELP_ENTRY)
  result.append(_RUN_ENTRY)
  result.append(_IMPROVE_ENTRY)
  result.append(_STOP_IMPROVE_ENTRY)
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
    thread_mgr: ThreadManager = Depends(get_thread_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Execute a slash command for a session."""
  name = req.command.lstrip('/')

  # Built-in /help
  if name == 'help':
    return {'type': 'help', 'commands': await _build_command_list()}

  # Built-in /improve
  if name == 'improve':
    args_text = req.args.strip()
    if not args_text:
      return {'error': 'Usage: /improve [max_iterations] <goal>'}
    parts = args_text.split(None, 1)
    max_iterations = 5
    goal = args_text
    if parts[0].isdigit():
      max_iterations = int(parts[0])
      goal = parts[1] if len(parts) > 1 else ''
    if not goal:
      return {'error': 'Usage: /improve [max_iterations] <goal>'}
    from src.core.improve_command import ImproveState, build_improve_master_prompt, save_improve_state

    state = ImproveState(goal=goal, max_iterations=max_iterations, status='running')
    save_improve_state(session_id, state, cfg)

    prompt = build_improve_master_prompt(session_id, goal, max_iterations, cfg)
    create_logged_task(run_and_finalize(cfg, meta, prompt, session_mgr))
    return JSONResponse(
        status_code=202,
        content={
            'type': 'improve_started',
            'goal': goal,
            'max_iterations': max_iterations,
        },
    )

  # Built-in /stop-improve
  if name == 'stop-improve':
    from src.core.improve_command import stop_improve_loop
    stopped = await stop_improve_loop(session_id, cfg)
    if stopped:
      return {'type': 'improve_stopped', 'message': 'Improve loop will stop after current iteration'}
    return {'error': 'No active improve loop in this session'}

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
    create_logged_task(
        run_and_finalize(
            cfg, meta, dispatch.substituted_prompt, session_mgr, extra_claude_flags=dispatch.claude_code_flags))
    return JSONResponse(status_code=202, content={'type': 'prompt_dispatched', 'command': name})

  return {'error': f'Unexpected dispatch result for /{name}'}
