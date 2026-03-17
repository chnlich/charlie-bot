"""Abstract base class for agent subprocess backends with template-method run()."""

import asyncio
import json
import os
import shutil
import signal
import time
from abc import ABC, abstractmethod
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Awaitable, Callable, Optional

import structlog

log = structlog.get_logger()

DEFAULT_BUFFER_LIMIT = 1024 * 1024 * 1024  # 1 GB


def resolve_binary(name: str, fallback_dir: str) -> str:
  """Resolve a CLI binary by name, falling back to a directory path.

  Checks PATH via shutil.which first, then tries fallback_dir/name.
  Raises FileNotFoundError with a consistent message if neither exists.
  """
  path = shutil.which(name)
  if path:
    return path
  fallback = Path(fallback_dir) / name
  if fallback.exists():
    return str(fallback)
  raise FileNotFoundError(f"{name} binary not found on PATH or at {fallback}")


def make_text_event(text: str) -> dict:
  """Build a CC-compatible assistant-text event."""
  return {"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}}


def make_error_event(msg: str) -> dict:
  """Build a CC-compatible error event."""
  return {"type": "error", "message": msg, "content": msg}


def make_result_event(
    input_tokens: int = 0,
    output_tokens: int = 0,
    cache_read: int = 0,
    cache_creation: int = 0,
    cost: float = 0,
) -> dict:
  """Build a CC-compatible result/usage event."""
  return {
      "type": "result",
      "result": "",
      "usage":
          {
              "input_tokens": input_tokens,
              "output_tokens": output_tokens,
              "cache_read_input_tokens": cache_read,
              "cache_creation_input_tokens": cache_creation,
          },
      "total_cost_usd": cost,
  }


def make_tool_use_event(name: str, input_data: dict) -> dict:
  """Build a CC-compatible tool_use event (flat format)."""
  return {"type": "tool_use", "name": name, "input": input_data}


def make_tool_result_event(tool_name: str, content: str) -> dict:
  """Build a CC-compatible tool_result event keyed by tool_name."""
  return {"type": "tool_result", "tool_name": tool_name, "content": content}


class AgentBackend(ABC):
  """Abstract interface for running a Claude agent subprocess.

  Subclasses encapsulate subprocess lifecycle management and NDJSON streaming
  so callers can consume a uniform stream of event dicts regardless of the
  underlying execution mechanism.

  Template-method pattern: subclasses override ``_build_command()`` (required),
  and optionally ``_prepare_env()`` and ``translate_event()``.

  Event schema contract:
    All events yielded by run() must be JSON-serializable dicts with at
    minimum a "type" key. A "timestamp" (UTC ISO-8601) is recommended but
    optional — the persistence layer injects one if missing.
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

  def _effective_prompt(self, prompt: str) -> str:
    """Return prompt with instructions prepended, if any are configured."""
    if self._instructions_content:
      return f"<system-instructions>\n{self._instructions_content}\n</system-instructions>\n\n{prompt}"
    return prompt

  @abstractmethod
  def _build_command(self, prompt: str) -> list[str]:
    """Build the full CLI command list for the subprocess.

    This is the ONLY abstract method. Subclasses must implement it.
    """
    ...

  def _prepare_cwd(self, cwd: str) -> None:
    """Hook to prepare the working directory before subprocess spawn. No-op default."""

  def _prepare_env(self, env: dict) -> dict:
    """Hook to modify the environment before subprocess spawn. Identity default."""
    return env

  def translate_event(self, event: dict) -> list[dict]:
    """Translate a raw backend NDJSON event into CC-compatible event dict(s).

    Each returned event dict MUST contain a "type" field. Standard types:
      - "assistant": message content (requires "message.content" list)
      - "system": system-level info
      - "result": final usage/cost summary
      - "error": error details

    Events MAY include a "timestamp" field (UTC ISO-8601 string). If omitted,
    the persistence layer (save_chat_event) injects one automatically.

    Identity default: returns [event] unchanged.
    """
    return [event]

  async def run(self, prompt: str, cwd: str, env: dict) -> AsyncIterator[dict]:
    """Spawn the agent subprocess and yield parsed NDJSON event dicts.

    Template method: calls _build_command() -> _prepare_env() -> subprocess
    spawn -> NDJSON read loop -> translate_event() -> drain stderr.

    After the generator is fully consumed, ``exit_code`` and ``stderr_text``
    are populated.
    """
    await asyncio.to_thread(self._prepare_cwd, cwd)
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
        start_new_session=True,
    )
    if self._on_spawn is not None:
      await self._on_spawn(self._proc.pid)

    assert self._proc.stdout is not None
    _saw_result = False
    _result_time: float = 0.0
    _POST_RESULT_TIMEOUT = 30.0
    _CLEANUP_TIMEOUT = 5.0
    _pending_tool_calls: set[str] = set()

    while True:
      if _saw_result:
        remaining = _POST_RESULT_TIMEOUT - (time.monotonic() - _result_time)
        if remaining <= 0:
          log.warning("backend_post_result_timeout", pid=self._proc.pid, timeout=_POST_RESULT_TIMEOUT)
          break
        try:
          raw_line = await asyncio.wait_for(self._proc.stdout.readline(), timeout=remaining)
        except asyncio.TimeoutError:
          log.warning("backend_post_result_timeout", pid=self._proc.pid, timeout=_POST_RESULT_TIMEOUT)
          break
      else:
        raw_line = await self._proc.stdout.readline()

      if not raw_line:
        break

      line = raw_line.decode("utf-8", errors="replace").strip()
      if not line:
        continue
      try:
        event = json.loads(line)
      except json.JSONDecodeError as e:
        log.debug("backend_line_not_json", error=str(e))
        continue

      for translated in self.translate_event(event):
        evt_type = translated.get("type")

        # Track pending tool calls — wrapped format (OpenCode/GLM-5):
        # {type: 'assistant', message: {content: [{type: 'tool_use', id: '...'}]}}
        if evt_type == "assistant":
          for item in translated.get("message", {}).get("content", []):
            if isinstance(item, dict) and item.get("type") == "tool_use":
              tool_id = item.get("id", "")
              if tool_id:
                _pending_tool_calls.add(tool_id)

        # Track pending tool calls — flat format (Codex/Gemini):
        # {type: 'tool_use', id: '...', name: '...'}
        if evt_type == "tool_use":
          tool_id = translated.get("id", "")
          if tool_id:
            _pending_tool_calls.add(tool_id)

        # Clear pending tool calls on tool_result events.
        if evt_type == "tool_result":
          tool_use_id = translated.get("tool_use_id", "")
          if tool_use_id:
            _pending_tool_calls.discard(tool_use_id)

        yield translated

        if not _saw_result and evt_type == "result":
          if _pending_tool_calls:
            log.debug(
                "backend_result_suppressed_pending_tools",
                pending=len(_pending_tool_calls),
                tool_ids=list(_pending_tool_calls),
            )
          else:
            _saw_result = True
            _result_time = time.monotonic()

    # Drain stderr and wait for process exit with a timeout.
    assert self._proc.stderr is not None
    try:
      stderr_bytes = await asyncio.wait_for(self._proc.stderr.read(), timeout=_CLEANUP_TIMEOUT)
    except asyncio.TimeoutError:
      stderr_bytes = b""
    try:
      await asyncio.wait_for(self._proc.wait(), timeout=_CLEANUP_TIMEOUT)
    except asyncio.TimeoutError:
      log.warning("backend_wait_timeout_after_result", pid=self._proc.pid)
      try:
        os.killpg(os.getpgid(self._proc.pid), signal.SIGTERM)
      except (ProcessLookupError, PermissionError):
        pass
      try:
        await asyncio.wait_for(self._proc.wait(), timeout=_CLEANUP_TIMEOUT)
      except asyncio.TimeoutError:
        log.warning("backend_sigkill_after_result", pid=self._proc.pid)
        try:
          os.killpg(os.getpgid(self._proc.pid), signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
          pass
    self.exit_code = self._proc.returncode or 0
    self.stderr_text = stderr_bytes.decode("utf-8", errors="replace").strip() if stderr_bytes else ""

  async def terminate(self) -> None:
    """Send SIGTERM to process group; escalate to SIGKILL if not exited within 5 s."""
    if self._proc is None or self._proc.returncode is not None:
      return
    try:
      os.killpg(os.getpgid(self._proc.pid), signal.SIGTERM)
    except (ProcessLookupError, PermissionError):
      log.debug("backend_terminate_pid_gone", pid=self._proc.pid)
      return
    try:
      await asyncio.wait_for(self._proc.wait(), timeout=5.0)
    except asyncio.TimeoutError:
      log.warning("backend_terminate_timeout", pid=self._proc.pid)
      try:
        os.killpg(os.getpgid(self._proc.pid), signal.SIGKILL)
      except (ProcessLookupError, PermissionError):
        pass
