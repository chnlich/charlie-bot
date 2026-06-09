"""Abstract base class for agent subprocess backends with template-method run()."""

import asyncio
import contextlib
import json
import os
import shutil
import signal
import time
from abc import ABC, abstractmethod
from collections.abc import AsyncIterator
from datetime import datetime, timezone
from pathlib import Path
from typing import Awaitable, Callable, Optional

import aiofiles
import structlog

from src.core import event_types as ET
from src.core.process import kill_process_group

log = structlog.get_logger()

DEFAULT_BUFFER_LIMIT = 1024 * 1024 * 1024  # 1 GB
_STDERR_TAIL_BYTES = 64 * 1024


async def _capture_proc_diagnostics(pid: int) -> dict:
  """Best-effort snapshot of a hung subprocess: pgid, ps tree, /proc state, fds, children."""
  out: dict = {"captured_at": datetime.now(timezone.utc).isoformat(), "pid": pid}
  try:
    out["pgid"] = os.getpgid(pid)
  except Exception as e:
    out["pgid"] = f"<error: {e}>"
  pgid_for_cmd = out["pgid"] if isinstance(out["pgid"], int) else pid
  for key, cmd in [
      ("process_tree", ["bash", "-c", f"ps --forest -o pid,pgid,stat,etime,wchan,cmd -g {pgid_for_cmd} 2>&1"]),
      ("fds", ["bash", "-c", f"ls -l /proc/{pid}/fd 2>&1 | head -200"]),
      ("status", ["bash", "-c", f"cat /proc/{pid}/status 2>&1"]),
      ("children", ["bash", "-c", f"ps -o pid,stat,etime,cmd --ppid {pid} 2>&1"]),
  ]:
    try:
      r = await asyncio.create_subprocess_exec(*cmd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT)
      data, _ = await asyncio.wait_for(r.communicate(), timeout=2.0)
      out[key] = data.decode("utf-8", errors="replace")
    except Exception as e:
      out[key] = f"<capture failed: {e}>"
  return out


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
  return {"type": ET.ASSISTANT, "message": {"content": [{"type": "text", "text": text}]}}


def make_error_event(msg: str) -> dict:
  """Build a CC-compatible error event."""
  return {"type": ET.ERROR, "message": msg, "content": msg}


