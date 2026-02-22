"""Slash command API routes."""

import asyncio

import structlog
from fastapi import APIRouter, Depends
from fastapi.responses import JSONResponse
from pydantic import BaseModel

from src.api.chat import run_and_finalize
from src.api.deps import get_session_manager, require_session
from src.core.config import CharlieBotConfig, get_config
from src.core.models import SessionMetadata
from src.core.sessions import SessionManager
from src.core.slash_commands import execute_shell_command, load_slash_commands

log = structlog.get_logger()

router = APIRouter()

# ---------------------------------------------------------------------------
# Built-in /help command descriptor
# ---------------------------------------------------------------------------

_HELP_ENTRY = {
  'name': 'help',
  'scope': 'builtin',
  'description': 'Show available slash commands',
}


def _build_command_list() -> list[dict]:
  """Return the full command list: YAML commands + built-in /help."""
  cmds = load_slash_commands()
  result = [{'name': c.name, 'scope': c.scope, 'description': c.description, 'args': c.args} for c in cmds]
  result.append(_HELP_ENTRY)
  return result


class SlashExecuteRequest(BaseModel):
  command: str
  args: str = ''


@router.get('/commands')
async def list_commands():
  """Return all available slash commands including built-in /help."""
  return _build_command_list()


@router.post('/{session_id}/execute')
async def execute_command(
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
    return {'type': 'help', 'commands': _build_command_list()}

  # Look up in YAML registry
  commands = {c.name: c for c in load_slash_commands()}
  cmd = commands.get(name)
  if cmd is None:
    return {'error': f'Unknown command: /{name}'}

  if cmd.scope == 'shell':
    if not cmd.command:
      log.warning('slash_shell_missing_command_template', name=name)
      return {'error': f'Command /{name} has no command template configured'}

    session_dir = str(cfg.sessions_dir / session_id)
    result = await execute_shell_command(
      cmd_template=cmd.command,
      args=req.args,
      session_dir=session_dir,
      timeout=cmd.timeout,
      cwd=cmd.cwd,
    )
    return {
      'type': 'shell_result',
      'command': name,
      'stdout': result['stdout'],
      'stderr': result['stderr'],
      'exit_code': result['exit_code'],
    }

  if cmd.scope == 'prompt':
    if not cmd.prompt:
      log.warning('slash_prompt_missing_template', name=name)
      return {'error': f'Command /{name} has no prompt template configured'}

    substituted = cmd.prompt.replace('{args}', req.args)
    asyncio.create_task(run_and_finalize(cfg, meta, substituted, session_mgr))
    return JSONResponse(status_code=202, content={'type': 'prompt_dispatched', 'command': name})

  log.warning('slash_unknown_scope', name=name, scope=cmd.scope)
  return {'error': f'Unknown scope for command /{name}: {cmd.scope}'}
