"""ClaudeCodeBackend — concrete AgentBackend wrapping the Claude Code CLI."""

import asyncio
import json
from collections.abc import AsyncIterator
from typing import Awaitable, Callable, Optional

import structlog

from src.agents.backends.base import AgentBackend

log = structlog.get_logger()

BASE_COMMAND: list[str] = [
  "claude",
  "-p",
  "--output-format",
  "stream-json",
  "--verbose",
  "--dangerously-skip-permissions",
]

_DEFAULT_BUFFER_LIMIT = 1024 * 1024 * 1024  # 1 GB


class ClaudeCodeBackend(AgentBackend):
  """Runs a Claude Code CLI subprocess and streams NDJSON events as dicts.

  After the ``run()`` generator is fully consumed, ``exit_code`` and
  ``stderr_text`` are populated with the process exit status and any stderr
  output respectively.
  """

  def __init__(
    self,
    model: Optional[str] = None,
    system_prompt_path: Optional[str] = None,
    extra_flags: Optional[list[str]] = None,
    buffer_limit: Optional[int] = None,
    on_spawn: Optional[Callable[[int], Awaitable[None]]] = None,
  ):
    """Initialise the backend.

    Args:
      model: Optional ``--model`` flag value passed to the Claude Code CLI.
      system_prompt_path: Optional ``--system-prompt`` flag value.
      extra_flags: Additional CLI flags inserted between the base command and
        the prompt argument (e.g. ``["--resume", "<session-id>"]``).
      buffer_limit: asyncio StreamReader buffer limit in bytes.  Defaults to 1 GB.
      on_spawn: Async callable invoked with the subprocess PID immediately
        after the process is created, before any output is read.
    """
    self._cmd: list[str] = list(BASE_COMMAND)
    if model:
      self._cmd += ["--model", model]
    if system_prompt_path:
      self._cmd += ["--system-prompt", system_prompt_path]
    if extra_flags:
      self._cmd += extra_flags
    self._buffer_limit = buffer_limit or _DEFAULT_BUFFER_LIMIT
    self._on_spawn = on_spawn
    self._proc: Optional[asyncio.subprocess.Process] = None
    self.exit_code: int = -1
    self.stderr_text: str = ""

  @property
  def pid(self) -> Optional[int]:
    return self._proc.pid if self._proc else None

  async def run(self, prompt: str, cwd: str, env: dict) -> AsyncIterator[dict]:
    """Spawn the Claude Code subprocess and yield parsed NDJSON event dicts.

    Reads stdout line-by-line; non-JSON lines are skipped with a debug log.
    After the stdout stream closes, stderr is drained and the process is
    awaited; ``self.exit_code`` and ``self.stderr_text`` are then set.

    Args:
      prompt: Passed as the final argument to the Claude Code CLI.
      cwd: Working directory for the subprocess.
      env: Full environment dict for the subprocess.

    Yields:
      Parsed NDJSON event dicts from stdout.
    """
    cmd = self._cmd + [prompt]
    self._proc = await asyncio.create_subprocess_exec(
      *cmd,
      cwd=cwd,
      stdin=asyncio.subprocess.DEVNULL,
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
      env=env,
      limit=self._buffer_limit,
    )
    if self._on_spawn is not None:
      await self._on_spawn(self._proc.pid)

    assert self._proc.stdout is not None
    async for raw_line in self._proc.stdout:
      line = raw_line.decode("utf-8", errors="replace").strip()
      if not line:
        continue
      try:
        event = json.loads(line)
      except json.JSONDecodeError as e:
        log.debug("backend_line_not_json", error=str(e))
        continue
      yield event

    assert self._proc.stderr is not None
    stderr_bytes = await self._proc.stderr.read()
    await self._proc.wait()
    self.exit_code = self._proc.returncode or 0
    self.stderr_text = stderr_bytes.decode("utf-8", errors="replace").strip() if stderr_bytes else ""

  async def terminate(self) -> None:
    """Send SIGTERM; escalate to SIGKILL if process does not exit within 5 s."""
    if self._proc is None or self._proc.returncode is not None:
      return
    try:
      self._proc.terminate()
    except ProcessLookupError:
      log.debug("backend_terminate_pid_gone", pid=self._proc.pid)
      return
    try:
      await asyncio.wait_for(self._proc.wait(), timeout=5.0)
    except asyncio.TimeoutError:
      log.warning("backend_terminate_timeout", pid=self._proc.pid)
      self._proc.kill()