def make_result_event(
    input_tokens: int = 0,
    output_tokens: int = 0,
    cache_read: int = 0,
    cache_creation: int = 0,
    cost: float | None = 0,
) -> dict:
  """Build a CC-compatible result/usage event."""
  return {
      "type": ET.RESULT,
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
  return {"type": ET.TOOL_USE, "name": name, "input": input_data}


def make_tool_result_event(tool_name: str, content: str) -> dict:
  """Build a CC-compatible tool_result event keyed by tool_name."""
  return {"type": ET.TOOL_RESULT, "tool_name": tool_name, "content": content}


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

  # Tunable lifecycle timeouts (class attrs so tests can monkey-patch).
  _POST_RESULT_TIMEOUT: float = 90.0
  _CLEANUP_TIMEOUT: float = 30.0

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
      log_dir: Optional[Path] = None,
      **_extra,
  ):
    self._model = model
    self._extra_flags = extra_flags or []
    self._buffer_limit = buffer_limit or DEFAULT_BUFFER_LIMIT
    self._on_spawn = on_spawn
    self._system_prompt_path = system_prompt_path
    self._instructions_content = instructions_content
    self._resume_session_id = resume_session_id
    self._log_dir = log_dir
    self._proc: Optional[asyncio.subprocess.Process] = None
    self._stderr_task: Optional[asyncio.Task] = None
    self._stderr_tail = bytearray()
    self.exit_code: int = -1
    self.stderr_text: str = ""
    # Set when terminate() is called — deliberate user stop or shutdown.
    self.terminated: bool = False
    self.hang_diagnostics: Optional[dict] = None

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
    _POST_RESULT_TIMEOUT = self._POST_RESULT_TIMEOUT
    _CLEANUP_TIMEOUT = self._CLEANUP_TIMEOUT
    _pending_tool_calls: set[str] = set()

    if self._log_dir is not None:
      self._log_dir.mkdir(parents=True, exist_ok=True)
      stdout_log_cm = aiofiles.open(self._log_dir / "stdout.log", "wb")
      stderr_log_path: Optional[Path] = self._log_dir / "stderr.log"
    else:
      stdout_log_cm = contextlib.nullcontext(None)
      stderr_log_path = None

    self._stderr_task = asyncio.create_task(self._stream_stderr(stderr_log_path))

    async with stdout_log_cm as stdout_log:
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

        if stdout_log is not None:
          await stdout_log.write(raw_line)
          await stdout_log.flush()

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
          if evt_type == ET.ASSISTANT:
            for item in translated.get("message", {}).get("content", []):
              if isinstance(item, dict) and item.get("type") == ET.TOOL_USE:
                tool_id = item.get("id", "")
                if tool_id:
                  _pending_tool_calls.add(tool_id)

          # Track pending tool calls — flat format (Codex/Gemini):
          # {type: 'tool_use', id: '...', name: '...'}
          if evt_type == ET.TOOL_USE:
            tool_id = translated.get("id", "")
            if tool_id:
              _pending_tool_calls.add(tool_id)

          # Clear pending tool calls on tool_result events — flat format.
          if evt_type == ET.TOOL_RESULT:
            tool_use_id = translated.get("tool_use_id", "")
            if tool_use_id:
              _pending_tool_calls.discard(tool_use_id)

          # Clear pending tool calls — wrapped format (Claude Code):
          # {type: 'user', message: {content: [{type: 'tool_result', tool_use_id: '...'}]}}
          if evt_type == ET.USER:
            for item in translated.get("message", {}).get("content", []):
              if isinstance(item, dict) and item.get("type") == ET.TOOL_RESULT:
                tool_use_id = item.get("tool_use_id", "")
                if tool_use_id:
                  _pending_tool_calls.discard(tool_use_id)

          yield translated

          if not _saw_result and evt_type == ET.RESULT:
            if _pending_tool_calls:
              log.debug(
                  "backend_result_suppressed_pending_tools",
                  pending=len(_pending_tool_calls),
                  tool_ids=list(_pending_tool_calls),
              )
            else:
              _saw_result = True
              _result_time = time.monotonic()

    await self._drain_and_cleanup(_CLEANUP_TIMEOUT)

  async def _graceful_shutdown(
      self,
      timeout: float,
      *,
      timeout_log_event: str,
      wait_even_if_sigterm_not_sent: bool = False,
  ) -> None:
    """Send SIGTERM, wait for exit, and escalate to SIGKILL if the process does not exit in time."""
    assert self._proc is not None
    sigterm_sent = kill_process_group(self._proc.pid, signal.SIGTERM)
    if not sigterm_sent and not wait_even_if_sigterm_not_sent:
      return
    try:
      await asyncio.wait_for(self._proc.wait(), timeout=timeout)
    except asyncio.TimeoutError:
      log.warning(timeout_log_event, pid=self._proc.pid)
      kill_process_group(self._proc.pid, signal.SIGKILL)

  async def _stream_stderr(self, stderr_log_path: Optional[Path]) -> None:
    """Continuously read subprocess stderr; tee to <log_dir>/stderr.log and a 64 KB tail buffer.

    Streams live so `tail -f stderr.log` works during long runs and the in-memory
    tail is always up to date for `self.stderr_text`.
    """
    assert self._proc is not None and self._proc.stderr is not None
    stderr_log_cm = (
        aiofiles.open(stderr_log_path, "wb") if stderr_log_path is not None else contextlib.nullcontext(None))
    async with stderr_log_cm as stderr_log:
      while True:
        chunk = await self._proc.stderr.read(8192)
        if not chunk:
          break
        if stderr_log is not None:
          await stderr_log.write(chunk)
          await stderr_log.flush()
        self._stderr_tail.extend(chunk)
        if len(self._stderr_tail) > _STDERR_TAIL_BYTES:
          del self._stderr_tail[:len(self._stderr_tail) - _STDERR_TAIL_BYTES]

  async def _drain_and_cleanup(self, timeout: float) -> None:
    """Wait for stderr-streamer + process exit; capture diagnostics + escalate to kill on hang."""
    assert self._proc is not None
    if self._stderr_task is not None:
      try:
        await asyncio.wait_for(asyncio.shield(self._stderr_task), timeout=timeout)
      except asyncio.TimeoutError:
        log.debug("backend_stderr_stream_timeout", pid=self._proc.pid, timeout=timeout)
    try:
      await asyncio.wait_for(self._proc.wait(), timeout=timeout)
    except asyncio.TimeoutError:
      log.warning("backend_wait_timeout_after_result", pid=self._proc.pid)
      self.hang_diagnostics = await _capture_proc_diagnostics(self._proc.pid)
      await self._graceful_shutdown(
          timeout,
          timeout_log_event="backend_sigkill_after_result",
          wait_even_if_sigterm_not_sent=True,
      )
    if self._stderr_task is not None and not self._stderr_task.done():
      self._stderr_task.cancel()
      try:
        await self._stderr_task
      except asyncio.CancelledError:
        log.debug("backend_stderr_stream_cancelled", pid=self._proc.pid)
      except Exception as e:
        log.warning("backend_stderr_stream_cancel_failed", pid=self._proc.pid, error=str(e))
    self.exit_code = self._proc.returncode or 0
    self.stderr_text = bytes(self._stderr_tail).decode("utf-8", errors="replace").strip()

  async def terminate(self) -> None:
    """Send SIGTERM to process group; escalate to SIGKILL if not exited within 5 s."""
    self.terminated = True
    if self._proc is None or self._proc.returncode is not None:
      return
    await self._graceful_shutdown(5.0, timeout_log_event="backend_terminate_timeout")
