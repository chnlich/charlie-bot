"""Abstract base class for agent subprocess backends with template-method run()."""

import asyncio
import json
from abc import ABC, abstractmethod
from collections.abc import AsyncIterator
from typing import Awaitable, Callable, Optional

import structlog

log = structlog.get_logger()

DEFAULT_BUFFER_LIMIT = 1024 * 1024 * 1024  # 1 GB


class AgentBackend(ABC):
  """Abstract interface for running a Claude agent subprocess.

  Subclasses encapsulate subprocess lifecycle management and NDJSON streaming
  so callers can consume a uniform stream of event dicts regardless of the
  underlying execution mechanism.

  Template-method pattern: subclasses override ``_build_command()`` (required),
  and optionally ``_prepare_env()`` and ``translate_event()``.
  """

  def __init__(
      self,
      *,
      model: Optional[str] = None,
      extra_flags: Optional[list[str]] = None,
      buffer_limit: Optional[int] = None,
      on_spawn: Optional[Callable[[int], Awaitable[None]]] = None,
      system_prompt_path: Optional[str] = None,
      instructions_content: Optional[str] = None,
      resume_session_id: Optional[str] = None,
      **_extra,
  ):
    self._model = model
    self._extra_flags = extra_flags or []
    self._buffer_limit = buffer_limit or DEFAULT_BUFFER_LIMIT
    self._on_spawn = on_spawn
    self._system_prompt_path = system_prompt_path
    self._instructions_content = instructions_content
    self._resume_session_id = resume_session_id
    self._proc: Optional[asyncio.subprocess.Process] = None
    self.exit_code: int = -1
    self.stderr_text: str = ""

  @property
  def pid(self) -> Optional[int]:
    """Return the PID of the running subprocess, or None if not yet started."""
    return self._proc.pid if self._proc else None

  @abstractmethod
  def _build_command(self, prompt: str) -> list[str]:
    """Build the full CLI command list for the subprocess.

    This is the ONLY abstract method. Subclasses must implement it.
    """
    ...

  def _prepare_env(self, env: dict) -> dict:
    """Hook to modify the environment before subprocess spawn. Identity default."""
    return env

  def translate_event(self, event: dict) -> list[dict]:
    """Hook to translate a raw NDJSON event into CC-compatible event(s). Identity default."""
    return [event]

  async def run(self, prompt: str, cwd: str, env: dict) -> AsyncIterator[dict]:
    """Spawn the agent subprocess and yield parsed NDJSON event dicts.

    Template method: calls _build_command() -> _prepare_env() -> subprocess
    spawn -> NDJSON read loop -> translate_event() -> drain stderr.

    After the generator is fully consumed, ``exit_code`` and ``stderr_text``
    are populated.
    """
    cmd = self._build_command(prompt)
    final_env = self._prepare_env(env)

    self._proc = await asyncio.create_subprocess_exec(
        *cmd,
        cwd=cwd,
        stdin=asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env=final_env,
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
      for translated in self.translate_event(event):
        yield translated

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
