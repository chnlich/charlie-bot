"""Slash command loading and execution."""

import asyncio
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

import structlog
import yaml
from pydantic import BaseModel

log = structlog.get_logger()

_SLASH_COMMANDS_FILE = Path.home() / '.charliebot' / 'slash_commands.yaml'


class SlashCommand(BaseModel):
  name: str
  scope: str  # 'shell' or 'prompt'
  description: str
  command: Optional[str] = None
  prompt: Optional[str] = None
  timeout: int = 10
  args: Optional[str] = None  # Description string for help text
  cwd: Optional[str] = None
  claude_code_flags: list[str] = []  # Extra CLI flags passed to Claude Code subprocess (scope=prompt only)


def load_slash_commands() -> list[SlashCommand]:
  """Read ~/.charliebot/slash_commands.yaml fresh on every call. Returns empty list if missing."""
  if not _SLASH_COMMANDS_FILE.exists():
    return []
  try:
    raw = _SLASH_COMMANDS_FILE.read_text(encoding='utf-8')
    data = yaml.safe_load(raw) or {}
  except (OSError, yaml.YAMLError) as e:
    log.warning('slash_commands_load_failed', path=str(_SLASH_COMMANDS_FILE), error=str(e))
    return []

  commands_raw = data.get('commands') or {}
  result: list[SlashCommand] = []
  for name, cfg in commands_raw.items():
    if not isinstance(cfg, dict):
      log.warning('slash_command_invalid_entry', name=name)
      continue
    try:
      result.append(SlashCommand(name=name, **cfg))
    except Exception as e:
      log.warning('slash_command_parse_failed', name=name, error=str(e))
  return result


async def execute_shell_command(
    cmd_template: str,
    args: str = '',
    session_dir: str = '',
    timeout: int = 10,
    cwd: Optional[str] = None,
) -> dict:
  """Run a shell command template and return {stdout, stderr, exit_code}.

  Template variables: {args}, {session_dir}.
  On timeout, kills the process and returns stderr='Command timed out'.
  """
  cmd = cmd_template.replace('{args}', args).replace('{session_dir}', session_dir)
  log.debug('slash_shell_exec', cmd=cmd, cwd=cwd, timeout=timeout)

  try:
    proc = await asyncio.create_subprocess_shell(
        cmd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        cwd=cwd or None,
    )
  except Exception as e:
    log.warning('slash_shell_spawn_failed', cmd=cmd, error=str(e))
    return {'stdout': '', 'stderr': str(e), 'exit_code': -1}

  try:
    stdout_bytes, stderr_bytes = await asyncio.wait_for(proc.communicate(), timeout=timeout)
  except asyncio.TimeoutError:
    log.warning('slash_shell_timeout', cmd=cmd, timeout=timeout)
    try:
      proc.kill()
    except Exception as kill_err:
      log.debug('slash_shell_kill_failed', error=str(kill_err))
    return {'stdout': '', 'stderr': 'Command timed out', 'exit_code': -1}

  return {
      'stdout': stdout_bytes.decode('utf-8', errors='replace'),
      'stderr': stderr_bytes.decode('utf-8', errors='replace'),
      'exit_code': proc.returncode,
  }


@dataclass
class SlashDispatchResult:
  """Result of dispatching a slash command."""
  kind: str  # 'not_found', 'shell_result', 'prompt', 'error'
  command: Optional[SlashCommand] = None
  shell_result: Optional[dict] = None  # {stdout, stderr, exit_code}
  substituted_prompt: Optional[str] = None
  claude_code_flags: Optional[list[str]] = None
  error: Optional[str] = None


async def dispatch_slash_command(
    name: str,
    args: str,
    session_dir: str = '',
) -> SlashDispatchResult:
  """Look up a YAML slash command by name and execute/prepare it.

  For shell commands: runs the command and returns the result.
  For prompt commands: substitutes args and returns the prompt text.
  Returns kind='not_found' if the command doesn't exist in the registry.
  """
  commands = {c.name: c for c in await asyncio.to_thread(load_slash_commands)}
  cmd = commands.get(name)
  if cmd is None:
    return SlashDispatchResult(kind='not_found')

  if cmd.scope == 'shell':
    if not cmd.command:
      log.warning('slash_shell_missing_command_template', name=name)
      return SlashDispatchResult(kind='error', command=cmd, error=f'Command /{name} has no command template configured')
    result = await execute_shell_command(
        cmd_template=cmd.command, args=args, session_dir=session_dir, timeout=cmd.timeout, cwd=cmd.cwd)
    return SlashDispatchResult(kind='shell_result', command=cmd, shell_result=result)

  if cmd.scope == 'prompt':
    if not cmd.prompt:
      log.warning('slash_prompt_missing_template', name=name)
      return SlashDispatchResult(kind='error', command=cmd, error=f'Command /{name} has no prompt template configured')
    substituted = cmd.prompt.replace('{args}', args)
    return SlashDispatchResult(
        kind='prompt', command=cmd, substituted_prompt=substituted, claude_code_flags=cmd.claude_code_flags or None)

  log.warning('slash_unknown_scope', name=name, scope=cmd.scope)
  return SlashDispatchResult(kind='error', command=cmd, error=f'Unknown scope for command /{name}: {cmd.scope}')
